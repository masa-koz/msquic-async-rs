use crate::stream::{StartError, Stream, StreamType};

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};

use libc::c_void;

use thiserror::Error;

pub struct Connection(Box<ConnectionInner>);

struct ConnectionInner {
    exclusive: Mutex<ConnectionInnerExclusive>,
    shared: ConnectionInnerShared,
}

struct ConnectionInnerExclusive {
    msquic_conn: msquic::Connection,
    state: ConnectionState,
    error: Option<ConnectionError>,
    start_waiters: Vec<Waker>,
    inbound_stream_waiters: Vec<Waker>,
    inbound_streams: VecDeque<crate::stream::Stream>,
}
unsafe impl Sync for ConnectionInnerExclusive {}
unsafe impl Send for ConnectionInnerExclusive {}

struct ConnectionInnerShared {
    shutdown_complete: Arc<(Mutex<bool>, Condvar)>,
}

impl Connection {
    pub fn new(msquic_conn: msquic::Connection, registration: &msquic::Registration) -> Self {
        let inner = Box::new(ConnectionInner {
            exclusive: Mutex::new(ConnectionInnerExclusive {
                msquic_conn,
                state: ConnectionState::Open,
                error: None,
                start_waiters: Vec::new(),
                inbound_stream_waiters: Vec::new(),
                inbound_streams: VecDeque::new(),
            }),
            shared: ConnectionInnerShared {
                shutdown_complete: Arc::new((Mutex::new(false), Condvar::new())),
            },
        });
        {
            let exclusive = inner.exclusive.lock().unwrap();
            exclusive.msquic_conn.open(
                registration,
                Self::native_callback,
                &*inner as *const _ as *const c_void,
            );
        }
        Self(inner)
    }

    pub(crate) fn from_handle(conn: msquic::Handle) -> Self {
        let msquic_conn = msquic::Connection::from_parts(conn, &*crate::MSQUIC_API);
        let inner = Box::new(ConnectionInner {
            exclusive: Mutex::new(ConnectionInnerExclusive {
                msquic_conn,
                state: ConnectionState::Connected,
                error: None,
                start_waiters: Vec::new(),
                inbound_stream_waiters: Vec::new(),
                inbound_streams: VecDeque::new(),
            }),
            shared: ConnectionInnerShared {
                shutdown_complete: Arc::new((Mutex::new(false), Condvar::new())),
            },
        });
        {
            let exclusive = inner.exclusive.lock().unwrap();
            exclusive
                .msquic_conn
                .set_callback_handler(Self::native_callback, &*inner as *const _ as *const c_void);
        }
        Self(inner)
    }

    pub fn start<'a>(
        &'a self,
        configuration: &'a msquic::Configuration,
        host: &'a str,
        port: u16,
    ) -> ConnectionStart<'a> {
        ConnectionStart {
            conn: &self.0,
            configuration,
            host,
            port,
        }
    }

    pub(crate) fn set_configuration(&self, configuration: &msquic::Configuration) {
        let exclusive = self.0.exclusive.lock().unwrap();
        exclusive.msquic_conn.set_configuration(configuration);
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
        println!("msquic-async::Connection({:p}) Transport shutdown 0x{:x}", inner, payload.status);

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
        println!("msquic-async::Connection({:p}) App shutdown {}", inner, payload.error_code);

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
        println!("msquic-async::Connection({:p}) Connection Shutdown complete", inner);

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
        }
        {
            let (lock, cvar) = &*inner.shared.shutdown_complete;
            let mut shutdown_complete = lock.lock().unwrap();
            *shutdown_complete = true;
            cvar.notify_one();
        }
        0
    }

    fn handle_event_peer_stream_started(
        inner: &ConnectionInner,
        payload: &msquic::ConnectionEventPeerStreamStarted,
    ) -> u32 {
        println!("msquic-async::Connection({:p}) Peer stream started", inner);

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
            _ => {
                println!("msquic-async::Connection({:p}) Other callback {}", inner, event.event_type);
                0
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        println!("msquic-async::Connection({:p}) dropping", &self.0);
        {
            let exclusive = self.0.exclusive.lock().unwrap();
            if exclusive.state != ConnectionState::ShutdownComplete {
                println!("msquic-async::Connection({:p}) do shutdown", self);
                exclusive
                    .msquic_conn
                    .shutdown(msquic::CONNECTION_SHUTDOWN_FLAG_NONE, 0);
            }
        }
        println!("msquic-async::Connection({:p}) wait for shutdown complete", self);
        let (lock, cvar) = &*self.0.shared.shutdown_complete;
        let mut shutdown_complete = lock.lock().unwrap();
        while !*shutdown_complete {
            shutdown_complete = cvar.wait(shutdown_complete).unwrap();
        }
        println!("msquic-async::Connection({:p}) after shutdown complete", self);
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
}

pub struct ConnectionStart<'a> {
    conn: &'a ConnectionInner,
    configuration: &'a msquic::Configuration,
    host: &'a str,
    port: u16,
}

impl Future for ConnectionStart<'_> {
    type Output = Result<(), ConnectionError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut exclusive = this.conn.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                exclusive
                    .msquic_conn
                    .start(this.configuration, this.host, this.port);
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
                &exclusive.msquic_conn,
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
