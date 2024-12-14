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
        msquic_api: &msquic::Api,
        stream_type: StreamType,
    ) -> Self {
        let msquic_stream = msquic::Stream::new(msquic_api);
        let flags = if stream_type == StreamType::Unidirectional {
            msquic::STREAM_OPEN_FLAG_UNIDIRECTIONAL
        } else {
            msquic::STREAM_OPEN_FLAG_NONE
        };
        let inner = Arc::new(StreamInner::new(
            msquic_stream,
            stream_type,
            StreamSendState::Closed,
            StreamRecvState::Closed,
            true,
        ));
        inner
            .shared
            .msquic_stream
            .open(
                msquic_conn,
                flags,
                StreamInner::native_callback,
                Arc::into_raw(inner.clone()) as *const c_void,
            )
            .unwrap();
        trace!("Stream({:p}) Open by local", &*inner);

        Self(Arc::new(StreamInstance(inner)))
    }

    pub(crate) fn from_handle(
        handle: msquic::Handle,
        msquic_api: &msquic::Api,
        stream_type: StreamType,
    ) -> Self {
        let msquic_stream = msquic::Stream::from_parts(handle, msquic_api);
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
        inner.shared.msquic_stream.set_callback_handler(
            StreamInner::native_callback,
            Arc::into_raw(inner.clone()) as *const c_void,
        );
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
                    .unwrap();
                exclusive.state = StreamState::Start;
                if self.0.shared.stream_type == StreamType::Bidirectional {
                    exclusive.recv_state = StreamRecvState::Start;
                }
                exclusive.send_state = StreamSendState::Start;
            }
            StreamState::Start => {}
            _ => {
                if let Some(start_status) = exclusive.start_status {
                    if msquic::Status::succeeded(start_status) {
                        return Poll::Ready(Ok(()));
                    }
                    return Poll::Ready(Err(match start_status {
                        msquic::QUIC_STATUS_STREAM_LIMIT_REACHED => StartError::LimitReached,
                        msquic::QUIC_STATUS_ABORTED | msquic::QUIC_STATUS_INVALID_STATE => {
                            StartError::ConnectionLost(
                                exclusive.conn_error.as_ref().expect("conn_error").clone(),
                            )
                        }
                        _ => StartError::Unknown(start_status),
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
    pub fn write_chunks<'a>(
        &'a mut self,
        chunks: &'a [Bytes],
        fin: bool,
    ) -> WriteChunks<'a> {
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
    pub fn write_chunks<'a>(
        &'a mut self,
        chunks: &'a [Bytes],
        fin: bool,
    ) -> WriteChunks<'a> {
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
            let id = 0u64;
            let mut buffer_length = std::mem::size_of_val(&id) as u32;
            let status = self.0.shared.msquic_stream.get_param(
                msquic::PARAM_STREAM_ID,
                &mut buffer_length as *mut _,
                &id as *const _ as *const c_void,
            );
            if msquic::Status::succeeded(status) {
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
                ReadStatus::Finished(read) => Poll::Ready(Ok(read)),
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
        self
            .poll_write_generic(cx, |write_buf| {
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

    pub(crate) fn write_chunks<'a>(
        &'a self,
        chunks: &'a [Bytes],
        fin: bool,
    ) -> WriteChunks<'a> {
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
        let (buffer, buffer_count) = write_buf.get_buffer();
        match status {
            WriteStatus::Writable(val) | WriteStatus::Blocked(Some(val)) => {
                self.0
                    .shared
                    .msquic_stream
                    .send(
                        buffer,
                        buffer_count,
                        msquic::SEND_FLAG_NONE,
                        write_buf.into_raw() as *const _,
                    )
                    .unwrap();
                Poll::Ready(Ok(Some(val)))
            }
            WriteStatus::Blocked(None) => unreachable!(),
            WriteStatus::Finished(val) => {
                self.0
                    .shared
                    .msquic_stream
                    .send(
                        buffer,
                        buffer_count,
                        msquic::SEND_FLAG_FIN,
                        write_buf.into_raw() as *const _,
                    )
                    .unwrap();
                Poll::Ready(Ok(val))
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
                self.0
                    .shared
                    .msquic_stream
                    .shutdown(msquic::STREAM_SHUTDOWN_FLAG_GRACEFUL, 0);
                exclusive.send_state = StreamSendState::Shutdown;
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
                self.0
                    .shared
                    .msquic_stream
                    .shutdown(msquic::STREAM_SHUTDOWN_FLAG_ABORT_SEND, error_code);
                exclusive.send_state = StreamSendState::Shutdown;
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
                    .shutdown(msquic::STREAM_SHUTDOWN_FLAG_ABORT_SEND, error_code);
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
                return Poll::Pending;
            }
            StreamRecvState::StartComplete => {
                self.0
                    .shared
                    .msquic_stream
                    .shutdown(msquic::STREAM_SHUTDOWN_FLAG_ABORT_RECEIVE, error_code);
                exclusive.recv_state = StreamRecvState::ShutdownComplete;
                exclusive
                    .read_waiters
                    .drain(..)
                    .for_each(|waker| waker.wake());
            }
            StreamRecvState::ShutdownComplete => {
                if let Some(conn_error) = &exclusive.conn_error {
                    return Poll::Ready(Err(ReadError::ConnectionLost(conn_error.clone())));
                } else if let Some(error_code) = &exclusive.recv_error_code {
                    return Poll::Ready(Err(ReadError::Reset(*error_code)));
                } else {
                    return Poll::Ready(Ok(()));
                }
            }
            _ => {
                return Poll::Ready(Err(ReadError::Closed));
            }
        }
        Poll::Ready(Ok(()))
    }

    pub(crate) fn abort_read(&self, error_code: u64) -> Result<(), ReadError> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.recv_state {
            StreamRecvState::StartComplete => {
                self.0
                    .shared
                    .msquic_stream
                    .shutdown(msquic::STREAM_SHUTDOWN_FLAG_ABORT_RECEIVE, error_code);
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
        exclusive.recv_buffers.clear();
        match exclusive.state {
            StreamState::Start | StreamState::StartComplete => {
                trace!("StreamInstance({:p}) shutdown while dropping", &*self.0);
                self.0.shared.msquic_stream.shutdown(
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
    start_status: Option<u32>,
    recv_state: StreamRecvState,
    recv_buffers: VecDeque<StreamRecvBuffer>,
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

        let complete_len = if !buffer_range.is_empty() {
            let mut exclusive = self.exclusive.lock().unwrap();
            exclusive.read_complete_map.insert(buffer_range);
            let complete_range = exclusive.read_complete_map.first().unwrap();
            trace!(
                "StreamInner({:p}) complete read offset={} len={}",
                self,
                complete_range.start,
                complete_range.end - complete_range.start
            );
            if complete_range.start == 0 && exclusive.read_complete_cursor < complete_range.end {
                let complete_len = complete_range.end - exclusive.read_complete_cursor;
                exclusive.read_complete_cursor = complete_range.end;
                complete_len
            } else {
                0
            }
        } else {
            0
        };
        if complete_len > 0 {
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
        _stream: msquic::Handle,
        inner: &Self,
        payload: &msquic::StreamEventStartComplete,
    ) -> u32 {
        if msquic::Status::succeeded(payload.status) {
            inner.shared.id.write().unwrap().replace(payload.id);
        }
        trace!(
            "Stream({:p}, id={:?}) start complete status=0x{:x}, peer_accepted={}, id={}",
            inner,
            inner.shared.id.read(),
            payload.status,
            payload.bit_flags.peer_accepted(),
            payload.id,
        );
        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.start_status = Some(payload.status);
        if msquic::Status::succeeded(payload.status) && payload.bit_flags.peer_accepted() == 1 {
            exclusive.state = StreamState::StartComplete;
            if inner.shared.stream_type == StreamType::Bidirectional {
                exclusive.recv_state = StreamRecvState::StartComplete;
            }
            exclusive.send_state = StreamSendState::StartComplete;
        }

        if payload.status == msquic::QUIC_STATUS_STREAM_LIMIT_REACHED
            || payload.bit_flags.peer_accepted() == 1
        {
            exclusive
                .start_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
        }
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_receive(
        _stream: msquic::Handle,
        inner: &Self,
        payload: &msquic::StreamEventReceive,
    ) -> u32 {
        trace!(
            "Stream({:p}, id={:?}) Receive {} offsets {} bytes, fin {}",
            inner,
            inner.shared.id.read(),
            payload.absolute_offset,
            payload.total_buffer_length,
            (payload.flags & msquic::RECEIVE_FLAG_FIN) == msquic::RECEIVE_FLAG_FIN
        );

        let buffers =
            unsafe { std::slice::from_raw_parts(payload.buffer, payload.buffer_count as usize) };

        let arc_inner: Arc<Self> = unsafe { Arc::from_raw(inner as *const _) };

        let recv_buffer = StreamRecvBuffer::new(
            payload.absolute_offset as usize,
            &buffers,
            (payload.flags & msquic::RECEIVE_FLAG_FIN) == msquic::RECEIVE_FLAG_FIN,
        );

        let _ = Arc::into_raw(arc_inner);

        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.recv_buffers.push_back(recv_buffer);
        exclusive
            .read_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::QUIC_STATUS_PENDING
    }

    fn handle_event_send_complete(
        _stream: msquic::Handle,
        inner: &Self,
        payload: &msquic::StreamEventSendComplete,
    ) -> u32 {
        trace!(
            "Stream({:p}, id={:?}) Send complete",
            inner,
            inner.shared.id.read()
        );

        let mut write_buf = unsafe { WriteBuffer::from_raw(payload.client_context) };
        let mut exclusive = inner.exclusive.lock().unwrap();
        write_buf.reset();
        exclusive.write_pool.push(write_buf);
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_peer_send_shutdown(_stream: msquic::Handle, inner: &Self) -> u32 {
        trace!(
            "Stream({:p}, id={:?}) Peer send shutdown",
            inner,
            inner.shared.id.read()
        );
        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.recv_state = StreamRecvState::ShutdownComplete;
        exclusive
            .read_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_peer_send_aborted(
        _stream: msquic::Handle,
        inner: &Self,
        payload: &msquic::StreamEventPeerSendAborted,
    ) -> u32 {
        trace!(
            "Stream({:p}, id={:?}) Peer send aborted",
            inner,
            inner.shared.id.read()
        );
        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.recv_state = StreamRecvState::ShutdownComplete;
        exclusive.recv_error_code = Some(payload.error_code);
        exclusive
            .read_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_peer_receive_aborted(
        _stream: msquic::Handle,
        inner: &Self,
        payload: &msquic::StreamEventPeerReceiveAborted,
    ) -> u32 {
        trace!(
            "Stream({:p}, id={:?}) Peer receive aborted",
            inner,
            inner.shared.id.read()
        );
        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.send_state = StreamSendState::ShutdownComplete;
        exclusive.send_error_code = Some(payload.error_code);
        exclusive
            .write_shutdown_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_send_shutdown_complete(
        _stream: msquic::Handle,
        inner: &Self,
        _payload: &msquic::StreamEventSendShutdownComplete,
    ) -> u32 {
        trace!(
            "Stream({:p}, id={:?}) Send shutdown complete",
            inner,
            inner.shared.id.read()
        );
        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.send_state = StreamSendState::ShutdownComplete;
        exclusive
            .write_shutdown_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_shutdown_complete(
        _stream: msquic::Handle,
        inner: &Self,
        payload: &msquic::StreamEventShutdownComplete,
    ) -> u32 {
        trace!(
            "Stream({:p}, id={:?}) Shutdown complete",
            inner,
            inner.shared.id.read()
        );
        {
            let mut exclusive = inner.exclusive.lock().unwrap();
            exclusive.state = StreamState::ShutdownComplete;
            exclusive.recv_state = StreamRecvState::ShutdownComplete;
            exclusive.send_state = StreamSendState::ShutdownComplete;
            if payload.connection_shutdown {
                match (
                    payload.flags.conn_shutdown_by_app() == 1,
                    payload.flags.conn_closed_remotely() == 1,
                ) {
                    (true, true) => {
                        exclusive.conn_error = Some(ConnectionError::ShutdownByPeer(
                            payload.connection_error_code,
                        ));
                    }
                    (true, false) => {
                        exclusive.conn_error = Some(ConnectionError::ShutdownByLocal);
                    }
                    (false, true) | (false, false) => {
                        exclusive.conn_error = Some(ConnectionError::ShutdownByTransport(
                            payload.connection_close_status,
                            payload.connection_error_code,
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
            Arc::from_raw(inner as *const _);
        }
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_ideal_send_buffer_size(
        _stream: msquic::Handle,
        inner: &Self,
        _payload: &msquic::StreamEventIdealSendBufferSize,
    ) -> u32 {
        trace!(
            "Stream({:p}, id={:?}) Ideal send buffer size",
            inner,
            inner.shared.id.read()
        );
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_peer_accepted(_stream: msquic::Handle, inner: &Self) -> u32 {
        trace!(
            "Stream({:p}, id={:?}) Peer accepted",
            inner,
            inner.shared.id.read()
        );
        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.state = StreamState::StartComplete;
        if inner.shared.stream_type == StreamType::Bidirectional {
            exclusive.recv_state = StreamRecvState::StartComplete;
        }
        exclusive.send_state = StreamSendState::StartComplete;
        exclusive
            .start_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::QUIC_STATUS_SUCCESS
    }

    extern "C" fn native_callback(
        stream: msquic::Handle,
        context: *mut c_void,
        event: &msquic::StreamEvent,
    ) -> u32 {
        let inner = unsafe { &*(context as *const Self) };

        match event.event_type {
            msquic::STREAM_EVENT_START_COMPLETE => {
                Self::handle_event_start_complete(stream, inner, unsafe {
                    &event.payload.start_complete
                })
            }
            msquic::STREAM_EVENT_RECEIVE => {
                Self::handle_event_receive(stream, inner, unsafe { &event.payload.receive })
            }
            msquic::STREAM_EVENT_SEND_COMPLETE => {
                Self::handle_event_send_complete(stream, inner, unsafe {
                    &event.payload.send_complete
                })
            }
            msquic::STREAM_EVENT_PEER_SEND_SHUTDOWN => {
                Self::handle_event_peer_send_shutdown(stream, inner)
            }
            msquic::STREAM_EVENT_PEER_SEND_ABORTED => {
                Self::handle_event_peer_send_aborted(stream, inner, unsafe {
                    &event.payload.peer_send_aborted
                })
            }
            msquic::STREAM_EVENT_PEER_RECEIVE_ABORTED => {
                Self::handle_event_peer_receive_aborted(stream, inner, unsafe {
                    &event.payload.peer_receive_aborted
                })
            }
            msquic::STREAM_EVENT_SEND_SHUTDOWN_COMPLETE => {
                Self::handle_event_send_shutdown_complete(stream, inner, unsafe {
                    &event.payload.send_shutdown_complete
                })
            }
            msquic::STREAM_EVENT_SHUTDOWN_COMPLETE => {
                Self::handle_event_shutdown_complete(stream, inner, unsafe {
                    &event.payload.shutdown_complete
                })
            }
            msquic::STREAM_EVENT_IDEAL_SEND_BUFFER_SIZE => {
                Self::handle_event_ideal_send_buffer_size(stream, inner, unsafe {
                    &event.payload.ideal_send_buffer_size
                })
            }
            msquic::STREAM_EVENT_PEER_ACCEPTED => Self::handle_event_peer_accepted(stream, inner),
            _ => {
                trace!("Stream({:p}) Other callback {}", inner, event.event_type);
                msquic::QUIC_STATUS_SUCCESS
            }
        }
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

pub struct WriteChunk<'a> {
    stream: &'a StreamInstance,
    chunk: &'a Bytes,
    fin: bool,
}

impl Future for WriteChunk<'_> {
    type Output = Result<usize, WriteError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.stream
            .poll_write_chunk(cx, self.chunk, self.fin)
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
        self.stream
            .poll_write_chunks(cx, self.chunks, self.fin)
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

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StartError {
    #[error("connection not started yet")]
    ConnectionNotStarted,
    #[error("reach stream count limit")]
    LimitReached,
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
    #[error("unknown error {0}")]
    Unknown(u32),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReadError {
    #[error("stream not opened for reading")]
    Closed,
    #[error("stream reset by peer: error {0}")]
    Reset(u64),
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
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
        };
        Self::new(kind, e)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WriteError {
    #[error("stream not opened for writing")]
    Closed,
    #[error("stream finished")]
    Finished,
    #[error("stream stopped by peer: error {0}")]
    Stopped(u64),
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
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
        };
        Self::new(kind, e)
    }
}
