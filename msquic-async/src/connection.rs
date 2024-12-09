use crate::buffer::WriteBuffer;
use crate::stream::{StartError as StreamStartError, Stream, ReadStream, StreamType};

use std::collections::VecDeque;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use libc::c_void;
use thiserror::Error;
use tracing::trace;

#[derive(Clone)]
pub struct Connection(Arc<ConnectionInstance>);

impl Connection {
    /// Create a new connection.
    ///
    /// The connection is not started until `start` is called.
    pub fn new(msquic_conn: msquic::Connection, registration: &msquic::Registration, api: &msquic::Api) -> Self {
        let inner = Arc::new(ConnectionInner::new(msquic_conn, api));
        inner.shared.msquic_conn.open(
            registration,
            ConnectionInner::native_callback,
            Arc::into_raw(inner.clone()) as *const c_void,
        ).unwrap();
        trace!("Connection({:p}) Open by local", &*inner);
        Self(Arc::new(ConnectionInstance(inner)))
    }

    pub(crate) fn from_handle(conn: msquic::Handle, api: &msquic::Api) -> Self {
        let msquic_conn = msquic::Connection::from_parts(conn, api);
        let inner = Arc::new(ConnectionInner::new(msquic_conn, api));
        inner.shared.msquic_conn.set_callback_handler(
            ConnectionInner::native_callback,
            Arc::into_raw(inner.clone()) as *const c_void,
        );
        trace!("Connection({:p}) Open by peer", &*inner);
        Self(Arc::new(ConnectionInstance(inner)))
    }

    /// Start the connection.
    pub fn start<'a>(
        &'a self,
        configuration: &'a msquic::Configuration,
        host: &'a str,
        port: u16,
    ) -> ConnectionStart<'a> {
        ConnectionStart {
            conn: &self,
            configuration,
            host,
            port,
        }
    }

    /// Poll to start the connection.
    fn poll_start(
        &self,
        cx: &mut Context<'_>,
        configuration: &msquic::Configuration,
        host: &str,
        port: u16,
    ) -> Poll<Result<(), StartError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                self.0.shared.msquic_conn.start(configuration, host, port).unwrap();
                exclusive.state = ConnectionState::Connecting;
            }
            ConnectionState::Connecting => {}
            ConnectionState::Connected => return Poll::Ready(Ok(())),
            ConnectionState::Shutdown | ConnectionState::ShutdownComplete => {
                return Poll::Ready(Err(StartError::ConnectionLost(
                    exclusive.error.as_ref().expect("error").clone(),
                )));
            }
        }
        exclusive.start_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    pub(crate) fn set_configuration(&self, configuration: &msquic::Configuration) {
        self.0.shared.msquic_conn.set_configuration(configuration).unwrap();
    }

    /// Open a new outbound stream.
    pub fn open_outbound_stream(
        &self,
        stream_type: StreamType,
        fail_on_blocked: bool,
    ) -> OpenOutboundStream<'_> {
        OpenOutboundStream {
            conn: &self.0,
            stream_type: Some(stream_type),
            stream: None,
            fail_on_blocked,
        }
    }

    /// Accept an inbound bidilectional stream.
    pub fn accept_inbound_stream(&self) -> AcceptInboundStream<'_> {
        AcceptInboundStream { conn: &self }
    }

    /// Poll to accept an inbound bidilectional stream.
    pub fn poll_accept_inbound_stream(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Stream, StreamStartError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                return Poll::Ready(Err(StreamStartError::ConnectionNotStarted));
            }
            ConnectionState::Connecting => {
                exclusive.start_waiters.push(cx.waker().clone());
                return Poll::Pending;
            }
            ConnectionState::Connected => {}
            ConnectionState::Shutdown | ConnectionState::ShutdownComplete => {
                return Poll::Ready(Err(StreamStartError::ConnectionLost(
                    exclusive.error.as_ref().expect("error").clone(),
                )));
            }
        }

        if !exclusive.inbound_streams.is_empty() {
            return Poll::Ready(Ok(exclusive.inbound_streams.pop_front().unwrap()));
        }
        exclusive.inbound_stream_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    /// Accept an inbound unidirectional stream.
    pub fn accept_inbound_uni_stream(&self) -> AcceptInboundUniStream<'_> {
        AcceptInboundUniStream { conn: &self }
    }

    /// Poll to accept an inbound unidirectional stream.
    pub fn poll_accept_inbound_uni_stream(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<ReadStream, StreamStartError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                return Poll::Ready(Err(StreamStartError::ConnectionNotStarted));
            }
            ConnectionState::Connecting => {
                exclusive.start_waiters.push(cx.waker().clone());
                return Poll::Pending;
            }
            ConnectionState::Connected => {}
            ConnectionState::Shutdown | ConnectionState::ShutdownComplete => {
                return Poll::Ready(Err(StreamStartError::ConnectionLost(
                    exclusive.error.as_ref().expect("error").clone(),
                )));
            }
        }

        if !exclusive.inbound_uni_streams.is_empty() {
            return Poll::Ready(Ok(exclusive.inbound_uni_streams.pop_front().unwrap()));
        }
        exclusive.inbound_uni_stream_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    /// Poll to receive a datagram.
    pub fn poll_receive_datagram(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Bytes, DgramReceiveError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                return Poll::Ready(Err(DgramReceiveError::ConnectionNotStarted));
            }
            ConnectionState::Connecting => {
                exclusive.start_waiters.push(cx.waker().clone());
                return Poll::Pending;
            }
            ConnectionState::Connected => {}
            ConnectionState::Shutdown | ConnectionState::ShutdownComplete => {
                return Poll::Ready(Err(DgramReceiveError::ConnectionLost(
                    exclusive.error.as_ref().expect("error").clone(),
                )));
            }
        }
        
        if let Some(buf) = exclusive.recv_buffers.pop_front() {
            Poll::Ready(Ok(buf))
        } else {
            exclusive.recv_waiters.push(cx.waker().clone());
            return Poll::Pending
        }
    }

    /// Poll to send a datagram.
    pub fn poll_send_datagram(
        &self,
        cx: &mut Context<'_>,
        buf: &Bytes,
    ) -> Poll<Result<(), DgramSendError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                return Poll::Ready(Err(DgramSendError::ConnectionNotStarted));
            }
            ConnectionState::Connecting => {
                exclusive.start_waiters.push(cx.waker().clone());
                return Poll::Pending;
            }
            ConnectionState::Connected => {}
            ConnectionState::Shutdown | ConnectionState::ShutdownComplete => {
                return Poll::Ready(Err(DgramSendError::ConnectionLost(
                    exclusive.error.as_ref().expect("error").clone(),
                )));
            }
        }

        let mut write_buf = exclusive
            .write_pool
            .pop()
            .unwrap_or_else(|| WriteBuffer::new());
        let _ = write_buf.put_zerocopy(buf);
        let (buffer, buffer_count) = write_buf.get_buffer();
        self.0.shared.msquic_conn.datagram_send(
            buffer,
            buffer_count,
            msquic::SEND_FLAG_NONE,
            write_buf.into_raw() as *const _ as *const c_void,
        ).unwrap();
        Poll::Ready(Ok(()))
    }

    /// Send a datagram.
    pub fn send_datagram(
        &self,
        buf: &Bytes,
    ) -> Result<(), DgramSendError> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                return Err(DgramSendError::ConnectionNotStarted);
            }
            ConnectionState::Connecting => {
                return Err(DgramSendError::ConnectionNotStarted);
            }
            ConnectionState::Connected => {}
            ConnectionState::Shutdown | ConnectionState::ShutdownComplete => {
                return Err(DgramSendError::ConnectionLost(
                    exclusive.error.as_ref().expect("error").clone(),
                ));
            }
        }

        let mut write_buf = exclusive
            .write_pool
            .pop()
            .unwrap_or_else(|| WriteBuffer::new());
        let _ = write_buf.put_zerocopy(buf);
        let (buffer, buffer_count) = write_buf.get_buffer();
        self.0.shared.msquic_conn.datagram_send(
            buffer,
            buffer_count,
            msquic::SEND_FLAG_NONE,
            write_buf.into_raw() as *const _ as *const c_void,
        ).unwrap();
        Ok(())
    }

    /// Poll to shutdown the connection.
    pub fn poll_shutdown(
        &self,
        cx: &mut Context<'_>,
        error_code: u64,
    ) -> Poll<Result<(), ShutdownError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                return Poll::Ready(Err(ShutdownError::ConnectionNotStarted));
            }
            ConnectionState::Connecting => {
                exclusive.start_waiters.push(cx.waker().clone());
                return Poll::Pending;
            }
            ConnectionState::Connected => {
                self.0
                    .shared
                    .msquic_conn
                    .shutdown(msquic::CONNECTION_SHUTDOWN_FLAG_NONE, error_code);
                exclusive.state = ConnectionState::Shutdown;
            }
            ConnectionState::Shutdown => {}
            ConnectionState::ShutdownComplete => {
                if let Some(ConnectionError::ShutdownByLocal) = &exclusive.error {
                    return Poll::Ready(Ok(()));
                } else {
                    return Poll::Ready(Err(ShutdownError::ConnectionLost(
                        exclusive.error.as_ref().expect("error").clone(),
                    )));
                }
            }
        }

        exclusive.shutdown_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    /// Shutdown the connection.
    pub fn shutdown(
        &self,
        error_code: u64,
    ) -> Result<(), ShutdownError> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open | ConnectionState::Connecting => {
                return Err(ShutdownError::ConnectionNotStarted);
            }
            ConnectionState::Connected => {
                self.0
                    .shared
                    .msquic_conn
                    .shutdown(msquic::CONNECTION_SHUTDOWN_FLAG_NONE, error_code);
                exclusive.state = ConnectionState::Shutdown;
            }
            _ => {},
        }
        Ok(())
    }
}

struct ConnectionInstance(Arc<ConnectionInner>);

impl Deref for ConnectionInstance {
    type Target = ConnectionInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for ConnectionInstance {
    fn drop(&mut self) {
        trace!("Connection({:p}) dropping", &*self.0);
        {
            let exclusive = self.0.exclusive.lock().unwrap();
            match exclusive.state {
                ConnectionState::Open
                | ConnectionState::Connecting
                | ConnectionState::Connected => {
                    trace!("Connection({:p}) shutdown while dropping", &*self.0);
                    self.0
                        .shared
                        .msquic_conn
                        .shutdown(msquic::CONNECTION_SHUTDOWN_FLAG_NONE, 0);
                }
                ConnectionState::Shutdown | ConnectionState::ShutdownComplete => {}
            }
        }
    }
}

struct ConnectionInner {
    exclusive: Mutex<ConnectionInnerExclusive>,
    shared: ConnectionInnerShared,
}

struct ConnectionInnerExclusive {
    state: ConnectionState,
    error: Option<ConnectionError>,
    start_waiters: Vec<Waker>,
    inbound_stream_waiters: Vec<Waker>,
    inbound_uni_stream_waiters: Vec<Waker>,
    inbound_streams: VecDeque<crate::stream::Stream>,
    inbound_uni_streams: VecDeque<crate::stream::ReadStream>,
    recv_buffers: VecDeque<Bytes>,
    recv_waiters: Vec<Waker>,
    write_pool: Vec<WriteBuffer>,
    shutdown_waiters: Vec<Waker>,
}

struct ConnectionInnerShared {
    msquic_conn: msquic::Connection,
    msquic_api: msquic::Api,
}

impl ConnectionInner {
    fn new(msquic_conn: msquic::Connection, api: &msquic::Api) -> Self {
        ConnectionInner {
            exclusive: Mutex::new(ConnectionInnerExclusive {
                state: ConnectionState::Open,
                error: None,
                start_waiters: Vec::new(),
                inbound_stream_waiters: Vec::new(),
                inbound_uni_stream_waiters: Vec::new(),
                inbound_streams: VecDeque::new(),
                inbound_uni_streams: VecDeque::new(),
                recv_buffers: VecDeque::new(),
                recv_waiters: Vec::new(),
                write_pool: Vec::new(),
                shutdown_waiters: Vec::new(),
            }),
            shared: ConnectionInnerShared {
                msquic_conn,
                msquic_api: api.clone(),
            },
        }
    }

    fn handle_event_connected(
        inner: &ConnectionInner,
        _payload: &msquic::ConnectionEventConnected,
    ) -> u32 {
        trace!("Connection({:p}) Connected", inner);

        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.state = ConnectionState::Connected;
        exclusive
            .start_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_shutdown_initiated_by_transport(
        inner: &ConnectionInner,
        payload: &msquic::ConnectionEventConnectionShutdownByTransport,
    ) -> u32 {
        trace!(
            "Connection({:p}) Transport shutdown 0x{:x}",
            inner, payload.status
        );

        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.state = ConnectionState::Shutdown;
        exclusive.error = Some(ConnectionError::ShutdownByTransport(
            payload.status,
            payload.error_code,
        ));
        exclusive
            .start_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        exclusive
            .inbound_stream_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_shutdown_initiated_by_peer(
        inner: &ConnectionInner,
        payload: &msquic::ConnectionEventConnectionShutdownByPeer,
    ) -> u32 {
        trace!(
            "Connection({:p}) App shutdown {}",
            inner, payload.error_code
        );

        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.state = ConnectionState::Shutdown;
        exclusive.error = Some(ConnectionError::ShutdownByPeer(payload.error_code));
        exclusive
            .start_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        exclusive
            .inbound_stream_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_shutdown_complete(
        inner: &ConnectionInner,
        _payload: &msquic::ConnectionEventShutdownComplete,
    ) -> u32 {
        trace!(
            "Connection({:p}) Connection Shutdown complete",
            inner
        );

        {
            let mut exclusive = inner.exclusive.lock().unwrap();
            exclusive.state = ConnectionState::ShutdownComplete;
            if exclusive.error.is_none() {
                exclusive.error = Some(ConnectionError::ShutdownByLocal);
            }
            exclusive
                .start_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
            exclusive
                .inbound_stream_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
            exclusive
                .shutdown_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
        }
        unsafe {
            Arc::from_raw(inner as *const _);
        }
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_peer_stream_started(
        inner: &ConnectionInner,
        payload: &msquic::ConnectionEventPeerStreamStarted,
    ) -> u32 {
        let stream_type = if (payload.flags & msquic::STREAM_OPEN_FLAG_UNIDIRECTIONAL) != 0 {
            StreamType::Unidirectional
        } else {
            StreamType::Bidirectional
        };
        trace!(
            "Connection({:p}) Peer stream started {:?}",
            inner, stream_type
        );

        let stream = Stream::from_handle(payload.stream, &inner.shared.msquic_api, stream_type);
        if (payload.flags & msquic::STREAM_OPEN_FLAG_UNIDIRECTIONAL) != 0 {
            if let (Some(read_stream), None) = stream.split() {
                let mut exclusive = inner.exclusive.lock().unwrap();
                exclusive.inbound_uni_streams.push_back(read_stream);
                exclusive
                    .inbound_uni_stream_waiters
                    .drain(..)
                    .for_each(|waker| waker.wake());
            } else {
                unreachable!();
            }
        } else {
            {
                let mut exclusive = inner.exclusive.lock().unwrap();
                exclusive.inbound_streams.push_back(stream);
                exclusive
                    .inbound_stream_waiters
                    .drain(..)
                    .for_each(|waker| waker.wake());
            }
        }

        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_streams_available(
        inner: &ConnectionInner,
        payload: &msquic::ConnectionEventStreamsAvailable,
    ) -> u32 {
        trace!("Connection({:p}) Streams available bidirectional_count:{} unidirectional_count:{}", inner, payload.bidirectional_count, payload.unidirectional_count);
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_datagram_state_changed(
        inner: &ConnectionInner,
        payload: &msquic::ConnectionEventDatagramStateChanged,
    ) -> u32 {
        trace!("Connection({:p}) Datagram state changed send_enabled:{} max_send_length:{}", inner, payload.send_enabled, payload.max_send_length);
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_datagram_received(
        inner: &ConnectionInner,
        payload: &msquic::ConnectionEventDatagramReceived,
    ) -> u32 {
        trace!("Connection({:p}) Datagram received", inner);
        let buffer = unsafe {
            std::slice::from_raw_parts((*payload.buffer).buffer, (*payload.buffer).length as usize)
        };
        let buf = Bytes::copy_from_slice(buffer);
        {
            let mut exclusive = inner.exclusive.lock().unwrap();
            exclusive.recv_buffers.push_back(buf);
            exclusive
                .recv_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
        }
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_datagram_send_state_changed(
        inner: &ConnectionInner,
        payload: &msquic::ConnectionEventDatagramSendStateChanged,
    ) -> u32 {
        trace!(
            "Connection({:p}) Datagram send state changed state:{}",
            inner, payload.state
        );
        match payload.state {
            msquic::DATAGRAM_SEND_SENT | msquic::DATAGRAM_SEND_CANCELED => {
                let mut write_buf = unsafe { WriteBuffer::from_raw(payload.client_context) };
                let mut exclusive = inner.exclusive.lock().unwrap();
                write_buf.reset();
                exclusive.write_pool.push(write_buf);
            }
            _ => {}
        }
        msquic::QUIC_STATUS_SUCCESS
    }

    extern "C" fn native_callback(
        _connection: msquic::Handle,
        context: *mut c_void,
        event: &msquic::ConnectionEvent,
    ) -> u32 {
        let inner = unsafe { &mut *(context as *mut ConnectionInner) };
        match event.event_type {
            msquic::CONNECTION_EVENT_CONNECTED => {
                Self::handle_event_connected(inner, unsafe { &event.payload.connected })
            }
            msquic::CONNECTION_EVENT_SHUTDOWN_INITIATED_BY_TRANSPORT => {
                Self::handle_event_shutdown_initiated_by_transport(inner, unsafe {
                    &event.payload.shutdown_initiated_by_transport
                })
            }
            msquic::CONNECTION_EVENT_SHUTDOWN_INITIATED_BY_PEER => {
                Self::handle_event_shutdown_initiated_by_peer(inner, unsafe {
                    &event.payload.shutdown_initiated_by_peer
                })
            }
            msquic::CONNECTION_EVENT_SHUTDOWN_COMPLETE => {
                Self::handle_event_shutdown_complete(inner, unsafe {
                    &event.payload.shutdown_complete
                })
            }
            msquic::CONNECTION_EVENT_PEER_STREAM_STARTED => {
                Self::handle_event_peer_stream_started(inner, unsafe {
                    &event.payload.peer_stream_started
                })
            }
            msquic::CONNECTION_EVENT_STREAMS_AVAILABLE => {
                Self::handle_event_streams_available(inner, unsafe {
                    &event.payload.streams_available
                })
            }
            msquic::CONNECTION_EVENT_DATAGRAM_STATE_CHANGED => {
                Self::handle_event_datagram_state_changed(inner, unsafe {
                    &event.payload.datagram_state_changed
                })
            }
            msquic::CONNECTION_EVENT_DATAGRAM_RECEIVED => {
                Self::handle_event_datagram_received(inner, unsafe {
                    &event.payload.datagram_received
                })
            }
            msquic::CONNECTION_EVENT_DATAGRAM_SEND_STATE_CHANGED => {
                Self::handle_event_datagram_send_state_changed(inner, unsafe {
                    &event.payload.datagram_send_state_changed
                })
            }
            _ => {
                trace!(
                    "Connection({:p}) Other callback {}",
                    inner, event.event_type
                );
                msquic::QUIC_STATUS_SUCCESS
            }
        }
    }
}
impl Drop for ConnectionInner {
    fn drop(&mut self) {
        trace!("ConnectionInner({:p}) dropping", self);
    }
}

#[derive(Debug, PartialEq)]
enum ConnectionState {
    Open,
    Connecting,
    Connected,
    Shutdown,
    ShutdownComplete,
}

/// Errors that can occur when managing a connection.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConnectionError {
    #[error("connection shutdown by transport: status 0x{0:x}, error 0x{1:x}")]
    ShutdownByTransport(u32, u64),
    #[error("connection shutdown by peer: error 0x{0:x}")]
    ShutdownByPeer(u64),
    #[error("connection shutdown by local")]
    ShutdownByLocal,
    #[error("connection closed")]
    ConnectionClosed,
}

/// Errors that can occur when receiving a datagram.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DgramReceiveError {
    #[error("connection not started yet")]
    ConnectionNotStarted,
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
}

/// Errors that can occur when sending a datagram.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DgramSendError {
    #[error("connection not started yet")]
    ConnectionNotStarted,
    #[error("not allowed for sending dgram")]
    Denied,
    #[error("exceeded maximum data size for sending dgram")]
    TooBig,
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
}

/// Errors that can occur when starting a connection.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StartError {
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
}

/// Errors that can occur when shutdowning a connection.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ShutdownError {
    #[error("connection not started yet")]
    ConnectionNotStarted,
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
}

/// Future produced by [`Connection::start()`].
pub struct ConnectionStart<'a> {
    conn: &'a Connection,
    configuration: &'a msquic::Configuration,
    host: &'a str,
    port: u16,
}

impl Future for ConnectionStart<'_> {
    type Output = Result<(), StartError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.conn
            .poll_start(cx, self.configuration, self.host, self.port)
    }
}

/// Future produced by [`Connection::open_outbound_stream()`].
pub struct OpenOutboundStream<'a> {
    conn: &'a ConnectionInner,
    stream_type: Option<crate::stream::StreamType>,
    stream: Option<crate::stream::Stream>,
    fail_on_blocked: bool,
}

impl Future for OpenOutboundStream<'_> {
    type Output = Result<crate::stream::Stream, StreamStartError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let OpenOutboundStream {
            conn,
            ref mut stream_type,
            ref mut stream,
            fail_on_blocked: fail_blocked,
            ..
        } = *this;

        let mut exclusive = conn.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                return Poll::Ready(Err(StreamStartError::ConnectionNotStarted));
            }
            ConnectionState::Connecting => {
                exclusive.start_waiters.push(cx.waker().clone());
                return Poll::Pending;
            }
            ConnectionState::Connected => {}
            ConnectionState::Shutdown | ConnectionState::ShutdownComplete => {
                return Poll::Ready(Err(StreamStartError::ConnectionLost(
                    exclusive.error.as_ref().expect("error").clone(),
                )));
            }
        }
        if stream.is_none() {
            *stream = Some(Stream::open(
                &conn.shared.msquic_conn,
                &conn.shared.msquic_api,
                stream_type.take().unwrap(),
            ));
        }
        stream
            .as_mut()
            .unwrap()
            .poll_start(cx, fail_blocked)
            .map(|res| res.map(|_| stream.take().unwrap()))
    }
}

/// Future produced by [`Connection::accept_inbound_stream()`].
pub struct AcceptInboundStream<'a> {
    conn: &'a Connection,
}

impl Future for AcceptInboundStream<'_> {
    type Output = Result<Stream, StreamStartError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.conn.poll_accept_inbound_stream(cx)
    }
}

/// Future produced by [`Connection::accept_inbound_uni_stream()`].
pub struct AcceptInboundUniStream<'a> {
    conn: &'a Connection,
}

impl Future for AcceptInboundUniStream<'_> {
    type Output = Result<ReadStream, StreamStartError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.conn.poll_accept_inbound_uni_stream(cx)
    }
}
