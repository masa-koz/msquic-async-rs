use crate::buffer::WriteBuffer;
use crate::stream::{ReadStream, StartError as StreamStartError, Stream, StreamType};

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
    pub fn new(
        msquic_conn: msquic::Connection,
        registration: &msquic::Registration,
    ) -> Result<Self, ConnectionError> {
        let mut inner = Arc::new(ConnectionInner::new(msquic_conn, ConnectionState::Open));
        Arc::get_mut(&mut inner)
            .unwrap()
            .shared
            .msquic_conn
            .open(
                registration,
                Some(ConnectionInner::native_callback),
                std::ptr::null(),
            )
            .map_err(ConnectionError::OtherError)?;
        unsafe {
            inner.shared.msquic_conn.set_callback_handler(
                Some(ConnectionInner::native_callback),
                Arc::into_raw(inner.clone()) as *const c_void,
            );
        }
        trace!("Connection({:p}) Open by local", &*inner);
        Ok(Self(Arc::new(ConnectionInstance(inner))))
    }

    pub(crate) fn from_raw(handle: msquic::ffi::HQUIC) -> Self {
        let msquic_conn = unsafe { msquic::Connection::from_raw(handle) };
        let inner = Arc::new(ConnectionInner::new(
            msquic_conn,
            ConnectionState::Connected,
        ));
        unsafe {
            inner.shared.msquic_conn.set_callback_handler(
                Some(ConnectionInner::native_callback),
                Arc::into_raw(inner.clone()) as *const c_void,
            )
        };
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
            conn: self,
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
                self.0
                    .shared
                    .msquic_conn
                    .start(configuration, host, port)
                    .map_err(StartError::OtherError)?;
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
        AcceptInboundStream { conn: self }
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
        AcceptInboundUniStream { conn: self }
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
        exclusive
            .inbound_uni_stream_waiters
            .push(cx.waker().clone());
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
            Poll::Pending
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

        let mut write_buf = exclusive.write_pool.pop().unwrap_or(WriteBuffer::new());
        let _ = write_buf.put_zerocopy(buf);
        let buffers = unsafe {
            let (data, len) = write_buf.get_buffers();
            std::slice::from_raw_parts(data, len)
        };
        let res = unsafe {
            self.0.shared.msquic_conn.datagram_send(
                buffers,
                msquic::SEND_FLAG_NONE,
                write_buf.into_raw() as *const _,
            )
        }
        .map_err(DgramSendError::OtherError);
        Poll::Ready(res)
    }

    /// Send a datagram.
    pub fn send_datagram(&self, buf: &Bytes) -> Result<(), DgramSendError> {
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

        let mut write_buf = exclusive.write_pool.pop().unwrap_or(WriteBuffer::new());
        let _ = write_buf.put_zerocopy(buf);
        let buffers = unsafe {
            let (data, len) = write_buf.get_buffers();
            std::slice::from_raw_parts(data, len)
        };
        unsafe {
            self.0.shared.msquic_conn.datagram_send(
                buffers,
                msquic::SEND_FLAG_NONE,
                write_buf.into_raw() as *const _,
            )
        }
        .map_err(DgramSendError::OtherError)?;
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
    pub fn shutdown(&self, error_code: u64) -> Result<(), ShutdownError> {
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
            _ => {}
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
}

impl ConnectionInner {
    fn new(msquic_conn: msquic::Connection, state: ConnectionState) -> Self {
        Self {
            exclusive: Mutex::new(ConnectionInnerExclusive {
                state,
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
            shared: ConnectionInnerShared { msquic_conn },
        }
    }

    fn handle_event_connected(
        &self,
        _session_resumed: bool,
        _negotiated_alpn: &[u8],
    ) -> msquic::StatusCode {
        trace!("Connection({:p}) Connected", self);

        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive.state = ConnectionState::Connected;
        exclusive
            .start_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_shutdown_initiated_by_transport(
        &self,
        status: msquic::Status,
        error_code: u64,
    ) -> msquic::StatusCode {
        trace!("Connection({:p}) Transport shutdown {:?}", self, status);

        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive.state = ConnectionState::Shutdown;
        exclusive.error = Some(ConnectionError::ShutdownByTransport(status, error_code));
        exclusive
            .start_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        exclusive
            .inbound_stream_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_shutdown_initiated_by_peer(&self, error_code: u64) -> msquic::StatusCode {
        trace!("Connection({:p}) App shutdown {}", self, error_code);

        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive.state = ConnectionState::Shutdown;
        exclusive.error = Some(ConnectionError::ShutdownByPeer(error_code));
        exclusive
            .start_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        exclusive
            .inbound_stream_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_shutdown_complete(
        &self,
        _handshake_completed: bool,
        _peer_acknowledged_shutdown: bool,
        _app_close_in_progress: bool,
    ) -> msquic::StatusCode {
        trace!("Connection({:p}) Connection Shutdown complete", self);

        {
            let mut exclusive = self.exclusive.lock().unwrap();
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
            Arc::from_raw(self as *const _);
        }
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_peer_stream_started(
        &self,
        stream: msquic::StreamRef,
        flags: msquic::ffi::QUIC_STREAM_OPEN_FLAGS,
    ) -> msquic::StatusCode {
        let stream_type = if (flags
            & msquic::ffi::QUIC_STREAM_OPEN_FLAGS_QUIC_STREAM_OPEN_FLAG_UNIDIRECTIONAL)
            != 0
        {
            StreamType::Unidirectional
        } else {
            StreamType::Bidirectional
        };
        trace!(
            "Connection({:p}) Peer stream started {:?}",
            self,
            stream_type
        );

        let stream = Stream::from_raw(unsafe { stream.as_raw() }, stream_type);
        if (flags & msquic::ffi::QUIC_STREAM_OPEN_FLAGS_QUIC_STREAM_OPEN_FLAG_UNIDIRECTIONAL) != 0 {
            if let (Some(read_stream), None) = stream.split() {
                let mut exclusive = self.exclusive.lock().unwrap();
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
                let mut exclusive = self.exclusive.lock().unwrap();
                exclusive.inbound_streams.push_back(stream);
                exclusive
                    .inbound_stream_waiters
                    .drain(..)
                    .for_each(|waker| waker.wake());
            }
        }

        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_streams_available(
        &self,
        bidirectional_count: u16,
        unidirectional_count: u16,
    ) -> msquic::StatusCode {
        trace!(
            "Connection({:p}) Streams available bidirectional_count:{} unidirectional_count:{}",
            self,
            bidirectional_count,
            unidirectional_count
        );
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_datagram_state_changed(
        &self,
        send_enabled: bool,
        max_send_length: u16,
    ) -> msquic::StatusCode {
        trace!(
            "Connection({:p}) Datagram state changed send_enabled:{} max_send_length:{}",
            self,
            send_enabled,
            max_send_length
        );
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_datagram_received(
        &self,
        buffer: &msquic::BufferRef,
        _flags: msquic::ffi::QUIC_RECEIVE_FLAGS,
    ) -> msquic::StatusCode {
        trace!("Connection({:p}) Datagram received", self);
        let buf = Bytes::copy_from_slice(buffer.as_bytes());
        {
            let mut exclusive = self.exclusive.lock().unwrap();
            exclusive.recv_buffers.push_back(buf);
            exclusive
                .recv_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
        }
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_datagram_send_state_changed(
        &self,
        client_context: *const c_void,
        state: msquic::ffi::QUIC_DATAGRAM_SEND_STATE,
    ) -> msquic::StatusCode {
        trace!(
            "Connection({:p}) Datagram send state changed state:{}",
            self,
            state
        );
        match state {
            msquic::ffi::QUIC_DATAGRAM_SEND_STATE_QUIC_DATAGRAM_SEND_SENT
            | msquic::ffi::QUIC_DATAGRAM_SEND_STATE_QUIC_DATAGRAM_SEND_CANCELED => {
                let mut write_buf = unsafe { WriteBuffer::from_raw(client_context) };
                let mut exclusive = self.exclusive.lock().unwrap();
                write_buf.reset();
                exclusive.write_pool.push(write_buf);
            }
            _ => {}
        }
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    extern "C" fn native_callback(
        _connection: msquic::ffi::HQUIC,
        context: *mut c_void,
        event: *mut msquic::ffi::QUIC_CONNECTION_EVENT,
    ) -> msquic::ffi::QUIC_STATUS {
        let inner = unsafe { &*(context as *const Self) };
        let ev_ref = unsafe { event.as_ref().unwrap() };
        let event = msquic::ConnectionEvent::from(ev_ref);

        let status = match event {
            msquic::ConnectionEvent::Connected {
                session_resumed,
                negotiated_alpn,
            } => inner.handle_event_connected(session_resumed, negotiated_alpn),
            msquic::ConnectionEvent::ShutdownInitiatedByTransport { status, error_code } => {
                inner.handle_event_shutdown_initiated_by_transport(status, error_code)
            }
            msquic::ConnectionEvent::ShutdownInitiatedByPeer { error_code } => {
                inner.handle_event_shutdown_initiated_by_peer(error_code)
            }
            msquic::ConnectionEvent::ShutdownComplete {
                handshake_completed,
                peer_acknowledged_shutdown,
                app_close_in_progress,
            } => inner.handle_event_shutdown_complete(
                handshake_completed,
                peer_acknowledged_shutdown,
                app_close_in_progress,
            ),
            msquic::ConnectionEvent::PeerStreamStarted { stream, flags } => {
                inner.handle_event_peer_stream_started(stream, flags)
            }
            msquic::ConnectionEvent::StreamsAvailable {
                bidirectional_count,
                unidirectional_count,
            } => inner.handle_event_streams_available(bidirectional_count, unidirectional_count),
            msquic::ConnectionEvent::DatagramStateChanged {
                send_enabled,
                max_send_length,
            } => inner.handle_event_datagram_state_changed(send_enabled, max_send_length),
            msquic::ConnectionEvent::DatagramReceived { buffer, flags } => {
                inner.handle_event_datagram_received(buffer, flags)
            }
            msquic::ConnectionEvent::DatagramSendStateChanged {
                client_context,
                state,
            } => inner.handle_event_datagram_send_state_changed(client_context, state),
            _ => {
                trace!("Connection({:p}) Other callback {}", inner, ev_ref.Type);
                msquic::StatusCode::QUIC_STATUS_SUCCESS
            }
        };
        status.into()
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
#[derive(Debug, Error, Clone)]
pub enum ConnectionError {
    #[error("connection shutdown by transport: status {0:?}, error 0x{1:x}")]
    ShutdownByTransport(msquic::Status, u64),
    #[error("connection shutdown by peer: error 0x{0:x}")]
    ShutdownByPeer(u64),
    #[error("connection shutdown by local")]
    ShutdownByLocal,
    #[error("connection closed")]
    ConnectionClosed,
    #[error("other error: status {0:?}")]
    OtherError(msquic::Status),
}

/// Errors that can occur when receiving a datagram.
#[derive(Debug, Error, Clone)]
pub enum DgramReceiveError {
    #[error("connection not started yet")]
    ConnectionNotStarted,
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
    #[error("other error: status {0:?}")]
    OtherError(msquic::Status),
}

/// Errors that can occur when sending a datagram.
#[derive(Debug, Error, Clone)]
pub enum DgramSendError {
    #[error("connection not started yet")]
    ConnectionNotStarted,
    #[error("not allowed for sending dgram")]
    Denied,
    #[error("exceeded maximum data size for sending dgram")]
    TooBig,
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
    #[error("other error: status {0:?}")]
    OtherError(msquic::Status),
}

/// Errors that can occur when starting a connection.
#[derive(Debug, Error, Clone)]
pub enum StartError {
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
    #[error("other error: status {0:?}")]
    OtherError(msquic::Status),
}

/// Errors that can occur when shutdowning a connection.
#[derive(Debug, Error, Clone)]
pub enum ShutdownError {
    #[error("connection not started yet")]
    ConnectionNotStarted,
    #[error("connection lost")]
    ConnectionLost(#[from] ConnectionError),
    #[error("other error: status {0:?}")]
    OtherError(msquic::Status),
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
            match Stream::open(&conn.shared.msquic_conn, stream_type.take().unwrap()) {
                Ok(new_stream) => {
                    *stream = Some(new_stream);
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
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
