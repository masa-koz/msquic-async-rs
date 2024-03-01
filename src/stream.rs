use crate::connection::ConnectionError;

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};

use bytes::Bytes;

use futures::io::{AsyncRead, AsyncWrite};
use futures::ready;

use libc::c_void;

use thiserror::Error;

#[derive(Debug, PartialEq)]
pub enum StreamType {
    Bidirectional,
    Unidirectional,
}

pub struct Stream(Box<StreamInner>);

struct StreamInner {
    exclusive: Mutex<StreamInnerExclusive>,
    shared: StreamInnerShared,
}

struct StreamInnerExclusive {
    msquic_stream: msquic::Stream,
    state: StreamState,
    start_status: Option<u32>,
    recv_state: StreamRecvState,
    recv_buffers: VecDeque<StreamRecvBuffer>,
    send_state: StreamSendState,
    write_pool: Vec<StreamWriteBuffer>,
    recv_error_code: Option<u64>,
    send_error_code: Option<u64>,
    conn_error: Option<ConnectionError>,
    start_waiters: Vec<Waker>,
    read_waiters: Vec<Waker>,
    write_shutdown_waiters: Vec<Waker>,
}

unsafe impl Sync for StreamInnerExclusive {}
unsafe impl Send for StreamInnerExclusive {}

struct StreamInnerShared {
    stream_type: StreamType,
    shutdown_complete: Arc<(Mutex<bool>, Condvar)>,
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

impl Stream {
    pub(crate) fn open(msquic_conn: &msquic::Connection, stream_type: StreamType) -> Self {
        let msquic_stream = msquic::Stream::new(&*crate::MSQUIC_API as *const _ as *const c_void);
        let flags = if stream_type == StreamType::Unidirectional {
            msquic::STREAM_OPEN_FLAG_UNIDIRECTIONAL
        } else {
            0
        };
        let inner = Box::new(StreamInner {
            exclusive: Mutex::new(StreamInnerExclusive {
                msquic_stream,
                state: StreamState::Open,
                start_status: None,
                recv_state: StreamRecvState::Closed,
                recv_buffers: VecDeque::new(),
                send_state: StreamSendState::Closed,
                write_pool: Vec::new(),
                recv_error_code: None,
                send_error_code: None,
                conn_error: None,
                start_waiters: Vec::new(),
                read_waiters: Vec::new(),
                write_shutdown_waiters: Vec::new(),
            }),
            shared: StreamInnerShared {
                stream_type,
                shutdown_complete: Arc::new((Mutex::new(false), Condvar::new())),
            },
        });
        {
            let exclusive = inner.exclusive.lock().unwrap();
            exclusive.msquic_stream.open(
                msquic_conn,
                flags,
                Self::native_callback,
                &*inner as *const _ as *const c_void,
            );
        }
        Self(inner)
    }

    pub(crate) fn from_handle(stream: msquic::Handle, stream_type: StreamType) -> Self {
        let msquic_stream = msquic::Stream::from_parts(stream, &*crate::MSQUIC_API);
        let send_state = if stream_type == StreamType::Bidirectional {
            StreamSendState::StartComplete
        } else {
            StreamSendState::Closed
        };
        let inner = Box::new(StreamInner {
            exclusive: Mutex::new(StreamInnerExclusive {
                msquic_stream,
                state: StreamState::StartComplete,
                start_status: None,
                recv_state: StreamRecvState::StartComplete,
                recv_buffers: VecDeque::new(),
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
                stream_type,
                shutdown_complete: Arc::new((Mutex::new(false), Condvar::new())),
            },
        });
        {
            let exclusive = inner.exclusive.lock().unwrap();
            exclusive
                .msquic_stream
                .set_callback_handler(Self::native_callback, &*inner as *const _ as *const c_void);
        }
        Self(inner)
    }

    pub(crate) fn poll_start(
        &mut self,
        cx: &mut Context,
        failed_on_block: bool,
    ) -> Poll<Result<(), StartError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            StreamState::Open => {
                exclusive.msquic_stream.start(
                    msquic::STREAM_START_FLAG_SHUTDOWN_ON_FAIL
                        | msquic::STREAM_START_FLAG_INDICATE_PEER_ACCEPT
                        | if failed_on_block {
                            msquic::STREAM_START_FLAG_FAIL_BLOCKED
                        } else {
                            msquic::STREAM_START_FLAG_NONE
                        },
                );
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
                        0x80410008 /* QUIC_STATUS_STREAM_LIMIT_REACHED */ => StartError::LimitReached,
                        0x80004004 /* QUIC_STATUS_ABORTED */ |
                        0x8007139f /* QUIC_STATUS_INVALID_STATE */ => if let Some(error) = &exclusive.conn_error {
                            StartError::ConnectionLost(error.clone())
                        } else {
                            StartError::ConnectionClosed
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

    pub fn close(&self) {
        let exclusive = self.0.exclusive.lock().unwrap();
        exclusive.msquic_stream.close();
    }

    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, ReadError>> {
        self.poll_read_generic(cx, |recv_buffers| {
            let mut read = 0;
            let mut fin = false;
            loop {
                if read == buf.len() {
                    return ReadStatus::Readable(read);
                }

                match recv_buffers
                    .front_mut()
                    .and_then(|x| x.next(buf.len() - read))
                {
                    Some(slice) => {
                        let len = slice.len();
                        buf[read..read + len].copy_from_slice(slice);
                        read += len;
                    }
                    None => {
                        if let Some(recv_buffer) = recv_buffers.pop_front() {
                            fin = recv_buffer.fin();
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

    fn poll_read_generic<T, U>(
        &mut self,
        cx: &mut Context<'_>,
        mut read_fn: T,
    ) -> Poll<Result<Option<U>, ReadError>>
    where
        T: FnMut(&mut VecDeque<StreamRecvBuffer>) -> ReadStatus<U>,
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
                } else {
                    if let Some(error_code) = &exclusive.recv_error_code {
                        return Poll::Ready(Err(ReadError::Reset(*error_code)));
                    } else {
                        return Poll::Ready(Ok(None));
                    }
                }
            }
        }

        let status = read_fn(&mut exclusive.recv_buffers);

        match status {
            ReadStatus::Readable(read) | ReadStatus::Blocked(Some(read)) => {
                Poll::Ready(Ok(Some(read)))
            }
            ReadStatus::Finished(read) => Poll::Ready(Ok(read)),
            ReadStatus::Blocked(None) => {
                exclusive.read_waiters.push(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    pub fn poll_write(
        &mut self,
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

    pub fn zerocopy_write<'a>(&'a self, buf: &'a Bytes, fin: bool) -> ZeroCopyWrite<'a> {
        ZeroCopyWrite {
            stream: &self,
            buf,
            fin,
        }
    }

    pub fn zerocopy_write_vectored<'a>(
        &'a self,
        bufs: &'a [Bytes],
        fin: bool,
    ) -> ZeroCopyWriteVectored<'a> {
        ZeroCopyWriteVectored {
            stream: &self,
            bufs,
            fin,
        }
    }

    fn poll_write_generic<T, U>(
        &self,
        _cx: &mut Context<'_>,
        mut write_fn: T,
    ) -> Poll<Result<Option<U>, WriteError>>
    where
        T: FnMut(&mut StreamWriteBuffer) -> WriteStatus<U>,
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
                } else {
                    if let Some(error_code) = &exclusive.send_error_code {
                        return Poll::Ready(Err(WriteError::Stopped(*error_code)));
                    } else {
                        return Poll::Ready(Err(WriteError::Finished));
                    }
                }
            }
        }
        let mut write_buf = exclusive
            .write_pool
            .pop()
            .unwrap_or_else(|| StreamWriteBuffer::new());
        let status = write_fn(&mut write_buf);
        let (buffer, buffer_count) = write_buf.get_buffer();
        match status {
            WriteStatus::Writable(val) | WriteStatus::Blocked(Some(val)) => {
                exclusive.msquic_stream.send(
                    buffer,
                    buffer_count,
                    msquic::SEND_FLAG_NONE,
                    write_buf.into_raw() as *const _ as *const c_void,
                );
                Poll::Ready(Ok(Some(val)))
            }
            WriteStatus::Blocked(None) => unreachable!(),
            WriteStatus::Finished(val) => {
                exclusive.msquic_stream.send(
                    buffer,
                    buffer_count,
                    msquic::SEND_FLAG_FIN,
                    write_buf.into_raw() as *const _ as *const c_void,
                );
                Poll::Ready(Ok(val))
            }
        }
    }

    pub fn poll_finish_write(&self, cx: &mut Context<'_>) -> Poll<Result<(), WriteError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.send_state {
            StreamSendState::Start => {
                exclusive.start_waiters.push(cx.waker().clone());
                return Poll::Pending;
            }
            StreamSendState::StartComplete => {
                exclusive
                    .msquic_stream
                    .shutdown(msquic::STREAM_SHUTDOWN_FLAG_GRACEFUL, 0);
                exclusive.send_state = StreamSendState::Shutdown;
            }
            StreamSendState::Shutdown => {}
            StreamSendState::ShutdownComplete => {
                if let Some(conn_error) = &exclusive.conn_error {
                    return Poll::Ready(Err(WriteError::ConnectionLost(conn_error.clone())));
                } else {
                    if let Some(error_code) = &exclusive.send_error_code {
                        return Poll::Ready(Err(WriteError::Stopped(*error_code)));
                    } else {
                        return Poll::Ready(Ok(()));
                    }
                }
            }
            _ => {
                return Poll::Ready(Err(WriteError::Closed));
            }
        }
        exclusive.write_shutdown_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    pub fn poll_abort_write(
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
                exclusive
                    .msquic_stream
                    .shutdown(msquic::STREAM_SHUTDOWN_FLAG_ABORT_SEND, error_code);
                exclusive.send_state = StreamSendState::Shutdown;
            }
            StreamSendState::Shutdown => {}
            StreamSendState::ShutdownComplete => {
                if let Some(conn_error) = &exclusive.conn_error {
                    return Poll::Ready(Err(WriteError::ConnectionLost(conn_error.clone())));
                } else {
                    if let Some(error_code) = &exclusive.send_error_code {
                        return Poll::Ready(Err(WriteError::Stopped(*error_code)));
                    } else {
                        return Poll::Ready(Ok(()));
                    }
                }
            }
            _ => {
                return Poll::Ready(Err(WriteError::Closed));
            }
        }
        exclusive.write_shutdown_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    pub fn poll_abort_read(
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
                exclusive
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
                } else {
                    if let Some(error_code) = &exclusive.recv_error_code {
                        return Poll::Ready(Err(ReadError::Reset(*error_code)));
                    } else {
                        return Poll::Ready(Ok(()));
                    }
                }
            }
            _ => {
                return Poll::Ready(Err(ReadError::Closed));
            }
        }
        Poll::Ready(Ok(()))
    }

    fn handle_event_start_complete(
        _stream: msquic::Handle,
        inner: &StreamInner,
        payload: &msquic::StreamEventStartComplete,
    ) -> u32 {
        println!(
            "Stream start complete status=0x{:x}, peer_accepted={}",
            payload.status,
            payload.bit_flags.peer_accepted()
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

        if msquic::Status::failed(payload.status) || payload.bit_flags.peer_accepted() == 1 {
            exclusive
                .start_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
        }
        0
    }

    fn handle_event_receive(
        stream: msquic::Handle,
        inner: &StreamInner,
        payload: &msquic::StreamEventReceive,
    ) -> u32 {
        let buffers =
            unsafe { std::slice::from_raw_parts(payload.buffer, payload.buffer_count as usize) };
        let fin = (payload.flags & msquic::RECEIVE_FLAG_FIN) == msquic::RECEIVE_FLAG_FIN;
        let recv_buffer = unsafe { StreamRecvBuffer::new(&buffers, fin, Some(stream)) };
        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.recv_buffers.push_back(recv_buffer);
        exclusive
            .read_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        println!("Receive {} bytes, fin {}", payload.total_buffer_length, fin);
        0x703e5
    }

    fn handle_event_send_complete(
        _stream: msquic::Handle,
        inner: &StreamInner,
        payload: &msquic::StreamEventSendComplete,
    ) -> u32 {
        println!("Send complete");
        let mut write_buf =
            unsafe { StreamWriteBuffer::from_raw(payload.client_context as *mut _) };
        //write_buf.send_zerocopy();
        let mut exclusive = inner.exclusive.lock().unwrap();
        write_buf.reset();
        exclusive.write_pool.push(write_buf);
        0
    }

    fn handle_event_peer_send_shutdown(_stream: msquic::Handle, inner: &StreamInner) -> u32 {
        println!("Peer send shutdown");
        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.recv_state = StreamRecvState::ShutdownComplete;
        exclusive
            .read_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        0
    }

    fn handle_event_peer_send_aborted(
        _stream: msquic::Handle,
        inner: &StreamInner,
        payload: &msquic::StreamEventPeerSendAborted,
    ) -> u32 {
        println!("Peer send aborted");
        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.recv_state = StreamRecvState::ShutdownComplete;
        exclusive.recv_error_code = Some(payload.error_code);
        exclusive
            .read_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        0
    }

    fn handle_event_peer_receive_aborted(
        _stream: msquic::Handle,
        inner: &StreamInner,
        payload: &msquic::StreamEventPeerReceiveAborted,
    ) -> u32 {
        println!("Peer receive aborted");
        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.send_state = StreamSendState::ShutdownComplete;
        exclusive.send_error_code = Some(payload.error_code);
        exclusive
            .write_shutdown_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        0
    }

    fn handle_event_send_shutdown_complete(
        _stream: msquic::Handle,
        inner: &StreamInner,
        _payload: &msquic::StreamEventSendShutdownComplete,
    ) -> u32 {
        println!("Send shutdown complete");
        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.send_state = StreamSendState::ShutdownComplete;
        exclusive
            .write_shutdown_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        0
    }

    fn handle_event_shutdown_complete(
        _stream: msquic::Handle,
        inner: &StreamInner,
        payload: &msquic::StreamEventShutdownComplete,
    ) -> u32 {
        println!("Stream shutdown complete");
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
        let (lock, cvar) = &*inner.shared.shutdown_complete;
        let mut shutdown_complete = lock.lock().unwrap();
        *shutdown_complete = true;
        cvar.notify_one();
        0
    }

    fn handle_event_ideal_send_buffer_size(
        _stream: msquic::Handle,
        _inner: &StreamInner,
        _payload: &msquic::StreamEventIdealSendBufferSize,
    ) -> u32 {
        println!("Ideal send buffer size");
        0
    }

    fn handle_event_peer_accepted(_stream: msquic::Handle, inner: &StreamInner) -> u32 {
        println!("Peer accepted");
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
        0
    }

    extern "C" fn native_callback(
        stream: msquic::Handle,
        context: *mut c_void,
        event: &msquic::StreamEvent,
    ) -> u32 {
        println!("Stream event 0x{:x}", event.event_type);
        let inner = unsafe { &*(context as *const StreamInner) };
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
                println!("Other callback {}", event.event_type);
                0
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        println!("Stream dropped");
        {
            let exclusive = self.0.exclusive.lock().unwrap();
            if exclusive.state != StreamState::ShutdownComplete {
                exclusive.msquic_stream.shutdown(
                    msquic::STREAM_SHUTDOWN_FLAG_ABORT_SEND
                        | msquic::STREAM_SHUTDOWN_FLAG_ABORT_RECEIVE
                        | msquic::STREAM_SHUTDOWN_FLAG_IMMEDIATE,
                    0,
                );
            }
        }
        let (lock, cvar) = &*self.0.shared.shutdown_complete;
        let mut shutdown_complete = lock.lock().unwrap();
        while !*shutdown_complete {
            shutdown_complete = cvar.wait(shutdown_complete).unwrap();
        }
        println!("Stream shutdown complete finished");
    }
}

pub struct ZeroCopyWrite<'a> {
    stream: &'a Stream,
    buf: &'a Bytes,
    fin: bool,
}

impl Future for ZeroCopyWrite<'_> {
    type Output = Result<usize, WriteError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.stream
            .poll_write_generic(cx, |write_buf| {
                let written = write_buf.put_zerocopy(self.buf);
                if written == self.buf.len() && !self.fin {
                    WriteStatus::Writable(written)
                } else {
                    (Some(written), self.fin).into()
                }
            })
            .map(|res| res.map(|x| x.unwrap_or(0)))
    }
}

pub struct ZeroCopyWriteVectored<'a> {
    stream: &'a Stream,
    bufs: &'a [Bytes],
    fin: bool,
}

impl Future for ZeroCopyWriteVectored<'_> {
    type Output = Result<usize, WriteError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.stream
            .poll_write_generic(cx, |write_buf| {
                let (mut total_len, mut total_written) = (0, 0);
                for buf in self.bufs {
                    total_len += buf.len();
                    total_written += write_buf.put_zerocopy(buf);
                }
                if total_written == total_len && !self.fin {
                    WriteStatus::Writable(total_written)
                } else {
                    (Some(total_written), self.fin).into()
                }
            })
            .map(|res| res.map(|x| x.unwrap_or(0)))
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let len = ready!(Stream::poll_read(self.get_mut(), cx, buf))?;
        Poll::Ready(Ok(len))
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let len = ready!(Stream::poll_write(self.get_mut(), cx, buf, false))?;
        Poll::Ready(Ok(len))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let _ = ready!(Stream::poll_finish_write(self.get_mut(), cx))?;
        Poll::Ready(Ok(()))
    }
}

struct StreamRecvBuffer {
    handle: Option<msquic::Handle>,
    buffers: Vec<msquic::Buffer>,
    total_length: usize,
    read: usize,
    fin: bool,
}

impl StreamRecvBuffer {
    pub(crate) unsafe fn new<T: AsRef<[msquic::Buffer]>>(
        buffers: &T,
        fin: bool,
        handle: Option<msquic::Handle>,
    ) -> Self {
        let total_length: u32 = buffers.as_ref().iter().map(|x| x.length).sum();
        Self {
            handle,
            buffers: buffers.as_ref().to_vec(),
            total_length: total_length as usize,
            read: 0,
            fin,
        }
    }

    fn next<'a>(&mut self, max_length: usize) -> Option<&'a [u8]> {
        if self.read >= self.total_length {
            return None;
        }
        let mut skip = self.read;
        for buffer in &self.buffers {
            if (buffer.length as usize) <= skip {
                skip -= buffer.length as usize;
                continue;
            }
            let len = std::cmp::min(buffer.length as usize - skip, max_length);
            self.read += len;
            let slice = unsafe { std::slice::from_raw_parts(buffer.buffer.add(skip), len) };
            return Some(slice);
        }
        None
    }

    fn fin(&self) -> bool {
        self.fin
    }
}

impl Drop for StreamRecvBuffer {
    fn drop(&mut self) {
        println!("StreamRecvBuffer dropped");
        if let Some(handle) = self.handle.take() {
            println!(
                "stream_receive_complete self.total_length: {}",
                self.total_length
            );
            crate::MSQUIC_API.stream_receive_complete(handle, self.total_length as u64);
        }
    }
}

struct StreamWriteBuffer(Box<StreamWriteBufferInner>);

struct StreamWriteBufferInner {
    internal: Vec<u8>,
    zerocopy: Vec<Bytes>,
    msquic_buffer: Vec<msquic::Buffer>,
}

impl StreamWriteBuffer {
    fn new() -> Self {
        StreamWriteBuffer(Box::new(StreamWriteBufferInner {
            internal: Vec::new(),
            zerocopy: Vec::new(),
            msquic_buffer: Vec::new(),
        }))
    }

    unsafe fn from_raw(inner: *mut StreamWriteBufferInner) -> Self {
        StreamWriteBuffer(unsafe { Box::from_raw(inner) })
    }

    fn put_zerocopy(&mut self, buf: &Bytes) -> usize {
        self.0.zerocopy.push(buf.clone());
        buf.len()
    }

    fn put_slice(&mut self, slice: &[u8]) -> usize {
        self.0.internal.extend_from_slice(slice);
        slice.len()
    }

    fn get_buffer(&mut self) -> (*const msquic::Buffer, u32) {
        if !self.0.zerocopy.is_empty() {
            for buf in &self.0.zerocopy {
                self.0.msquic_buffer.push(buf[..].into());
            }
        } else {
            self.0.msquic_buffer.push((&self.0.internal).into());
        }
        let ptr = self.0.msquic_buffer.as_ptr();
        let len = self.0.msquic_buffer.len() as u32;
        (ptr, len)
    }

    fn into_raw(self) -> *mut StreamWriteBufferInner {
        Box::into_raw(self.0)
    }

    fn reset(&mut self) {
        self.0.internal.clear();
        self.0.zerocopy.clear();
        self.0.msquic_buffer.clear();
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
    #[error("connection closed")]
    ConnectionClosed,
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
    #[error("reach stream count limit")]
    LimitReached,
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
    #[error("stream stopped by peer: error {0}")]
    Stopped(u64),
    #[error("stream finished")]
    Finished,
    #[error("stream not opened for writing")]
    Closed,
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