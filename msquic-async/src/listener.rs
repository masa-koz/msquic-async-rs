use crate::connection::Connection;

use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use libc::c_void;
use thiserror::Error;
use tracing::trace;

/// Listener for incoming connections.
pub struct Listener(Arc<ListenerInner>);

impl Listener {
    /// Create a new listener.
    pub fn new(
        msquic_listener: msquic::Listener,
        registration: &msquic::Registration,
        configuration: msquic::Configuration,
    ) -> Result<Self, ListenError> {
        let mut inner = Arc::new(ListenerInner::new(msquic_listener, configuration));
        Arc::get_mut(&mut inner)
            .unwrap()
            .shared
            .msquic_listener
            .open(
                registration,
                Some(ListenerInner::native_callback),
                std::ptr::null(),
            )
            .map_err(ListenError::OtherError)?;
        unsafe {
            msquic::Api::set_callback_handler(
                inner.shared.msquic_listener.as_raw(),
                ListenerInner::native_callback as *const c_void,
                Arc::into_raw(inner.clone()) as *const c_void,
            );
        }
        Ok(Self(inner))
    }

    /// Start the listener.
    pub fn start<T: AsRef<[msquic::BufferRef]>>(
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
            .map_err(ListenError::OtherError)?;
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

impl Drop for Listener {
    fn drop(&mut self) {
        self.0.shared.msquic_listener.stop();
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
        &self,
        _info: msquic::NewConnectionInfo<'_>,
        connection: msquic::Connection,
    ) -> msquic::StatusCode {
        trace!("Listener({:p}) new connection event", self);

        if let Err(status) = connection.set_configuration(&self.shared.configuration) {
            return status.try_as_status_code().unwrap();
        }
        let new_conn = Connection::from_raw(unsafe { connection.into_raw() });

        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive.new_connections.push_back(new_conn);
        exclusive
            .new_connection_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    fn handle_event_stop_complete(&self, _app_close_in_progress: bool) -> msquic::StatusCode {
        trace!("Listener({:p}) stop complete", self);
        {
            let mut exclusive = self.exclusive.lock().unwrap();
            exclusive.state = ListenerState::ShutdownComplete;

            exclusive
                .new_connection_waiters
                .drain(..)
                .for_each(|waker| waker.wake());

            exclusive
                .shutdown_complete_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
        }
        unsafe {
            Arc::from_raw(self as *const _);
        }
        msquic::StatusCode::QUIC_STATUS_SUCCESS
    }

    extern "C" fn native_callback(
        _listener: msquic::ffi::HQUIC,
        context: *mut c_void,
        event: *mut msquic::ffi::QUIC_LISTENER_EVENT,
    ) -> msquic::ffi::QUIC_STATUS {
        let inner = unsafe { &*(context as *const Self) };
        let ev_ref = unsafe { event.as_ref().unwrap() };
        let event = msquic::ListenerEvent::from(ev_ref);

        let res = match event {
            msquic::ListenerEvent::NewConnection { info, connection } => {
                inner.handle_event_new_connection(info, connection)
            }
            msquic::ListenerEvent::StopComplete {
                app_close_in_progress,
            } => inner.handle_event_stop_complete(app_close_in_progress),
        };
        res.into()
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

#[derive(Debug, Error, Clone)]
pub enum ListenError {
    #[error("Not started yet")]
    NotStarted,
    #[error("already started")]
    AlreadyStarted,
    #[error("finished")]
    Finished,
    #[error("failed")]
    Failed,
    #[error("other error: status {0:?}")]
    OtherError(msquic::Status),
}
