use crate::buffer::{StreamRecvBuffer, WriteBuffer};
use crate::connection::ConnectionError;

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::task::{ready, Context, Poll, Waker};

use bytes::Bytes;
use libc::c_void;
use rangemap::RangeSet;
use thiserror::Error;
use tracing::trace;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StreamType {
    Bidirectional,
    Unidirectional,
}

/// A stream represents a bidirectional or unidirectional stream.
#[derive(Debug)]
pub struct Stream(Arc<StreamInstance>);

impl Stream {
    pub(crate) fn open(
        msquic_conn: &msquic::Connection,
        stream_type: StreamType,
    ) -> Result<Self, StartError> {
        let msquic_stream = msquic::Stream::new();
        let flags = if stream_type == StreamType::Unidirectional {
            msquic::STREAM_OPEN_FLAG_UNIDIRECTIONAL
        } else {
            msquic::STREAM_OPEN_FLAG_NONE
        };
        let mut inner = Arc::new(StreamInner::new(
            msquic_stream,
            stream_type,
            StreamSendState::Closed,
            StreamRecvState::Closed,
            true,
        ));
        Arc::get_mut(&mut inner)
            .unwrap()
            .shared
            .msquic_stream
            .open(
                msquic_conn,
                flags,
                Some(StreamInner::native_callback),
                std::ptr::null(),
            )
            .map_err(StartError::OtherError)?;
        unsafe {
            inner.shared.msquic_stream.set_callback_handler(
                Some(StreamInner::native_callback),
                Arc::into_raw(inner.clone()) as *const c_void,
            );
        }
        trace!("Stream({:p}) Open by local", &*inner);

        Ok(Self(Arc::new(StreamInstance(inner))))
    }

    pub(crate) fn from_raw(handle: msquic::ffi::HQUIC, stream_type: StreamType) -> Self {
        let msquic_stream = unsafe { msquic::Stream::from_raw(handle) };
        let send_state = if stream_type == StreamType::Bidirectional {
            StreamSendState::StartComplete
        } else {
            StreamSendState::Closed
        };
        let inner = Arc::new(StreamInner::new(
            msquic_stream,
            stream_type,
            send_state,
            StreamRecvState::StartComplete,
            false,
        ));
        unsafe {
            inner.shared.msquic_stream.set_callback_handler(
                Some(StreamInner::native_callback),
                Arc::into_raw(inner.clone()) as *const c_void,
            )
        };
        let stream = Self(Arc::new(StreamInstance(inner)));
        trace!(
            "Stream({:p}, id={:?}) Start by peer",
            &*stream.0 .0,
            stream.id()
        );
        stream
    }

    pub(crate) fn poll_start(
        &mut self,
        cx: &mut Context,
        failed_on_block: bool,
    ) -> Poll<Result<(), StartError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            StreamState::Open => {
                self.0
                    .shared
                    .msquic_stream
                    .start(
                        msquic::STREAM_START_FLAG_SHUTDOWN_ON_FAIL
                            | msquic::STREAM_START_FLAG_INDICATE_PEER_ACCEPT
                            | if failed_on_block {
                                msquic::STREAM_START_FLAG_FAIL_BLOCKED
                            } else {
                                msquic::STREAM_START_FLAG_NONE
                            },
                    )
                    .map_err(StartError::OtherError)?;
                exclusive.state = StreamState::Start;
                if self.0.shared.stream_type == StreamType::Bidirectional {
                    exclusive.recv_state = StreamRecvState::Start;
                }
                exclusive.send_state = StreamSendState::Start;
            }
            StreamState::Start => {}
            _ => {
                if let Some(start_status) = &exclusive.start_status {
                    if start_status.is_ok() {
                        return Poll::Ready(Ok(()));
                    }
                    return Poll::Ready(Err(match start_status.try_as_status_code().unwrap() {
                        msquic::StatusCode::QUIC_STATUS_STREAM_LIMIT_REACHED => {
                            StartError::LimitReached
                        }
                        msquic::StatusCode::QUIC_STATUS_ABORTED
                        | msquic::StatusCode::QUIC_STATUS_INVALID_STATE => {
                            StartError::ConnectionLost(
                                exclusive.conn_error.as_ref().expect("conn_error").clone(),
                            )
                        }
                        _ => StartError::OtherError(start_status.clone()),
                    }));
                } else {
                    return Poll::Ready(Ok(()));
                }
            }
        }
        exclusive.start_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    /// Returns the stream ID.
    pub fn id(&self) -> Option<u64> {
        self.0.id()
    }

    /// Splits the stream into a read stream and a write stream.
    pub fn split(self) -> (Option<ReadStream>, Option<WriteStream>) {
        match (self.0.shared.stream_type, self.0.shared.local_open) {
            (StreamType::Unidirectional, true) => (None, Some(WriteStream(self.0))),
            (StreamType::Unidirectional, false) => (Some(ReadStream(self.0)), None),
            (StreamType::Bidirectional, _) => {
                (Some(ReadStream(self.0.clone())), Some(WriteStream(self.0)))
            }
        }
    }

    /// Poll to read from the stream into buf.
    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, ReadError>> {
        self.0.poll_read(cx, buf)
    }

    /// Poll to read the next segment of data.
    pub fn poll_read_chunk(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<StreamRecvBuffer>, ReadError>> {
        self.0.poll_read_chunk(cx)
    }

    /// Read the next segment of data.
    pub fn read_chunk(&self) -> ReadChunk<'_> {
        self.0.read_chunk()
    }

    /// Poll to write to the stream from buf.
    pub fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        buf: &[u8],
        fin: bool,
    ) -> Poll<Result<usize, WriteError>> {
        self.0.poll_write(cx, buf, fin)
    }

    /// Poll to write a bytes to the stream directly.
    pub fn poll_write_chunk(
        &mut self,
        cx: &mut Context<'_>,
        chunk: &Bytes,
        fin: bool,
    ) -> Poll<Result<usize, WriteError>> {
        self.0.poll_write_chunk(cx, chunk, fin)
    }

    /// Write a bytes to the stream directly.
    pub fn write_chunk<'a>(&'a mut self, chunk: &'a Bytes, fin: bool) -> WriteChunk<'a> {
        self.0.write_chunk(chunk, fin)
    }

    /// Poll to write the list of bytes to the stream directly.
    pub fn poll_write_chunks(
        &mut self,
        cx: &mut Context<'_>,
        chunks: &[Bytes],
        fin: bool,
    ) -> Poll<Result<usize, WriteError>> {
        self.0.poll_write_chunks(cx, chunks, fin)
    }

    /// Write the list of bytes to the stream directly.
    pub fn write_chunks<'a>(&'a mut self, chunks: &'a [Bytes], fin: bool) -> WriteChunks<'a> {
        self.0.write_chunks(chunks, fin)
    }

    /// Poll to finish writing to the stream.
    pub fn poll_finish_write(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), WriteError>> {
        self.0.poll_finish_write(cx)
    }

    /// Poll to abort writing to the stream.
    pub fn poll_abort_write(
        &mut self,
        cx: &mut Context<'_>,
        error_code: u64,
    ) -> Poll<Result<(), WriteError>> {
        self.0.poll_abort_write(cx, error_code)
    }

    /// Abort writing to the stream.
    pub fn abort_write(&mut self, error_code: u64) -> Result<(), WriteError> {
        self.0.abort_write(error_code)
    }

    /// Poll to abort reading from the stream.
    pub fn poll_abort_read(
        &mut self,
        cx: &mut Context<'_>,
        error_code: u64,
    ) -> Poll<Result<(), ReadError>> {
        self.0.poll_abort_read(cx, error_code)
    }

    /// Abort reading from the stream.
    pub fn abort_read(&mut self, error_code: u64) -> Result<(), ReadError> {
        self.0.abort_read(error_code)
    }
}

/// A stream that can only be read from.
#[derive(Debug)]
pub struct ReadStream(Arc<StreamInstance>);

impl ReadStream {
    /// Returns the stream ID.
    pub fn id(&self) -> Option<u64> {
        self.0.id()
    }

    /// Poll to read from the stream into buf.
    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, ReadError>> {
        self.0.poll_read(cx, buf)
    }

    /// Poll to read the next segment of data.
    pub fn poll_read_chunk(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<StreamRecvBuffer>, ReadError>> {
        self.0.poll_read_chunk(cx)
    }

    /// Read the next segment of data.
    pub fn read_chunk(&self) -> ReadChunk<'_> {
        self.0.read_chunk()
    }

    /// Poll to abort reading from the stream.
    pub fn poll_abort_read(
        &mut self,
        cx: &mut Context<'_>,
        error_code: u64,
    ) -> Poll<Result<(), ReadError>> {
        self.0.poll_abort_read(cx, error_code)
    }

    /// Abort reading from the stream.
    pub fn abort_read(&mut self, error_code: u64) -> Result<(), ReadError> {
        self.0.abort_read(error_code)
    }
}

/// A stream that can only be written to.
#[derive(Debug)]
pub struct WriteStream(Arc<StreamInstance>);

impl WriteStream {
    /// Returns the stream ID.
    pub fn id(&self) -> Option<u64> {
        self.0.id()
    }

    /// Poll to write to the stream from buf.
    pub fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        buf: &[u8],
        fin: bool,
    ) -> Poll<Result<usize, WriteError>> {
        self.0.poll_write(cx, buf, fin)
    }

    /// Poll to write a bytes to the stream directly.
    pub fn poll_write_chunk(
        &mut self,
        cx: &mut Context<'_>,
        chunk: &Bytes,
        fin: bool,
    ) -> Poll<Result<usize, WriteError>> {
        self.0.poll_write_chunk(cx, chunk, fin)
    }

    /// Write a bytes to the stream directly.
    pub fn write_chunk<'a>(&'a mut self, chunk: &'a Bytes, fin: bool) -> WriteChunk<'a> {
        self.0.write_chunk(chunk, fin)
    }

    /// Poll to write the list of bytes to the stream directly.
    pub fn poll_write_chunks(
        &mut self,
        cx: &mut Context<'_>,
        chunks: &[Bytes],
        fin: bool,
    ) -> Poll<Result<usize, WriteError>> {
        self.0.poll_write_chunks(cx, chunks, fin)
    }

    /// Write the list of bytes to the stream directly.
    pub fn write_chunks<'a>(&'a mut self, chunks: &'a [Bytes], fin: bool) -> WriteChunks<'a> {
        self.0.write_chunks(chunks, fin)
    }

    /// Poll to finish writing to the stream.
    pub fn poll_finish_write(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), WriteError>> {
        self.0.poll_finish_write(cx)
    }

    /// Poll to abort writing to the stream.
    pub fn poll_abort_write(
        &mut self,
        cx: &mut Context<'_>,
        error_code: u64,
    ) -> Poll<Result<(), WriteError>> {
        self.0.poll_abort_write(cx, error_code)
    }

    /// Abort writing to the stream.
    pub fn abort_write(&mut self, error_code: u64) -> Result<(), WriteError> {
        self.0.abort_write(error_code)
    }
}

impl StreamInstance {
    pub(crate) fn id(&self) -> Option<u64> {
        let id = { *self.0.shared.id.read().unwrap() };
        if id.is_some() {
            id
        } else {
            let res = unsafe {
                msquic::Api::get_param_auto::<u64>(
                    self.0.shared.msquic_stream.as_raw(),
                    msquic::PARAM_STREAM_ID,
                )
            };
            if let Ok(id) = res {
                self.0.shared.id.write().unwrap().replace(id);
                Some(id)
            } else {
                None
            }
        }
    }

    pub(crate) fn poll_read(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, ReadError>> {
        self.poll_read_generic(cx, |recv_buffers, read_complete_buffers| {
            let mut read = 0;
            let mut fin = false;
            loop {
                if read == buf.len() {
                    return ReadStatus::Readable(read);
                }

                match recv_buffers
                    .front_mut()
                    .and_then(|x| x.get_bytes_upto_size(buf.len() - read))
                {
                    Some(slice) => {
                        let len = slice.len();
                        buf[read..read + len].copy_from_slice(slice);
                        read += len;
                    }
                    None => {
                        if let Some(mut recv_buffer) = recv_buffers.pop_front() {
                            recv_buffer.set_stream(self.0.clone());
                            fin = recv_buffer.fin();
                            read_complete_buffers.push(recv_buffer);
                            continue;
                        } else {
                            return (if read > 0 { Some(read) } else { None }, fin).into();
                        }
                    }
                }
            }
        })
        .map(|res| res.map(|x| x.unwrap_or(0)))
    }

    fn poll_read_chunk(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<StreamRecvBuffer>, ReadError>> {
        self.poll_read_generic(cx, |recv_buffers, _| {
            recv_buffers
                .pop_front()
                .map(|mut recv_buffer| {
                    let fin = recv_buffer.fin();
                    recv_buffer.set_stream(self.0.clone());
                    (Some(recv_buffer), fin)
                })
                .unwrap_or((None, false))
                .into()
        })
    }

    fn read_chunk(&self) -> ReadChunk<'_> {
        ReadChunk { stream: self }
    }

    fn poll_read_generic<T, U>(
        &self,
        cx: &mut Context<'_>,
        mut read_fn: T,
    ) -> Poll<Result<Option<U>, ReadError>>
    where
        T: FnMut(&mut VecDeque<StreamRecvBuffer>, &mut Vec<StreamRecvBuffer>) -> ReadStatus<U>,
    {
        let res;
        let mut read_complete_buffers = Vec::new();
        {
            let mut exclusive = self.0.exclusive.lock().unwrap();
            match exclusive.recv_state {
                StreamRecvState::Closed => {
                    return Poll::Ready(Err(ReadError::Closed));
                }
                StreamRecvState::Start => {
                    exclusive.start_waiters.push(cx.waker().clone());
                    return Poll::Pending;
                }
                StreamRecvState::StartComplete => {}
                StreamRecvState::Shutdown => {
                    return Poll::Ready(Ok(None));
                }
                StreamRecvState::ShutdownComplete => {
                    if let Some(conn_error) = &exclusive.conn_error {
                        return Poll::Ready(Err(ReadError::ConnectionLost(conn_error.clone())));
                    } else if let Some(error_code) = &exclusive.recv_error_code {
                        return Poll::Ready(Err(ReadError::Reset(*error_code)));
                    } else {
                        return Poll::Ready(Ok(None));
                    }
                }
            }

            let status = read_fn(&mut exclusive.recv_buffers, &mut read_complete_buffers);

            res = match status {
                ReadStatus::Readable(read) | ReadStatus::Blocked(Some(read)) => {
                    Poll::Ready(Ok(Some(read)))
                }
                ReadStatus::Finished(read) => {
                    exclusive.recv_state = StreamRecvState::Shutdown;
                    Poll::Ready(Ok(read))
                }
                ReadStatus::Blocked(None) => {
                    exclusive.read_waiters.push(cx.waker().clone());
                    Poll::Pending
                }
            };
        }
        res
    }

    pub(crate) fn poll_write(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        fin: bool,
    ) -> Poll<Result<usize, WriteError>> {
        self.poll_write_generic(cx, |write_buf| {
            let written = write_buf.put_slice(buf);
            if written == buf.len() && !fin {
                WriteStatus::Writable(written)
            } else {
                (Some(written), fin).into()
            }
        })
        .map(|res| res.map(|x| x.unwrap_or(0)))
    }

    pub(crate) fn poll_write_chunk(
        &self,
        cx: &mut Context<'_>,
        chunk: &Bytes,
        fin: bool,
    ) -> Poll<Result<usize, WriteError>> {
        self.poll_write_generic(cx, |write_buf| {
            let written = write_buf.put_zerocopy(chunk);
            if written == chunk.len() && !fin {
                WriteStatus::Writable(written)
            } else {
                (Some(written), fin).into()
            }
        })
        .map(|res| res.map(|x| x.unwrap_or(0)))
    }

    pub(crate) fn write_chunk<'a>(&'a self, chunk: &'a Bytes, fin: bool) -> WriteChunk<'a> {
        WriteChunk {
            stream: self,
            chunk,
            fin,
        }
    }

    fn poll_write_chunks(
        &self,
        cx: &mut Context<'_>,
        chunks: &[Bytes],
        fin: bool,
    ) -> Poll<Result<usize, WriteError>> {
        self.poll_write_generic(cx, |write_buf| {
            let (mut total_len, mut total_written) = (0, 0);
            for buf in chunks {
                total_len += buf.len();
                total_written += write_buf.put_zerocopy(buf);
            }
            if total_written == total_len && !fin {
                WriteStatus::Writable(total_written)
            } else {
                (Some(total_written), fin).into()
            }
        })
        .map(|res| res.map(|x| x.unwrap_or(0)))
    }

    pub(crate) fn write_chunks<'a>(&'a self, chunks: &'a [Bytes], fin: bool) -> WriteChunks<'a> {
        WriteChunks {
            stream: self,
            chunks,
            fin,
        }
    }

    fn poll_write_generic<T, U>(
        &self,
        _cx: &mut Context<'_>,
        mut write_fn: T,
    ) -> Poll<Result<Option<U>, WriteError>>
    where
        T: FnMut(&mut WriteBuffer) -> WriteStatus<U>,
    {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.send_state {
            StreamSendState::Closed => {
                return Poll::Ready(Err(WriteError::Closed));
            }
            StreamSendState::Start => {
                exclusive.start_waiters.push(_cx.waker().clone());
                return Poll::Pending;
            }
            StreamSendState::StartComplete => {}
            StreamSendState::Shutdown => {
                return Poll::Ready(Err(WriteError::Finished));
            }
            StreamSendState::ShutdownComplete => {
                if let Some(conn_error) = &exclusive.conn_error {
                    return Poll::Ready(Err(WriteError::ConnectionLost(conn_error.clone())));
                } else if let Some(error_code) = &exclusive.send_error_code {
                    return Poll::Ready(Err(WriteError::Stopped(*error_code)));
                } else {
                    return Poll::Ready(Err(WriteError::Finished));
                }
            }
        }
        let mut write_buf = exclusive.write_pool.pop().unwrap_or(WriteBuffer::new());
        let status = write_fn(&mut write_buf);
        let buffers = unsafe {
            let (data, len) = write_buf.get_buffers();
            std::slice::from_raw_parts(data, len)
        };
        match status {
            WriteStatus::Writable(val) | WriteStatus::Blocked(Some(val)) => {
                match unsafe {
                    self.0.shared.msquic_stream.send(
                        buffers,
                        msquic::SEND_FLAG_NONE,
                        write_buf.into_raw() as *const _,
                    )
                }
                .map_err(WriteError::OtherError)
                {
                    Ok(()) => Poll::Ready(Ok(Some(val))),
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            WriteStatus::Blocked(None) => unreachable!(),
            WriteStatus::Finished(val) => {
                match unsafe {
                    self.0.shared.msquic_stream.send(
                        buffers,
                        msquic::SEND_FLAG_FIN,
                        write_buf.into_raw() as *const _,
                    )
                }
                .map_err(WriteError::OtherError)
                {
                    Ok(()) => {
                        exclusive.send_state = StreamSendState::Shutdown;
                        Poll::Ready(Ok(val))
                    }
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
        }
    }

    pub(crate) fn poll_finish_write(&self, cx: &mut Context<'_>) -> Poll<Result<(), WriteError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.send_state {
            StreamSendState::Start => {
                exclusive.start_waiters.push(cx.waker().clone());
                return Poll::Pending;
            }
            StreamSendState::StartComplete => {
                match self
                    .0
                    .shared
                    .msquic_stream
                    .shutdown(msquic::STREAM_SHUTDOWN_FLAG_GRACEFUL, 0)
                    .map_err(WriteError::OtherError)
                {
                    Ok(()) => {
                        exclusive.send_state = StreamSendState::Shutdown;
                    }
                    Err(e) => return Poll::Ready(Err(e)),
                }
            }
            StreamSendState::Shutdown => {}
            StreamSendState::ShutdownComplete => {
                if let Some(conn_error) = &exclusive.conn_error {
                    return Poll::Ready(Err(WriteError::ConnectionLost(conn_error.clone())));
                } else if let Some(error_code) = &exclusive.send_error_code {
                    return Poll::Ready(Err(WriteError::Stopped(*error_code)));
                } else {
                    return Poll::Ready(Ok(()));
                }
            }
            _ => {
                return Poll::Ready(Err(WriteError::Closed));
            }
        }
        exclusive.write_shutdown_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    pub(crate) fn poll_abort_write(
        &self,
        cx: &mut Context<'_>,
        error_code: u64,
    ) -> Poll<Result<(), WriteError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.send_state {
            StreamSendState::Start => {
                exclusive.start_waiters.push(cx.waker().clone());
                return Poll::Pending;
            }
            StreamSendState::StartComplete => {
                match self
                    .0
                    .shared
                    .msquic_stream
                    .shutdown(msquic::STREAM_SHUTDOWN_FLAG_ABORT_SEND, error_code)
                    .map_err(WriteError::OtherError)
                {
                    Ok(()) => {
                        exclusive.send_state = StreamSendState::Shutdown;
                    }
                    Err(e) => return Poll::Ready(Err(e)),
                }
            }
            StreamSendState::Shutdown => {}
            StreamSendState::ShutdownComplete => {
                if let Some(conn_error) = &exclusive.conn_error {
                    return Poll::Ready(Err(WriteError::ConnectionLost(conn_error.clone())));
                } else if let Some(error_code) = &exclusive.send_error_code {
                    return Poll::Ready(Err(WriteError::Stopped(*error_code)));
                } else {
                    return Poll::Ready(Ok(()));
                }
            }
            _ => {
                return Poll::Ready(Err(WriteError::Closed));
            }
        }
        exclusive.write_shutdown_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    pub(crate) fn abort_write(&self, error_code: u64) -> Result<(), WriteError> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.send_state {
            StreamSendState::StartComplete => {
                self.0
                    .shared
                    .msquic_stream
                    .shutdown(msquic::STREAM_SHUTDOWN_FLAG_ABORT_SEND, error_code)
                    .map_err(WriteError::OtherError)?;
                exclusive.send_state = StreamSendState::Shutdown;
                Ok(())
            }
            _ => Err(WriteError::Closed),
        }
    }

    pub(crate) fn poll_abort_read(
        &self,
        cx: &mut Context<'_>,
        error_code: u64,
    ) -> Poll<Result<(), ReadError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.recv_state {
            StreamRecvState::Start => {
                exclusive.start_waiters.push(cx.waker().clone());
                Poll::Pending
            }
            StreamRecvState::StartComplete => {
                match self
                    .0
                    .shared
                    .msquic_stream
                    .shutdown(msquic::STREAM_SHUTDOWN_FLAG_ABORT_RECEIVE, error_code)
                    .map_err(ReadError::OtherError)
                {
                    Ok(()) => {
                        exclusive.recv_state = StreamRecvState::ShutdownComplete;
                        exclusive
                            .read_waiters
                            .drain(..)
                            .for_each(|waker| waker.wake());
                        Poll::Ready(Ok(()))
                    }
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            StreamRecvState::ShutdownComplete => {
                if let Some(conn_error) = &exclusive.conn_error {
                    Poll::Ready(Err(ReadError::ConnectionLost(conn_error.clone())))
                } else if let Some(error_code) = &exclusive.recv_error_code {
                    Poll::Ready(Err(ReadError::Reset(*error_code)))
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            _ => Poll::Ready(Err(ReadError::Closed)),
        }
    }

    pub(crate) fn abort_read(&self, error_code: u64) -> Result<(), ReadError> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.recv_state {
            StreamRecvState::StartComplete => {
                self.0
                    .shared
                    .msquic_stream
                    .shutdown(msquic::STREAM_SHUTDOWN_FLAG_ABORT_RECEIVE, error_code)
                    .map_err(ReadError::OtherError)?;
                exclusive.recv_state = StreamRecvState::ShutdownComplete;
            }
            _ => {
                return Err(ReadError::Closed);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
struct StreamInstance(Arc<StreamInner>);

impl Drop for StreamInstance {
    fn drop(&mut self) {
        trace!("StreamInstance({:p}) dropping", &*self.0);
        let mut exclusive = self.0.exclusive.lock().unwrap();
        if !exclusive.recv_buffers.is_empty() {
            trace!(
                "StreamInstance({:p}) read complete {}",
                &*self.0,
                exclusive.recv_len - exclusive.read_complete_cursor
            );
            exclusive.recv_buffers.clear();
            self.0
                .shared
                .msquic_stream
                .receive_complete((exclusive.recv_len - exclusive.read_complete_cursor) as u64);
        }
        match exclusive.state {
            StreamState::Start | StreamState::StartComplete => {
                trace!("StreamInstance({:p}) shutdown while dropping", &*self.0);
                let _ = self.0.shared.msquic_stream.shutdown(
                    msquic::STREAM_SHUTDOWN_FLAG_ABORT_SEND
                        | msquic::STREAM_SHUTDOWN_FLAG_ABORT_RECEIVE
                        | msquic::STREAM_SHUTDOWN_FLAG_IMMEDIATE,
                    0,
                );
            }
            _ => {}
        }
    }
}

impl Deref for StreamInstance {
    type Target = StreamInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub(crate) struct StreamInner {
    exclusive: Mutex<StreamInnerExclusive>,
    pub(crate) shared: StreamInnerShared,
}

struct StreamInnerExclusive {
    state: StreamState,
    start_status: Option<msquic::Status>,
    recv_state: StreamRecvState,
    recv_buffers: VecDeque<StreamRecvBuffer>,
    recv_len: usize,
    read_complete_map: RangeSet<usize>,
    read_complete_cursor: usize,
    send_state: StreamSendState,
    write_pool: Vec<WriteBuffer>,
    recv_error_code: Option<u64>,
    send_error_code: Option<u64>,
    conn_error: Option<ConnectionError>,
    start_waiters: Vec<Waker>,
    read_waiters: Vec<Waker>,
    write_shutdown_waiters: Vec<Waker>,
}

pub(crate) struct StreamInnerShared {
    stream_type: StreamType,
    local_open: bool,
    id: RwLock<Option<u64>>,
    pub(crate) msquic_stream: msquic::Stream,
}

#[derive(Debug, PartialEq)]
enum StreamState {
    Open,
    Start,
    StartComplete,
    ShutdownComplete,
}

#[derive(Debug, PartialEq)]
enum StreamRecvState {
    Closed,
    Start,
    StartComplete,
    Shutdown,
    ShutdownComplete,
}

#[derive(Debug, PartialEq)]
enum StreamSendState {
    Closed,
    Start,
    StartComplete,
    Shutdown,
    ShutdownComplete,
}

impl StreamInner {
    fn new(
        msquic_stream: msquic::Stream,
        stream_type: StreamType,
        send_state: StreamSendState,
        recv_state: StreamRecvState,
        local_open: bool,
    ) -> Self {
        Self {
            exclusive: Mutex::new(StreamInnerExclusive {
                state: StreamState::Open,
                start_status: None,
                recv_state,
                recv_buffers: VecDeque::new(),
                recv_len: 0,
                read_complete_map: RangeSet::new(),
                read_complete_cursor: 0,
                send_state,
                write_pool: Vec::new(),
                recv_error_code: None,
                send_error_code: None,
                conn_error: None,
                start_waiters: Vec::new(),
                read_waiters: Vec::new(),
                write_shutdown_waiters: Vec::new(),
            }),
            shared: StreamInnerShared {
                msquic_stream,
                local_open,
                id: RwLock::new(None),
                stream_type,
            },
        }
    }

    pub(crate) fn read_complete(&self, buffer: &StreamRecvBuffer) {
        let buffer_range = buffer.range();
        trace!(
            "StreamInner({:p}) read complete offset={} len={}",
            self,
            buffer_range.start,
            buffer_range.end - buffer_range.start
        );

        let mut exclusive = self.exclusive.lock().unwrap();
        if !buffer_range.is_empty() {
            exclusive.read_complete_map.insert(buffer_range);
        }
        let complete_len = if let Some(complete_range) = exclusive.read_complete_map.first() {
            trace!(
                "StreamInner({:p}) complete read offset={} len={}",
                self,
                complete_range.start,
                complete_range.end - complete_range.start
            );

            if complete_range.start == 0 && exclusive.read_complete_cursor < complete_range.end {
                let complete_len = complete_range.end - exclusive.read_complete_cursor;
                exclusive.read_complete_cursor = complete_range.end;
                Some(complete_len)
            } else if complete_range.start == 0
                && exclusive.read_complete_cursor == complete_range.end
                && buffer.offset() == complete_range.end
                && buffer.is_empty()
                && buffer.fin()
            {
                Some(0)
            } else {
                None
            }
        } else if buffer.is_empty() && buffer.fin() {
            Some(0)
        } else {
            None
        };
        if let Some(complete_len) = complete_len {
            trace!(
                "StreamInner({:p}) call receive_complete len={}",
                self,
                complete_len
            );
            self.shared
                .msquic_stream
                .receive_complete(complete_len as u64);
        }
    }

    fn handle_event_start_complete(
        &self,
        status: msquic::Status,
        id: u64,
        peer_accepted: bool,
    ) -> msquic::StatusCode {
        if status.is_ok() {
            self.shared.id.write().unwrap().replace(id);
        }
        trace!(
            "Stream({:p}, id={:?}) start complete status={:?}, peer_accepted={}, id={}",
            self,
            self.shared.id.read(),
            status,
            peer_accepted,
            id,
        );
        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive.start_status = Some(status.clone());
        if status.is_ok() && peer_accepted {
            exclusive.state = StreamState::StartComplete;
            if self.shared.stream_type == StreamType::Bidirectional {
                exclusive.recv_state = StreamRecvState::StartComplete;
            }
            exclusive.send_state = StreamSendState::StartComplete;
        }

        if status.0 == msquic::StatusCode::QUIC_STATUS_STREAM_LIMIT_REACHED.into() || peer_accepted
        {
            exclusive
                .start_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
        }
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_receive(
        &self,
        absolute_offset: u64,
        total_buffer_length: &mut u64,
        buffers: &[msquic::BufferRef],
        flags: msquic::ffi::QUIC_RECEIVE_FLAGS,
    ) -> msquic::StatusCode {
        trace!(
            "Stream({:p}, id={:?}) Receive {} offsets {} bytes, fin {}",
            self,
            self.shared.id.read(),
            absolute_offset,
            total_buffer_length,
            (flags & msquic::ffi::QUIC_RECEIVE_FLAGS_QUIC_RECEIVE_FLAG_FIN)
                == msquic::ffi::QUIC_RECEIVE_FLAGS_QUIC_RECEIVE_FLAG_FIN
        );

        let arc_inner: Arc<Self> = unsafe { Arc::from_raw(self as *const _) };

        let recv_buffer = StreamRecvBuffer::new(
            absolute_offset as usize,
            buffers,
            (flags & msquic::ffi::QUIC_RECEIVE_FLAGS_QUIC_RECEIVE_FLAG_FIN)
                == msquic::ffi::QUIC_RECEIVE_FLAGS_QUIC_RECEIVE_FLAG_FIN,
        );

        let _ = Arc::into_raw(arc_inner);

        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive.recv_len += *total_buffer_length as usize;
        exclusive.recv_buffers.push_back(recv_buffer);
        exclusive
            .read_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::StatusCode::QUIC_STATUS_PENDING
    }

    fn handle_event_send_complete(
        &self,
        _canceled: bool,
        client_context: *const c_void,
    ) -> msquic::StatusCode {
        trace!(
            "Stream({:p}, id={:?}) Send complete",
            self,
            self.shared.id.read()
        );

        let mut write_buf = unsafe { WriteBuffer::from_raw(client_context) };
        let mut exclusive = self.exclusive.lock().unwrap();
        write_buf.reset();
        exclusive.write_pool.push(write_buf);
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_peer_send_shutdown(&self) -> msquic::StatusCode {
        trace!(
            "Stream({:p}, id={:?}) Peer send shutdown",
            self,
            self.shared.id.read()
        );
        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive.recv_state = StreamRecvState::ShutdownComplete;
        exclusive
            .read_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_peer_send_aborted(&self, error_code: u64) -> msquic::StatusCode {
        trace!(
            "Stream({:p}, id={:?}) Peer send aborted",
            self,
            self.shared.id.read()
        );
        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive.recv_state = StreamRecvState::ShutdownComplete;
        exclusive.recv_error_code = Some(error_code);
        exclusive
            .read_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_peer_receive_aborted(&self, error_code: u64) -> msquic::StatusCode {
        trace!(
            "Stream({:p}, id={:?}) Peer receive aborted",
            self,
            self.shared.id.read()
        );
        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive.send_state = StreamSendState::ShutdownComplete;
        exclusive.send_error_code = Some(error_code);
        exclusive
            .write_shutdown_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_send_shutdown_complete(&self, _graceful: bool) -> msquic::StatusCode {
        trace!(
            "Stream({:p}, id={:?}) Send shutdown complete",
            self,
            self.shared.id.read()
        );
        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive.send_state = StreamSendState::ShutdownComplete;
        exclusive
            .write_shutdown_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_shutdown_complete(
        &self,
        connection_shutdown: bool,
        _app_close_in_progress: bool,
        connection_shutdown_by_app: bool,
        connection_closed_remotely: bool,
        connection_error_code: u64,
        connection_close_status: msquic::Status,
    ) -> msquic::StatusCode {
        trace!(
            "Stream({:p}, id={:?}) Shutdown complete",
            self,
            self.shared.id.read()
        );
        {
            let mut exclusive = self.exclusive.lock().unwrap();
            exclusive.state = StreamState::ShutdownComplete;
            exclusive.recv_state = StreamRecvState::ShutdownComplete;
            exclusive.send_state = StreamSendState::ShutdownComplete;
            if connection_shutdown {
                match (connection_shutdown_by_app, connection_closed_remotely) {
                    (true, true) => {
                        exclusive.conn_error =
                            Some(ConnectionError::ShutdownByPeer(connection_error_code));
                    }
                    (true, false) => {
                        exclusive.conn_error = Some(ConnectionError::ShutdownByLocal);
                    }
                    (false, true) | (false, false) => {
                        exclusive.conn_error = Some(ConnectionError::ShutdownByTransport(
                            connection_close_status,
                            connection_error_code,
                        ));
                    }
                }
            }
            exclusive
                .start_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
            exclusive
                .read_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
        }
        unsafe {
            Arc::from_raw(self as *const _);
        }
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_ideal_send_buffer_size(&self, _byte_count: u64) -> msquic::StatusCode {
        trace!(
            "Stream({:p}, id={:?}) Ideal send buffer size",
            self,
            self.shared.id.read()
        );
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_peer_accepted(&self) -> msquic::StatusCode {
        trace!(
            "Stream({:p}, id={:?}) Peer accepted",
            self,
            self.shared.id.read()
        );
        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive.state = StreamState::StartComplete;
        if self.shared.stream_type == StreamType::Bidirectional {
            exclusive.recv_state = StreamRecvState::StartComplete;
        }
        exclusive.send_state = StreamSendState::StartComplete;
        exclusive
            .start_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    extern "C" fn native_callback(
        _stream: msquic::ffi::HQUIC,
        context: *mut c_void,
        event: *mut msquic::ffi::QUIC_STREAM_EVENT,
    ) -> msquic::ffi::QUIC_STATUS {
        let inner = unsafe { &*(context as *const Self) };
        let ev_ref = unsafe { event.as_ref().unwrap() };
        let event = msquic::StreamEvent::from(unsafe { event.as_mut().unwrap() });

        let res = match event {
            msquic::StreamEvent::StartComplete {
                status,
                id,
                peer_accepted,
            } => inner.handle_event_start_complete(status, id, peer_accepted),
            msquic::StreamEvent::Receive {
                absolute_offset,
                total_buffer_length,
                buffers,
                flags,
            } => inner.handle_event_receive(absolute_offset, total_buffer_length, buffers, flags),
            msquic::StreamEvent::SendComplete {
                cancelled,
                client_context,
            } => inner.handle_event_send_complete(cancelled, client_context),
            msquic::StreamEvent::PeerSendShutdown => inner.handle_event_peer_send_shutdown(),
            msquic::StreamEvent::PeerSendAborted { error_code } => {
                inner.handle_event_peer_send_aborted(error_code)
            }
            msquic::StreamEvent::PeerReceiveAborted { error_code } => {
                inner.handle_event_peer_receive_aborted(error_code)
            }
            msquic::StreamEvent::SendShutdownComplete { graceful } => {
                inner.handle_event_send_shutdown_complete(graceful)
            }
            msquic::StreamEvent::ShutdownComplete {
                connection_shutdown,
                app_close_in_progress,
                connection_shutdown_by_app,
                connection_closed_remotely,
                connection_error_code,
                connection_close_status,
            } => inner.handle_event_shutdown_complete(
                connection_shutdown,
                app_close_in_progress,
                connection_shutdown_by_app,
                connection_closed_remotely,
                connection_error_code,
                connection_close_status,
            ),
            msquic::StreamEvent::IdealSendBufferSize { byte_count } => {
                inner.handle_event_ideal_send_buffer_size(byte_count)
            }
            msquic::StreamEvent::PeerAccepted => inner.handle_event_peer_accepted(),
            _ => {
                trace!("Stream({:p}) Other callback {}", inner, ev_ref.Type);
                msquic::StatusCode::QUIC_STATUS_SUCCESS
            }
        };
        res.into()
    }
}

impl Drop for StreamInner {
    fn drop(&mut self) {
        trace!("StreamInner({:p}) dropping", self);
    }
}

impl fmt::Debug for StreamInnerExclusive {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Exclusive")
            .field("state", &self.state)
            .field("recv_state", &self.recv_state)
            .field("send_state", &self.send_state)
            .finish()
    }
}

impl fmt::Debug for StreamInnerShared {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Shared")
            .field("type", &self.stream_type)
            .field("id", &self.id)
            .finish()
    }
}
pub struct ReadChunk<'a> {
    stream: &'a StreamInstance,
}

impl Future for ReadChunk<'_> {
    type Output = Result<Option<StreamRecvBuffer>, ReadError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.stream.poll_read_chunk(cx)
    }
}

pub struct WriteChunk<'a> {
    stream: &'a StreamInstance,
    chunk: &'a Bytes,
    fin: bool,
}

impl Future for WriteChunk<'_> {
    type Output = Result<usize, WriteError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.stream.poll_write_chunk(cx, self.chunk, self.fin)
    }
}

pub struct WriteChunks<'a> {
    stream: &'a StreamInstance,
    chunks: &'a [Bytes],
    fin: bool,
}

impl Future for WriteChunks<'_> {
    type Output = Result<usize, WriteError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.stream.poll_write_chunks(cx, self.chunks, self.fin)
    }
}

#[cfg(feature = "tokio")]
impl tokio::io::AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let len = ready!(Self::poll_read(self.get_mut(), cx, buf.initialized_mut()))?;
        buf.set_filled(len);
        Poll::Ready(Ok(()))
    }
}

#[cfg(feature = "tokio")]
impl tokio::io::AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let len = ready!(Self::poll_write(self.get_mut(), cx, buf, false))?;
        Poll::Ready(Ok(len))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<std::io::Result<()>> {
        ready!(Self::poll_finish_write(self.get_mut(), cx))?;
        Poll::Ready(Ok(()))
    }
}

#[cfg(feature = "tokio")]
impl tokio::io::AsyncRead for ReadStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let len = ready!(Self::poll_read(self.get_mut(), cx, buf.initialized_mut()))?;
        buf.set_filled(len);
        Poll::Ready(Ok(()))
    }
}

#[cfg(feature = "tokio")]
impl tokio::io::AsyncWrite for WriteStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let len = ready!(Self::poll_write(self.get_mut(), cx, buf, false))?;
        Poll::Ready(Ok(len))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<std::io::Result<()>> {
        ready!(Self::poll_finish_write(self.get_mut(), cx))?;
        Poll::Ready(Ok(()))
    }
}

impl futures_io::AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let len = ready!(Self::poll_read(self.get_mut(), cx, buf))?;
        Poll::Ready(Ok(len))
    }
}

impl futures_io::AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let len = ready!(Self::poll_write(self.get_mut(), cx, buf, false))?;
        Poll::Ready(Ok(len))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        ready!(Self::poll_finish_write(self.get_mut(), cx))?;
        Poll::Ready(Ok(()))
    }
}

impl futures_io::AsyncRead for ReadStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let len = ready!(Self::poll_read(self.get_mut(), cx, buf))?;
        Poll::Ready(Ok(len))
    }
}

impl futures_io::AsyncWrite for WriteStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let len = ready!(Self::poll_write(self.get_mut(), cx, buf, false))?;
        Poll::Ready(Ok(len))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        ready!(Self::poll_finish_write(self.get_mut(), cx))?;
        Poll::Ready(Ok(()))
    }
}

enum ReadStatus<T> {
    Readable(T),
    Finished(Option<T>),
    Blocked(Option<T>),
}

impl<T> From<(Option<T>, bool)> for ReadStatus<T> {
    fn from(status: (Option<T>, bool)) -> Self {
        match status {
            (read, true) => Self::Finished(read),
            (read, false) => Self::Blocked(read),
        }
    }
}

enum WriteStatus<T> {
    Writable(T),
    Finished(Option<T>),
    Blocked(Option<T>),
}

impl<T> From<(Option<T>, bool)> for WriteStatus<T> {
    fn from(status: (Option<T>, bool)) -> Self {
        match status {
            (write, true) => Self::Finished(write),
            (write, false) => Self::Blocked(write),
        }
    }
}

#[derive(Debug, Error, Clone)]
pub enum StartError {
    #[error("connection not started yet")]
    ConnectionNotStarted,
    #[error("reach stream count limit")]
    LimitReached,
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
    #[error("other error: status {0:?}")]
    OtherError(msquic::Status),
}

#[derive(Debug, Error, Clone)]
pub enum ReadError {
    #[error("stream not opened for reading")]
    Closed,
    #[error("stream reset by peer: error {0}")]
    Reset(u64),
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
    #[error("other error: status {0:?}")]
    OtherError(msquic::Status),
}

impl From<ReadError> for std::io::Error {
    fn from(e: ReadError) -> Self {
        let kind = match e {
            ReadError::Closed => std::io::ErrorKind::NotConnected,
            ReadError::Reset(_) => std::io::ErrorKind::ConnectionReset,
            ReadError::ConnectionLost(ConnectionError::ConnectionClosed) => {
                std::io::ErrorKind::NotConnected
            }
            ReadError::ConnectionLost(_) => std::io::ErrorKind::ConnectionAborted,
            ReadError::OtherError(_) => std::io::ErrorKind::Other,
        };
        Self::new(kind, e)
    }
}

#[derive(Debug, Error, Clone)]
pub enum WriteError {
    #[error("stream not opened for writing")]
    Closed,
    #[error("stream finished")]
    Finished,
    #[error("stream stopped by peer: error {0}")]
    Stopped(u64),
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
    #[error("other error: status {0:?}")]
    OtherError(msquic::Status),
}

impl From<WriteError> for std::io::Error {
    fn from(e: WriteError) -> Self {
        let kind = match e {
            WriteError::Closed
            | WriteError::Finished
            | WriteError::ConnectionLost(ConnectionError::ConnectionClosed) => {
                std::io::ErrorKind::NotConnected
            }
            WriteError::Stopped(_) => std::io::ErrorKind::ConnectionReset,
            WriteError::ConnectionLost(_) => std::io::ErrorKind::ConnectionAborted,
            WriteError::OtherError(_) => std::io::ErrorKind::Other,
        };
        Self::new(kind, e)
    }
}
