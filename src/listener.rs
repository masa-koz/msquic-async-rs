use crate::connection::Connection;

use libc::c_void;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

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
}
unsafe impl Sync for ListenerInnerExclusive {}
unsafe impl Send for ListenerInnerExclusive {}

struct ListenerInnerShared {
    configuration: msquic::Configuration,
}

#[derive(Debug, Clone, PartialEq)]
enum ListenerState {
    Open,
    StartComplete,
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
        local_address: &msquic::Addr,
    ) -> Result<(), ListenError> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ListenerState::Open => {}
            ListenerState::StartComplete => {
                return Err(ListenError::AlreadyStarted);
            }
            ListenerState::ShutdownComplete => {
                return Err(ListenError::Finished);
            }
        }
        exclusive
            .msquic_listener
            .start(alpn.as_ref(), local_address);
        exclusive.state = ListenerState::StartComplete;
        Ok(())
    }

    pub fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<Result<Connection, ListenError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ListenerState::Open => {
                return Poll::Ready(Err(ListenError::NotStarted));
            }
            ListenerState::StartComplete => {}
            ListenerState::ShutdownComplete => {
                return Poll::Ready(Err(ListenError::Finished));
            }
        }
        if !exclusive.new_connections.is_empty() {
            return Poll::Ready(Ok(exclusive.new_connections.pop_front().unwrap()));
        }
        exclusive.new_connection_waiters.push(cx.waker().clone());
        Poll::Pending
    }

    pub fn close(&self) {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        exclusive.msquic_listener.close();
        exclusive.state = ListenerState::ShutdownComplete;
        exclusive
            .new_connection_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
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
            _ => {
                println!("Other callback {}", event.event_type);
                0
            }
        }
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        println!("Listener dropped");
        {
            let exclusive = self.0.exclusive.lock().unwrap();
            if exclusive.state != ListenerState::ShutdownComplete {
                exclusive.msquic_listener.close();
            }
        }
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
