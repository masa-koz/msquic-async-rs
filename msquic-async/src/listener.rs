use crate::connection::Connection;

use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use libc::c_void;
use thiserror::Error;
use tracing::trace;

/// Listener for incoming connections.
pub struct Listener(Box<ListenerInner>);

impl Listener {
    /// Create a new listener.
    pub fn new(
        msquic_listener: msquic::Listener,
        registration: &msquic::Registration,
        configuration: msquic::Configuration,
    ) -> Result<Self, ListenError> {
        let inner = Box::new(ListenerInner::new(msquic_listener, configuration));
        {
            inner
                .shared
                .msquic_listener
                .open(
                    registration,
                    ListenerInner::native_callback,
                    &*inner as *const _ as *const c_void,
                )
                .map_err(|status| ListenError::OtherError(status))?;
        }
        Ok(Self(inner))
    }

    /// Start the listener.
    pub fn start<T: AsRef<[msquic::Buffer]>>(
        &self,
        alpn: &T,
        local_address: Option<SocketAddr>,
    ) -> Result<(), ListenError> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ListenerState::Open | ListenerState::ShutdownComplete => {}
            ListenerState::StartComplete | ListenerState::Shutdown => {
                return Err(ListenError::AlreadyStarted);
            }
        }
        let local_address: Option<msquic::Addr> = local_address.map(|x| x.into());
        self.0
            .shared
            .msquic_listener
            .start(alpn.as_ref(), local_address.as_ref())
            .map_err(|status| ListenError::OtherError(status))?;
        exclusive.state = ListenerState::StartComplete;
        Ok(())
    }

    /// Accept a new connection.
    pub fn accept(&self) -> Accept {
        Accept(self)
    }

    /// Poll to accept a new connection.
    pub fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<Result<Connection, ListenError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();

        if !exclusive.new_connections.is_empty() {
            return Poll::Ready(Ok(exclusive.new_connections.pop_front().unwrap()));
        }

        match exclusive.state {
            ListenerState::Open => {
                return Poll::Ready(Err(ListenError::NotStarted));
            }
            ListenerState::StartComplete | ListenerState::Shutdown => {}
            ListenerState::ShutdownComplete => {
                return Poll::Ready(Err(ListenError::Finished));
            }
        }
        exclusive.new_connection_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    /// Stop the listener.
    pub fn stop(&self) -> Stop {
        Stop(self)
    }

    /// Poll to stop the listener.
    pub fn poll_stop(&self, cx: &mut Context<'_>) -> Poll<Result<(), ListenError>> {
        let mut call_stop = false;
        {
            let mut exclusive = self.0.exclusive.lock().unwrap();

            match exclusive.state {
                ListenerState::Open => {
                    return Poll::Ready(Err(ListenError::NotStarted));
                }
                ListenerState::StartComplete => {
                    call_stop = true;
                    exclusive.state = ListenerState::Shutdown;
                }
                ListenerState::Shutdown => {}
                ListenerState::ShutdownComplete => {
                    return Poll::Ready(Ok(()));
                }
            }
            exclusive.shutdown_complete_waiters.push(cx.waker().clone());
        }
        if call_stop {
            self.0.shared.msquic_listener.stop();
        }
        Poll::Pending
    }

    /// Get the local address the listener is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, ListenError> {
        self.0
            .shared
            .msquic_listener
            .get_local_addr()
            .map(|addr| addr.as_socket().expect("not a socket address"))
            .map_err(|_| ListenError::Failed)
    }
}

struct ListenerInner {
    exclusive: Mutex<ListenerInnerExclusive>,
    shared: ListenerInnerShared,
}

struct ListenerInnerExclusive {
    state: ListenerState,
    new_connections: VecDeque<Connection>,
    new_connection_waiters: Vec<Waker>,
    shutdown_complete_waiters: Vec<Waker>,
}
unsafe impl Sync for ListenerInnerExclusive {}
unsafe impl Send for ListenerInnerExclusive {}

struct ListenerInnerShared {
    msquic_listener: msquic::Listener,
    configuration: msquic::Configuration,
}
unsafe impl Sync for ListenerInnerShared {}
unsafe impl Send for ListenerInnerShared {}

#[derive(Debug, Clone, PartialEq)]
enum ListenerState {
    Open,
    StartComplete,
    Shutdown,
    ShutdownComplete,
}

impl ListenerInner {
    fn new(msquic_listener: msquic::Listener, configuration: msquic::Configuration) -> Self {
        Self {
            exclusive: Mutex::new(ListenerInnerExclusive {
                state: ListenerState::Open,
                new_connections: VecDeque::new(),
                new_connection_waiters: Vec::new(),
                shutdown_complete_waiters: Vec::new(),
            }),
            shared: ListenerInnerShared {
                msquic_listener,
                configuration,
            },
        }
    }

    fn handle_event_new_connection(
        inner: &Self,
        payload: &msquic::ListenerEventNewConnection,
    ) -> u32 {
        trace!("Listener({:p}) new connection event", inner);

        let new_conn = Connection::from_handle(payload.connection);
        if let Err(status) = new_conn.set_configuration(&inner.shared.configuration) {
            return status;
        }

        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.new_connections.push_back(new_conn);
        exclusive
            .new_connection_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_stop_complete(
        inner: &Self,
        _payload: &msquic::ListenerEventStopComplete,
    ) -> u32 {
        trace!("Listener({:p}) stop complete", inner);
        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.state = ListenerState::ShutdownComplete;

        exclusive
            .new_connection_waiters
            .drain(..)
            .for_each(|waker| waker.wake());

        exclusive
            .shutdown_complete_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::QUIC_STATUS_SUCCESS
    }

    extern "C" fn native_callback(
        _listener: msquic::Handle,
        context: *mut c_void,
        event: &msquic::ListenerEvent,
    ) -> u32 {
        let inner = unsafe { &mut *(context as *mut Self) };
        match event.event_type {
            msquic::LISTENER_EVENT_NEW_CONNECTION => {
                Self::handle_event_new_connection(inner, unsafe { &event.payload.new_connection })
            }
            msquic::LISTENER_EVENT_STOP_COMPLETE => {
                Self::handle_event_stop_complete(inner, unsafe { &event.payload.stop_complete })
            }

            _ => {
                println!("Other callback {}", event.event_type);
                0
            }
        }
    }
}

/// Future generated by `[Listener::accept()]`.
pub struct Accept<'a>(&'a Listener);

impl Future for Accept<'_> {
    type Output = Result<Connection, ListenError>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.poll_accept(cx)
    }
}

/// Future generated by `[Listener::stop()]`.
pub struct Stop<'a>(&'a Listener);

impl Future for Stop<'_> {
    type Output = Result<(), ListenError>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.poll_stop(cx)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ListenError {
    #[error("Not started yet")]
    NotStarted,
    #[error("already started")]
    AlreadyStarted,
    #[error("finished")]
    Finished,
    #[error("failed")]
    Failed,
    #[error("other error: status 0x{0:x}")]
    OtherError(u32),
}
