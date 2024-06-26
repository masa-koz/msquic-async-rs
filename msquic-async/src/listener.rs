use crate::connection::Connection;

use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use libc::{c_void, sockaddr};
use socket2::SockAddr;
use thiserror::Error;
use tracing::trace;

pub struct Listener(Box<ListenerInner>);

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

impl Listener {
    pub fn new(
        msquic_listener: msquic::Listener,
        registration: &msquic::Registration,
        configuration: msquic::Configuration,
    ) -> Self {
        let inner = Box::new(ListenerInner {
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
        });
        {
            inner.shared.msquic_listener.open(
                registration,
                Self::native_callback,
                &*inner as *const _ as *const c_void,
            ).unwrap();
        }
        Self(inner)
    }

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
        let local_address: Option<SockAddr> = local_address.map(|x| x.into());
        let local_address: Option<*const sockaddr> =
            local_address.as_ref().map(|x| x.as_ptr() as _);
        self.0
            .shared
            .msquic_listener
            .start(alpn.as_ref(), local_address).unwrap();
        exclusive.state = ListenerState::StartComplete;
        Ok(())
    }

    pub fn accept(&self) -> Accept {
        Accept(self)
    }

    pub fn stop(&self) -> Stop {
        Stop(self)
    }

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

    pub fn local_addr(&self) -> Result<SocketAddr, ListenError> {
        unsafe {
            SockAddr::try_init(|addr, len| {
                let status = self.0.shared.msquic_listener.get_param(
                    msquic::PARAM_LISTENER_LOCAL_ADDRESS,
                    len as _,
                    addr as _,
                );
                if msquic::Status::succeeded(status) {
                    Ok(())
                } else {
                    Err(std::io::Error::from(std::io::ErrorKind::Other))
                }
            })
            .map(|((), addr)| addr.as_socket().expect("socketaddr"))
            .map_err(|_| ListenError::Failed)
        }
    }

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

    fn handle_event_new_connection(
        inner: &ListenerInner,
        payload: &msquic::ListenerEventNewConnection,
    ) -> u32 {
        trace!("Listener({:p}) new connection event", inner);

        let new_conn = Connection::from_handle(payload.connection);
        new_conn.set_configuration(&inner.shared.configuration);

        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.new_connections.push_back(new_conn);
        exclusive
            .new_connection_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        msquic::QUIC_STATUS_SUCCESS
    }

    fn handle_event_stop_complete(
        inner: &ListenerInner,
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
        let inner = unsafe { &mut *(context as *mut ListenerInner) };
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

pub struct Accept<'a>(&'a Listener);

impl Future for Accept<'_> {
    type Output = Result<Connection, ListenError>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.poll_accept(cx)
    }
}

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
}
