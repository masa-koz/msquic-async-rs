use crate::connection::Connection;
use crate::MSQUIC_API;

use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use libc::{c_void, sockaddr};

use socket2::SockAddr;

use thiserror::Error;

pub struct Listener(Box<ListenerInner>);

struct ListenerInner {
    exclusive: Mutex<ListenerInnerExclusive>,
    shared: ListenerInnerShared,
}

struct ListenerInnerExclusive {
    msquic_listener: msquic::Listener,
    state: ListenerState,
    new_connections: VecDeque<Connection>,
    new_connection_waiters: Vec<Waker>,
    shutdown_complete_waiters: Vec<Waker>,
}
unsafe impl Sync for ListenerInnerExclusive {}
unsafe impl Send for ListenerInnerExclusive {}

struct ListenerInnerShared {
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
                msquic_listener,
                state: ListenerState::Open,
                new_connections: VecDeque::new(),
                new_connection_waiters: Vec::new(),
                shutdown_complete_waiters: Vec::new(),
            }),
            shared: ListenerInnerShared {
                configuration,
            },
        });
        {
            let exclusive = inner.exclusive.lock().unwrap();
            exclusive.msquic_listener.open(
                registration,
                Self::native_callback,
                &*inner as *const _ as *const c_void,
            );
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
        let local_address: Option<*const sockaddr> = local_address.as_ref().map(|x| x.as_ptr() as _);
        exclusive
            .msquic_listener
            .start(alpn.as_ref(), local_address);
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

    pub fn poll_stop(&self, cx: &mut Context<'_>) -> Poll<Result<(), ListenError>> {
        let mut msquic_listener_handle = None;
        {
            let mut exclusive = self.0.exclusive.lock().unwrap();

            match exclusive.state {
                ListenerState::Open => {
                    return Poll::Ready(Err(ListenError::NotStarted));
                }
                ListenerState::StartComplete => {
                    msquic_listener_handle = Some(exclusive.msquic_listener.handle);
                    exclusive.state = ListenerState::Shutdown;
                }
                ListenerState::Shutdown => {}
                ListenerState::ShutdownComplete => {
                    return Poll::Ready(Ok(()));
                }
            }
            exclusive.shutdown_complete_waiters.push(cx.waker().clone());
        }
        if let Some(msquic_listener_handle) = msquic_listener_handle {
            MSQUIC_API.listener_stop(msquic_listener_handle);
        }
        Poll::Pending
    }

    fn handle_event_new_connection(
        inner: &ListenerInner,
        payload: &msquic::ListenerEventNewConnection,
    ) -> u32 {
        println!("new connection");

        let new_conn = Connection::from_handle(payload.connection);
        new_conn.set_configuration(&inner.shared.configuration);

        let mut exclusive = inner.exclusive.lock().unwrap();
        exclusive.new_connections.push_back(new_conn);
        exclusive
            .new_connection_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        0
    }

    fn handle_event_stop_complete(
        inner: &ListenerInner,
        _payload: &msquic::ListenerEventStopComplete,
    ) -> u32 {
        println!("stop complete");
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
        0
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
}
