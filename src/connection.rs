use crate::stream::{StartError, Stream, StreamType};

use std::collections::VecDeque;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use libc::c_void;

use thiserror::Error;

#[derive(Clone)]
pub struct Connection(Arc<ConnectionInstance>);

struct ConnectionInstance(Arc<ConnectionInner>);

struct ConnectionInner {
    exclusive: Mutex<ConnectionInnerExclusive>,
    shared: ConnectionInnerShared,
}

struct ConnectionInnerExclusive {
    state: ConnectionState,
    error: Option<ConnectionError>,
    start_waiters: Vec<Waker>,
    inbound_stream_waiters: Vec<Waker>,
    inbound_streams: VecDeque<crate::stream::Stream>,
    shutdown_waiters: Vec<Waker>,
}
unsafe impl Sync for ConnectionInnerExclusive {}
unsafe impl Send for ConnectionInnerExclusive {}

struct ConnectionInnerShared {
    msquic_conn: msquic::Connection,
}
unsafe impl Sync for ConnectionInnerShared {}
unsafe impl Send for ConnectionInnerShared {}

impl Connection {
    pub fn new(msquic_conn: msquic::Connection, registration: &msquic::Registration) -> Self {
        let inner = Arc::new(ConnectionInner {
            exclusive: Mutex::new(ConnectionInnerExclusive {
                state: ConnectionState::Open,
                error: None,
                start_waiters: Vec::new(),
                inbound_stream_waiters: Vec::new(),
                inbound_streams: VecDeque::new(),
                shutdown_waiters: Vec::new(),
            }),
            shared: ConnectionInnerShared { msquic_conn },
        });
        inner.shared.msquic_conn.open(
            registration,
            Self::native_callback,
            Arc::into_raw(inner.clone()) as *const c_void,
        );
        println!("msquic-async::Connection({:p}) Open by local", &*inner);

        Self(Arc::new(ConnectionInstance(inner)))
    }

    pub(crate) fn from_handle(conn: msquic::Handle) -> Self {
        let msquic_conn = msquic::Connection::from_parts(conn, &*crate::MSQUIC_API);
        let inner = Arc::new(ConnectionInner {
            exclusive: Mutex::new(ConnectionInnerExclusive {
                state: ConnectionState::Connected,
                error: None,
                start_waiters: Vec::new(),
                inbound_stream_waiters: Vec::new(),
                inbound_streams: VecDeque::new(),
                shutdown_waiters: Vec::new(),
            }),
            shared: ConnectionInnerShared { msquic_conn },
        });
            inner.shared.msquic_conn.set_callback_handler(
                Self::native_callback,
                Arc::into_raw(inner.clone()) as *const c_void,
            );
        println!("msquic-async::Connection({:p}) Open by peer", &*inner);
        Self(Arc::new(ConnectionInstance(inner)))
    }

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

    fn poll_start(
        &self,
        cx: &mut Context<'_>,
        configuration: &msquic::Configuration,
        host: &str,
        port: u16,
    ) -> Poll<Result<(), ConnectionError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                self.0.shared.msquic_conn.start(configuration, host, port);
                exclusive.state = ConnectionState::Connecting;
            }
            ConnectionState::Connecting => {}
            ConnectionState::Connected => return Poll::Ready(Ok(())),
            ConnectionState::Shutdown | ConnectionState::ShutdownComplete => {
                if let Some(error) = &exclusive.error {
                    return Poll::Ready(Err(error.clone()));
                } else {
                    return Poll::Ready(Err(ConnectionError::ConnectionClosed));
                }
            }
        }
        exclusive.start_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    pub(crate) fn set_configuration(&self, configuration: &msquic::Configuration) {
        self.0.shared.msquic_conn.set_configuration(configuration);
    }

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

    pub fn accept_inbound_stream(&self) -> AcceptInboundStream<'_> {
        AcceptInboundStream { conn: &self }
    }

    pub fn poll_accept_inbound_stream(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Stream, StartError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                return Poll::Ready(Err(StartError::ConnectionClosed));
            }
            ConnectionState::Connecting => {
                exclusive.start_waiters.push(cx.waker().clone());
                return Poll::Pending;
            }
            ConnectionState::Connected => {}
            ConnectionState::Shutdown | ConnectionState::ShutdownComplete => {
                if let Some(error) = &exclusive.error {
                    return Poll::Ready(Err(StartError::ConnectionLost(error.clone())));
                } else {
                    return Poll::Ready(Err(StartError::ConnectionClosed));
                }
            }
        }

        if !exclusive.inbound_streams.is_empty() {
            return Poll::Ready(Ok(exclusive.inbound_streams.pop_front().unwrap()));
        }
        exclusive.inbound_stream_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    pub fn poll_shutdown(
        &self,
        cx: &mut Context<'_>,
        error_code: u64,
    ) -> Poll<Result<(), ConnectionError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                return Poll::Ready(Err(ConnectionError::ConnectionNotStarted));
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
                if let Some(error) = &exclusive.error {
                    return Poll::Ready(Err(error.clone()));
                } else {
                    return Poll::Ready(Ok(()));
                }
            }
        }

        exclusive.shutdown_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    fn handle_event_connected(
        inner: &ConnectionInner,
        _payload: &msquic::ConnectionEventConnected,
    ) -> u32 {
        println!("msquic-async::Connection({:p}) Connected", inner);

        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.state = ConnectionState::Connected;
        exclusive
            .start_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        0
    }

    fn handle_event_shutdown_initiated_by_transport(
        inner: &ConnectionInner,
        payload: &msquic::ConnectionEventConnectionShutdownByTransport,
    ) -> u32 {
        println!(
            "msquic-async::Connection({:p}) Transport shutdown 0x{:x}",
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
        0
    }

    fn handle_event_shutdown_initiated_by_peer(
        inner: &ConnectionInner,
        payload: &msquic::ConnectionEventConnectionShutdownByPeer,
    ) -> u32 {
        println!(
            "msquic-async::Connection({:p}) App shutdown {}",
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
        0
    }

    fn handle_event_shutdown_complete(
        inner: &ConnectionInner,
        _payload: &msquic::ConnectionEventShutdownComplete,
    ) -> u32 {
        println!(
            "msquic-async::Connection({:p}) Connection Shutdown complete",
            inner
        );

        {
            let mut exclusive = inner.exclusive.lock().unwrap();
            exclusive.state = ConnectionState::ShutdownComplete;
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
        0
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
        let stream = Stream::from_handle(payload.stream, stream_type);
        {
            let mut exclusive = inner.exclusive.lock().unwrap();
            exclusive.inbound_streams.push_back(stream);
            exclusive
                .inbound_stream_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
        }

        0
    }

    fn handle_event_streams_available(
        inner: &ConnectionInner,
        payload: &msquic::ConnectionEventStreamsAvailable,
    ) -> u32 {
        println!("msquic-async::Connection({:p}) Streams available bidirectional_count:{} unidirectional_count:{}", inner, payload.bidirectional_count, payload.unidirectional_count);
        0
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
            _ => {
                println!(
                    "msquic-async::Connection({:p}) Other callback {}",
                    inner, event.event_type
                );
                0
            }
        }
    }
}

impl Drop for ConnectionInstance {
    fn drop(&mut self) {
        println!("msquic-async::Connection({:p}) dropping", &*self.0);
        {
            let exclusive = self.0.exclusive.lock().unwrap();
            match exclusive.state {
                ConnectionState::Open
                | ConnectionState::Connecting
                | ConnectionState::Connected => {
                    println!("msquic-async::Connection({:p}) do shutdown", &*self.0);
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

impl Deref for ConnectionInstance {
    type Target = ConnectionInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for ConnectionInner {
    fn drop(&mut self) {
        println!("msquic-async::ConnectionInner({:p}) dropping", self);
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
    #[error("connection not started yet")]
    ConnectionNotStarted,
}

pub struct ConnectionStart<'a> {
    conn: &'a Connection,
    configuration: &'a msquic::Configuration,
    host: &'a str,
    port: u16,
}

impl Future for ConnectionStart<'_> {
    type Output = Result<(), ConnectionError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.conn
            .poll_start(cx, self.configuration, self.host, self.port)
    }
}

pub struct OpenOutboundStream<'a> {
    conn: &'a ConnectionInner,
    stream_type: Option<crate::stream::StreamType>,
    stream: Option<crate::stream::Stream>,
    fail_on_blocked: bool,
}

impl Future for OpenOutboundStream<'_> {
    type Output = Result<crate::stream::Stream, crate::stream::StartError>;

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
                return Poll::Ready(Err(StartError::ConnectionClosed));
            }
            ConnectionState::Connecting => {
                exclusive.start_waiters.push(cx.waker().clone());
                return Poll::Pending;
            }
            ConnectionState::Connected => {}
            ConnectionState::Shutdown | ConnectionState::ShutdownComplete => {
                if let Some(error) = &exclusive.error {
                    return Poll::Ready(Err(StartError::ConnectionLost(error.clone())));
                } else {
                    return Poll::Ready(Err(StartError::ConnectionClosed));
                }
            }
        }
        if stream.is_none() {
            *stream = Some(Stream::open(
                &conn.shared.msquic_conn,
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
pub struct AcceptInboundStream<'a> {
    conn: &'a Connection,
}

impl Future for AcceptInboundStream<'_> {
    type Output = Result<Stream, StartError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.conn.poll_accept_inbound_stream(cx)
    }
}
