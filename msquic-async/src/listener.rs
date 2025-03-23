use crate::connection::Connection;

use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll, Waker};

use thiserror::Error;
use tracing::trace;

/// Listener for incoming connections.
pub struct Listener(Arc<ListenerInner>);

impl Listener {
    /// Create a new listener.
    pub fn new(
        registration: &msquic::Registration,
        configuration: msquic::Configuration,
    ) -> Result<Self, ListenError> {
        let mut msquic_listener = msquic::Listener::new();
        let inner = Arc::new(ListenerInner::new(configuration));
        let inner_in_ev = inner.clone();
        let _ = Arc::into_raw(inner.clone());
        msquic_listener
            .open(registration, move |_, ev| match ev {
                msquic::ListenerEvent::NewConnection { info, connection } => {
                    inner_in_ev.handle_event_new_connection(info, connection)
                }
                msquic::ListenerEvent::StopComplete {
                    app_close_in_progress,
                } => inner_in_ev.handle_event_stop_complete(app_close_in_progress),
            })
            .map_err(ListenError::OtherError)?;
        *inner.shared.msquic_listener.write().unwrap() = Some(msquic_listener);
        trace!("Listener({:p}) new", inner);
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
            .read()
            .unwrap()
            .as_ref()
            .expect("msquic_listener set")
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
        trace!("Listener({:p}) poll_accept", self);
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
        trace!("Listener({:p}) poll_stop", self);
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
            self.0
                .shared
                .msquic_listener
                .read()
                .unwrap()
                .as_ref()
                .expect("msquic_listener set")
                .stop();
        }
        Poll::Pending
    }

    /// Get the local address the listener is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, ListenError> {
        self.0
            .shared
            .msquic_listener
            .read()
            .unwrap()
            .as_ref()
            .expect("msquic_listener set")
            .get_local_addr()
            .map(|addr| addr.as_socket().expect("not a socket address"))
            .map_err(|_| ListenError::Failed)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        trace!("Listener({:p}) dropping", self.0);
        self.0
            .shared
            .msquic_listener
            .read()
            .unwrap()
            .as_ref()
            .expect("msquic_listner set")
            .stop();
        {
            let mut exclusive = self.0.shared.msquic_listener.write().unwrap();
            let msquic_listener = exclusive.take();
            drop(exclusive);
            drop(msquic_listener);
            let mut exclusive = self.0.shared.configuration.write().unwrap();
            let configuration = exclusive.take();
            drop(exclusive);
            drop(configuration);
        }
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
    configuration: RwLock<Option<msquic::Configuration>>,
    msquic_listener: RwLock<Option<msquic::Listener>>,
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
    fn new(configuration: msquic::Configuration) -> Self {
        Self {
            exclusive: Mutex::new(ListenerInnerExclusive {
                state: ListenerState::Open,
                new_connections: VecDeque::new(),
                new_connection_waiters: Vec::new(),
                shutdown_complete_waiters: Vec::new(),
            }),
            shared: ListenerInnerShared {
                configuration: RwLock::new(Some(configuration)),
                msquic_listener: RwLock::new(None),
            },
        }
    }

    fn handle_event_new_connection(
        &self,
        _info: msquic::NewConnectionInfo<'_>,
        connection: msquic::ConnectionRef,
    ) -> Result<(), msquic::Status> {
        trace!("Listener({:p}) New connection", self);

        connection
            .set_configuration(self.shared.configuration.read().unwrap().as_ref().unwrap())?;
        let new_conn = Connection::from_raw(unsafe { connection.as_raw() });

        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive.new_connections.push_back(new_conn);
        exclusive
            .new_connection_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        Ok(())
    }

    fn handle_event_stop_complete(
        &self,
        app_close_in_progress: bool,
    ) -> Result<(), msquic::Status> {
        trace!(
            "Listener({:p}) Stop complete: app_close_in_progress={}",
            self,
            app_close_in_progress
        );
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
            trace!(
                "Listener({:p}) new_connections's len={}",
                self,
                exclusive.new_connections.len()
            );
        }
        // unsafe {
        //     Arc::from_raw(self as *const _);
        // }
        Ok(())
    }
}

impl Drop for ListenerInner {
    fn drop(&mut self) {
        trace!("ListenerInner({:p}) dropping", self);
        trace!(
            "msquic_listener: {:?}",
            self.shared.msquic_listener.read().unwrap()
        );
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
