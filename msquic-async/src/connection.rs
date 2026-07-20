use crate::buffer::WriteBuffer;
use crate::registration::{Registration, RundownGuard, RundownState};
use crate::stream::{ReadStream, StartError as StreamStartError, Stream, StreamType};
use crate::sync::LockPoisonTolerant;

#[cfg(feature = "msquic-2-5")]
use msquic_v2_5 as msquic;
#[cfg(feature = "msquic-seera")]
use seera_msquic as msquic;

use std::collections::VecDeque;
use std::fs::File;
use std::future::Future;
use std::io::{Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::ops::Deref;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use libc::c_void;
use msquic::ffi::QUIC_TLS_SECRETS__bindgen_ty_1;
use thiserror::Error;
use tracing::{error, info, trace};

#[derive(Clone)]
pub struct Connection(Arc<ConnectionInstance>);

impl Connection {
    /// Create a new connection.
    ///
    /// The connection is not started until `start` is called.
    pub fn new(registration: &Registration) -> Result<Self, ConnectionError> {
        Self::new_common(registration, false)
    }

    /// Create a new QMUX connection.
    ///
    /// The connection is not started until `start` is called.
    ///
    /// Only available with the `msquic-seera` backend, which is the only one
    /// that exposes `ConnectionOpenQmux`.
    #[cfg(feature = "msquic-seera")]
    pub fn new_qmux(registration: &Registration) -> Result<Self, ConnectionError> {
        Self::new_common(registration, true)
    }

    fn new_common(
        registration: &Registration,
        #[cfg_attr(not(feature = "msquic-seera"), allow(unused_variables))] is_qmux: bool,
    ) -> Result<Self, ConnectionError> {
        let inner = Arc::new(ConnectionInner::new(
            ConnectionState::Open,
            None,
            None,
            registration.state().clone(),
        ));
        let inner_in_ev = inner.clone();
        // Reserve before opening: `QuicConnRegister` acquires the registration
        // rundown during `ConnectionOpen`, so reserving first leaves no window
        // in which a live native handle is untracked. If `open` fails, this
        // guard drops and releases the reservation.
        let guard = RundownGuard::new(registration.state().clone());
        // `is_qmux` can only be true on the seera backend: `new_qmux` is gated
        // on it, so the other backends never reach the QMUX branch.
        #[cfg(feature = "msquic-seera")]
        let open_result = if is_qmux {
            msquic::Connection::open_qmux(registration.raw(), move |conn_ref, ev| {
                inner_in_ev.callback_handler_impl(conn_ref, ev)
            })
        } else {
            msquic::Connection::open(registration.raw(), move |conn_ref, ev| {
                inner_in_ev.callback_handler_impl(conn_ref, ev)
            })
        };
        #[cfg(not(feature = "msquic-seera"))]
        let open_result = msquic::Connection::open(registration.raw(), move |conn_ref, ev| {
            inner_in_ev.callback_handler_impl(conn_ref, ev)
        });
        let msquic_conn = open_result.map_err(ConnectionError::OtherError)?;
        let instance = Arc::new(ConnectionInstance {
            inner,
            msquic_conn,
            _guard: guard,
        });
        trace!(
            "ConnectionInstance({:p}, Inner: {:p}) Open by local",
            instance,
            instance.inner
        );
        Ok(Self(instance))
    }

    pub(crate) fn from_raw(
        #[cfg(feature = "msquic-2-5")] handle: msquic::ffi::HQUIC,
        #[cfg(not(feature = "msquic-2-5"))] msquic_conn: msquic::Connection,
        tls_secrets: Option<Box<msquic::ffi::QUIC_TLS_SECRETS>>,
        sslkeylog_file: Option<File>,
        guard: RundownGuard,
    ) -> Self {
        #[cfg(feature = "msquic-2-5")]
        let msquic_conn = unsafe { msquic::Connection::from_raw(handle) };
        let inner = Arc::new(ConnectionInner::new(
            ConnectionState::Connecting,
            tls_secrets,
            sslkeylog_file,
            guard.state().clone(),
        ));
        let inner_in_ev = inner.clone();
        msquic_conn.set_callback_handler(move |conn_ref, ev| {
            inner_in_ev.callback_handler_impl(conn_ref, ev)
        });
        let instance = Arc::new(ConnectionInstance {
            inner,
            msquic_conn,
            _guard: guard,
        });
        trace!(
            "ConnectionInstance({:p}, Inner: {:p}) Open by peer",
            instance,
            instance.inner
        );
        Self(instance)
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
    pub fn poll_start(
        &self,
        cx: &mut Context<'_>,
        configuration: &msquic::Configuration,
        host: &str,
        port: u16,
    ) -> Poll<Result<(), StartError>> {
        let mut exclusive = self.0.exclusive.lock_poison_tolerant();
        match exclusive.state {
            ConnectionState::Open => {
                self.0
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

    /// Poll to wait connection started. Mainly used for connections created by peer.
    pub fn poll_wait_start(&self, cx: &mut Context<'_>) -> Poll<Result<(), StartError>> {
        let mut exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open => {
                return Poll::Ready(Err(StartError::ConnectionNotStarted));
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
        let mut exclusive = self.0.exclusive.lock_poison_tolerant();
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
        let mut exclusive = self.0.exclusive.lock_poison_tolerant();
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
        let mut exclusive = self.0.exclusive.lock_poison_tolerant();
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
        let mut exclusive = self.0.exclusive.lock_poison_tolerant();
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

        if !exclusive.dgram_send_enabled {
            return Poll::Ready(Err(DgramSendError::Denied));
        }
        if buf.len() > exclusive.dgram_max_send_length as usize {
            return Poll::Ready(Err(DgramSendError::TooBig));
        }

        Poll::Ready(exclusive.send_datagram(&self.0.msquic_conn, buf))
    }

    /// Send a datagram.
    pub fn send_datagram(&self, buf: &Bytes) -> Result<(), DgramSendError> {
        let mut exclusive = self.0.exclusive.lock_poison_tolerant();
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

        if !exclusive.dgram_send_enabled {
            return Err(DgramSendError::Denied);
        }
        if buf.len() > exclusive.dgram_max_send_length as usize {
            return Err(DgramSendError::TooBig);
        }

        exclusive.send_datagram(&self.0.msquic_conn, buf)
    }

    /// Poll to shutdown the connection.
    pub fn poll_shutdown(
        &self,
        cx: &mut Context<'_>,
        error_code: u64,
    ) -> Poll<Result<(), ShutdownError>> {
        let mut exclusive = self.0.exclusive.lock_poison_tolerant();
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
                    .msquic_conn
                    .shutdown(msquic::ConnectionShutdownFlags::NONE, error_code);
                exclusive.state = ConnectionState::Shutdown;
                exclusive.error = Some(ConnectionError::ShutdownByLocal);
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
        let mut exclusive = self.0.exclusive.lock_poison_tolerant();
        match exclusive.state {
            ConnectionState::Open | ConnectionState::Connecting => {
                return Err(ShutdownError::ConnectionNotStarted);
            }
            ConnectionState::Connected => {
                self.0
                    .msquic_conn
                    .shutdown(msquic::ConnectionShutdownFlags::NONE, error_code);
                exclusive.state = ConnectionState::Shutdown;
                exclusive.error = Some(ConnectionError::ShutdownByLocal);
            }
            _ => {}
        }
        Ok(())
    }

    /// Get the local address of the connection.
    pub fn get_local_addr(&self) -> Result<SocketAddr, ConnectionError> {
        self.0
            .msquic_conn
            .get_local_addr()
            .map(|addr| addr.as_socket().expect("socket addr"))
            .map_err(ConnectionError::OtherError)
    }

    /// Get the remote address of the connection.
    pub fn get_remote_addr(&self) -> Result<SocketAddr, ConnectionError> {
        self.0
            .msquic_conn
            .get_remote_addr()
            .map(|addr| addr.as_socket().expect("socket addr"))
            .map_err(ConnectionError::OtherError)
    }

    /// Set whether to share the UDP binding.
    pub fn set_share_binding(&self, share: bool) -> Result<(), ConnectionError> {
        let share: u8 = if share { 1 } else { 0 };
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_SHARE_UDP_BINDING,
                std::mem::size_of::<u8>() as u32,
                &share as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// Add a new path to the connection.
    #[cfg(feature = "msquic-seera")]
    pub fn add_path(
        &self,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> Result<(), ConnectionError> {
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_ADD_PATH,
                std::mem::size_of::<msquic::ffi::QUIC_PATH_PARAM>() as u32,
                &msquic::ffi::QUIC_PATH_PARAM {
                    LocalAddress: &mut msquic::Addr::from(local_addr) as *mut _ as *mut _,
                    RemoteAddress: &mut msquic::Addr::from(remote_addr) as *mut _ as *mut _,
                } as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// Activate a path for the connection.
    #[cfg(feature = "msquic-seera")]
    pub fn activate_path(
        &self,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> Result<(), ConnectionError> {
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_ACTIVATE_PATH,
                std::mem::size_of::<msquic::ffi::QUIC_PATH_PARAM>() as u32,
                &msquic::ffi::QUIC_PATH_PARAM {
                    LocalAddress: &mut msquic::Addr::from(local_addr) as *mut _ as *mut _,
                    RemoteAddress: &mut msquic::Addr::from(remote_addr) as *mut _ as *mut _,
                } as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// Remove a path from the connection.
    #[cfg(feature = "msquic-seera")]
    pub fn remove_path(
        &self,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> Result<(), ConnectionError> {
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_REMOVE_PATH,
                std::mem::size_of::<msquic::ffi::QUIC_PATH_PARAM>() as u32,
                &msquic::ffi::QUIC_PATH_PARAM {
                    LocalAddress: &mut msquic::Addr::from(local_addr) as *mut _ as *mut _,
                    RemoteAddress: &mut msquic::Addr::from(remote_addr) as *mut _ as *mut _,
                } as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// Add a bound address to the connection.
    #[cfg(feature = "msquic-seera")]
    pub fn add_bound_addr(&self, addr: SocketAddr) -> Result<(), ConnectionError> {
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_ADD_BOUND_ADDRESS,
                std::mem::size_of::<msquic::Addr>() as u32,
                &msquic::Addr::from(addr) as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// Add an observed address to the connection.
    #[cfg(feature = "msquic-seera")]
    pub fn add_observed_addr(
        &self,
        addr: SocketAddr,
        observed_addr: SocketAddr,
    ) -> Result<(), ConnectionError> {
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_ADD_OBSERVED_ADDRESS,
                std::mem::size_of::<msquic::ffi::QUIC_ADD_OBSERVED_ADDRESS>() as u32,
                &msquic::ffi::QUIC_ADD_OBSERVED_ADDRESS {
                    LocalAddress: &mut msquic::Addr::from(addr) as *mut _ as *mut _,
                    ObservedAddress: &mut msquic::Addr::from(observed_addr) as *mut _ as *mut _,
                } as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// Remove a bound address from the connection.
    #[cfg(feature = "msquic-seera")]
    pub fn remove_bound_addr(&self, addr: SocketAddr) -> Result<(), ConnectionError> {
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_REMOVE_BOUND_ADDRESS,
                std::mem::size_of::<msquic::Addr>() as u32,
                &msquic::Addr::from(addr) as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// Add a candidate address to the connection.
    #[cfg(feature = "msquic-seera")]
    pub fn add_candidate_addr(
        &self,
        host_addr: SocketAddr,
        observed_addr: SocketAddr,
    ) -> Result<(), ConnectionError> {
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_ADD_CANDIDATE_ADDRESS,
                std::mem::size_of::<msquic::ffi::QUIC_CANDIDATE_ADDRESS>() as u32,
                &msquic::ffi::QUIC_CANDIDATE_ADDRESS {
                    HostAddress: &mut msquic::Addr::from(host_addr) as *mut _ as *mut _,
                    ObservedAddress: &mut msquic::Addr::from(observed_addr) as *mut _ as *mut _,
                } as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// Remove a candidate address from the connection.
    #[cfg(feature = "msquic-seera")]
    pub fn remove_candidate_addr(
        &self,
        host_addr: SocketAddr,
        observed_addr: SocketAddr,
    ) -> Result<(), ConnectionError> {
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_REMOVE_CANDIDATE_ADDRESS,
                std::mem::size_of::<msquic::ffi::QUIC_CANDIDATE_ADDRESS>() as u32,
                &msquic::ffi::QUIC_CANDIDATE_ADDRESS {
                    HostAddress: &mut msquic::Addr::from(host_addr) as *mut _ as *mut _,
                    ObservedAddress: &mut msquic::Addr::from(observed_addr) as *mut _ as *mut _,
                } as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// Poll to receive events on the connection.
    pub fn poll_event(&self, cx: &mut Context<'_>) -> Poll<Result<ConnectionEvent, EventError>> {
        let mut exclusive = self.0.exclusive.lock_poison_tolerant();
        match exclusive.state {
            ConnectionState::Open => {
                return Poll::Ready(Err(EventError::ConnectionNotStarted));
            }
            ConnectionState::Connecting => {
                exclusive.start_waiters.push(cx.waker().clone());
                return Poll::Pending;
            }
            ConnectionState::Connected | ConnectionState::Shutdown => {}
            ConnectionState::ShutdownComplete => {
                return Poll::Ready(Err(EventError::ConnectionLost(
                    exclusive.error.as_ref().expect("error").clone(),
                )));
            }
        }

        if exclusive.events.is_empty() {
            exclusive.event_waiters.push(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(Ok(exclusive.events.pop_front().unwrap()))
        }
    }

    /// Set the SSL key log file for the connection.
    pub fn set_sslkeylog_file(&self, file: File) -> Result<(), ConnectionError> {
        let mut exclusive = self.0.exclusive.lock_poison_tolerant();
        if exclusive.sslkeylog_file.is_some() {
            return Err(ConnectionError::SslKeyLogFileAlreadySet);
        }
        if exclusive.tls_secrets.is_none() {
            exclusive.tls_secrets = Some(Box::new(msquic::ffi::QUIC_TLS_SECRETS {
                SecretLength: 0,
                ClientRandom: [0; 32],
                IsSet: QUIC_TLS_SECRETS__bindgen_ty_1 {
                    _bitfield_align_1: [0; 0],
                    _bitfield_1: QUIC_TLS_SECRETS__bindgen_ty_1::new_bitfield_1(
                        0u8, 0u8, 0u8, 0u8, 0u8, 0u8,
                    ),
                },
                ClientEarlyTrafficSecret: [0; 64],
                ClientHandshakeTrafficSecret: [0; 64],
                ServerHandshakeTrafficSecret: [0; 64],
                ClientTrafficSecret0: [0; 64],
                ServerTrafficSecret0: [0; 64],
            }));
            unsafe {
                msquic::Api::set_param(
                    self.0.msquic_conn.as_raw(),
                    msquic::ffi::QUIC_PARAM_CONN_TLS_SECRETS,
                    std::mem::size_of::<msquic::ffi::QUIC_TLS_SECRETS>() as u32,
                    exclusive.tls_secrets.as_ref().unwrap().as_ref() as *const _ as *const _,
                )
            }
            .map_err(ConnectionError::OtherError)?;
        }
        exclusive.sslkeylog_file = Some(file);
        Ok(())
    }

    /// Send a TLS resumption ticket to the peer.
    ///
    /// Only available with the `msquic-seera` backend, which is the only one
    /// that exposes `ConnectionSendResumptionTicket`.
    #[cfg(feature = "msquic-seera")]
    pub fn send_resumption_ticket(
        &self,
        is_final: bool,
        resumption_app_data: Option<&[u8]>,
    ) -> Result<(), ConnectionError> {
        let exclusive = self.0.exclusive.lock().unwrap();
        match exclusive.state {
            ConnectionState::Open | ConnectionState::Connecting => {
                return Err(ConnectionError::ConnectionNotStarted);
            }
            ConnectionState::Connected => {}
            ConnectionState::Shutdown | ConnectionState::ShutdownComplete => {
                return Err(exclusive.error.as_ref().expect("error").clone());
            }
        }
        self.0
            .msquic_conn
            .send_resumption_ticket(
                if is_final {
                    msquic::ConnectionSendResumptionFlags::FINAL
                } else {
                    msquic::ConnectionSendResumptionFlags::NONE
                },
                resumption_app_data,
            )
            .map_err(ConnectionError::OtherError)
    }

    /// Set the resumption ticket for the connection.
    pub fn set_resumption_ticket(&self, resumption_ticket: &[u8]) -> Result<(), ConnectionError> {
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_RESUMPTION_TICKET,
                resumption_ticket.len() as u32,
                resumption_ticket.as_ptr() as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }
}

struct ConnectionInstance {
    inner: Arc<ConnectionInner>,
    msquic_conn: msquic::Connection,
    // Declared last so that, on drop, `msquic_conn`'s `ConnectionClose` (which
    // releases the native registration rundown reference) runs before this
    // guard decrements and wakes `Registration::wait_idle` waiters.
    //
    // The guard lives here rather than on `Connection` because `Connection` is
    // `Clone`: only the last `Arc<ConnectionInstance>` closes the handle.
    _guard: RundownGuard,
}

impl Deref for ConnectionInstance {
    type Target = ConnectionInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Drop for ConnectionInstance {
    fn drop(&mut self) {
        trace!("ConnectionInstance({:p}) dropping", self);
    }
}

struct ConnectionInner {
    exclusive: Mutex<ConnectionInnerExclusive>,
    /// Kept here, rather than only on `ConnectionInstance`, so the callback
    /// context can reserve for peer-initiated streams.
    rundown: Arc<RundownState>,
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
    dgram_send_enabled: bool,
    dgram_max_send_length: u16,
    shutdown_waiters: Vec<Waker>,
    events: VecDeque<ConnectionEvent>,
    event_waiters: Vec<Waker>,
    sslkeylog_file: Option<File>,
    tls_secrets: Option<Box<msquic::ffi::QUIC_TLS_SECRETS>>,
}

impl ConnectionInnerExclusive {
    /// Sends `buf` as a datagram, reclaiming the send buffer if MsQuic rejects
    /// the send.
    ///
    /// Ownership of the buffer is handed to MsQuic as a raw pointer and returned
    /// via the DatagramSendStateChanged callback. On an error MsQuic never took
    /// it and no callback fires, so it must be reclaimed here to avoid leaking
    /// the buffer (and the `Bytes` it holds) on every failed send — otherwise a
    /// peer that forces sends to fail could drive unbounded memory growth.
    fn send_datagram(
        &mut self,
        msquic_conn: &msquic::Connection,
        buf: &Bytes,
    ) -> Result<(), DgramSendError> {
        let mut write_buf = self.write_pool.pop().unwrap_or_else(WriteBuffer::new);
        let _ = write_buf.put_zerocopy(buf);
        let buffers = unsafe {
            let (data, len) = write_buf.get_buffers();
            std::slice::from_raw_parts(data, len)
        };
        let raw = write_buf.into_raw();
        match unsafe {
            msquic_conn.datagram_send(buffers, msquic::SendFlags::NONE, raw as *const _)
        }
        .map_err(DgramSendError::OtherError)
        {
            Ok(()) => Ok(()),
            Err(e) => {
                let mut write_buf = unsafe { WriteBuffer::from_raw(raw) };
                write_buf.reset();
                self.write_pool.push(write_buf);
                Err(e)
            }
        }
    }
}

impl ConnectionInner {
    fn new(
        state: ConnectionState,
        tls_secrets: Option<Box<msquic::ffi::QUIC_TLS_SECRETS>>,
        sslkeylog_file: Option<File>,
        rundown: Arc<RundownState>,
    ) -> Self {
        Self {
            rundown,
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
                dgram_send_enabled: false,
                dgram_max_send_length: 0,
                shutdown_waiters: Vec::new(),
                events: VecDeque::new(),
                event_waiters: Vec::new(),
                sslkeylog_file,
                tls_secrets,
            }),
        }
    }

    fn handle_event_connected(
        &self,
        _session_resumed: bool,
        _negotiated_alpn: &[u8],
    ) -> Result<(), msquic::Status> {
        trace!("ConnectionInner({:p}) Connected", self);

        let mut exclusive = self.exclusive.lock_poison_tolerant();
        match (
            exclusive.tls_secrets.take(),
            exclusive.sslkeylog_file.take(),
        ) {
            (Some(tls_secrets), Some(mut file)) => {
                info!("ConnectionInner({:p}) Writing TLS secrets to file", self);
                let client_random = if tls_secrets.IsSet.ClientRandom() != 0 {
                    hex::encode(tls_secrets.ClientRandom)
                } else {
                    String::new()
                };

                // `SecretLength` is supplied by MsQuic; clamp it to the actual
                // array capacity so a bogus value can never cause an
                // out-of-bounds slice panic.
                let secret_len = (tls_secrets.SecretLength as usize)
                    .min(msquic::ffi::QUIC_TLS_SECRETS_MAX_SECRET_LEN as usize);

                let _ = file.seek(SeekFrom::End(0));

                if tls_secrets.IsSet.ClientEarlyTrafficSecret() != 0 {
                    let _ = writeln!(
                        file,
                        "CLIENT_EARLY_TRAFFIC_SECRET {} {}",
                        client_random,
                        hex::encode(&tls_secrets.ClientEarlyTrafficSecret[0..secret_len])
                    );
                }

                if tls_secrets.IsSet.ClientHandshakeTrafficSecret() != 0 {
                    let _ = writeln!(
                        file,
                        "CLIENT_HANDSHAKE_TRAFFIC_SECRET {} {}",
                        client_random,
                        hex::encode(&tls_secrets.ClientHandshakeTrafficSecret[0..secret_len])
                    );
                }

                if tls_secrets.IsSet.ServerHandshakeTrafficSecret() != 0 {
                    let _ = writeln!(
                        file,
                        "SERVER_HANDSHAKE_TRAFFIC_SECRET {} {}",
                        client_random,
                        hex::encode(&tls_secrets.ServerHandshakeTrafficSecret[0..secret_len])
                    );
                }

                if tls_secrets.IsSet.ClientTrafficSecret0() != 0 {
                    let _ = writeln!(
                        file,
                        "CLIENT_TRAFFIC_SECRET_0 {} {}",
                        client_random,
                        hex::encode(&tls_secrets.ClientTrafficSecret0[0..secret_len])
                    );
                }

                if tls_secrets.IsSet.ServerTrafficSecret0() != 0 {
                    let _ = writeln!(
                        file,
                        "SERVER_TRAFFIC_SECRET_0 {} {}",
                        client_random,
                        hex::encode(&tls_secrets.ServerTrafficSecret0[0..secret_len])
                    );
                }
                exclusive.tls_secrets = Some(tls_secrets);
            }
            _ => { /* do nothing */ }
        }
        exclusive.state = ConnectionState::Connected;
        exclusive
            .start_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        Ok(())
    }

    fn handle_event_shutdown_initiated_by_transport(
        &self,
        status: msquic::Status,
        error_code: u64,
    ) -> Result<(), msquic::Status> {
        trace!(
            "ConnectionInner({:p}) Transport shutdown {:?}",
            self,
            status
        );

        let mut exclusive = self.exclusive.lock_poison_tolerant();
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
        exclusive
            .recv_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        Ok(())
    }

    fn handle_event_shutdown_initiated_by_peer(
        &self,
        error_code: u64,
    ) -> Result<(), msquic::Status> {
        trace!("ConnectionInner({:p}) App shutdown {}", self, error_code);

        let mut exclusive = self.exclusive.lock_poison_tolerant();
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
        exclusive
            .recv_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        Ok(())
    }

    fn handle_event_shutdown_complete(
        &self,
        handshake_completed: bool,
        peer_acknowledged_shutdown: bool,
        app_close_in_progress: bool,
    ) -> Result<(), msquic::Status> {
        trace!("ConnectionInner({:p}) Shutdown complete: handshake_completed={}, peer_acknowledged_shutdown={}, app_close_in_progress={}",
            self, handshake_completed, peer_acknowledged_shutdown, app_close_in_progress
        );

        {
            let mut exclusive = self.exclusive.lock_poison_tolerant();
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
                .recv_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
            exclusive
                .shutdown_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
            exclusive
                .event_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
        }
        Ok(())
    }

    fn handle_event_peer_stream_started(
        &self,
        stream: msquic::StreamRef,
        flags: msquic::StreamOpenFlags,
    ) -> Result<(), msquic::Status> {
        let stream_type = if (flags & msquic::StreamOpenFlags::UNIDIRECTIONAL)
            == msquic::StreamOpenFlags::UNIDIRECTIONAL
        {
            StreamType::Unidirectional
        } else {
            StreamType::Bidirectional
        };
        trace!(
            "ConnectionInner({:p}) Peer stream started {:?}",
            self,
            stream_type
        );

        let stream = Stream::from_raw(
            unsafe { stream.as_raw() },
            stream_type,
            RundownGuard::new(self.rundown.clone()),
        );
        if (flags & msquic::StreamOpenFlags::UNIDIRECTIONAL)
            == msquic::StreamOpenFlags::UNIDIRECTIONAL
        {
            if let (Some(read_stream), None) = stream.split() {
                let mut exclusive = self.exclusive.lock_poison_tolerant();
                exclusive.inbound_uni_streams.push_back(read_stream);
                exclusive
                    .inbound_uni_stream_waiters
                    .drain(..)
                    .for_each(|waker| waker.wake());
            } else {
                // A unidirectional stream opened by the peer must always split
                // into exactly a read half. This should be unreachable, but a
                // callback must never panic across the FFI boundary, so reject
                // the stream with an error status instead of aborting.
                error!(
                    "ConnectionInner({:p}) peer unidirectional stream did not split into a read stream",
                    self
                );
                return Err(msquic::StatusCode::QUIC_STATUS_INTERNAL_ERROR.into());
            }
        } else {
            {
                let mut exclusive = self.exclusive.lock_poison_tolerant();
                exclusive.inbound_streams.push_back(stream);
                exclusive
                    .inbound_stream_waiters
                    .drain(..)
                    .for_each(|waker| waker.wake());
            }
        }

        Ok(())
    }

    fn handle_event_streams_available(
        &self,
        bidirectional_count: u16,
        unidirectional_count: u16,
    ) -> Result<(), msquic::Status> {
        trace!(
            "ConnectionInner({:p}) Streams available bidirectional_count:{} unidirectional_count:{}",
            self,
            bidirectional_count,
            unidirectional_count
        );
        Ok(())
    }

    fn handle_event_datagram_state_changed(
        &self,
        send_enabled: bool,
        max_send_length: u16,
    ) -> Result<(), msquic::Status> {
        trace!(
            "ConnectionInner({:p}) Datagram state changed send_enabled:{} max_send_length:{}",
            self,
            send_enabled,
            max_send_length
        );
        let mut exclusive = self.exclusive.lock_poison_tolerant();
        exclusive.dgram_send_enabled = send_enabled;
        exclusive.dgram_max_send_length = max_send_length;
        Ok(())
    }

    fn handle_event_datagram_received(
        &self,
        buffer: &msquic::BufferRef,
        _flags: msquic::ReceiveFlags,
    ) -> Result<(), msquic::Status> {
        trace!("ConnectionInner({:p}) Datagram received", self);
        let buf = Bytes::copy_from_slice(buffer.as_bytes());
        {
            let mut exclusive = self.exclusive.lock_poison_tolerant();
            exclusive.recv_buffers.push_back(buf);
            exclusive
                .recv_waiters
                .drain(..)
                .for_each(|waker| waker.wake());
        }
        Ok(())
    }

    fn handle_event_datagram_send_state_changed(
        &self,
        client_context: *const c_void,
        state: msquic::DatagramSendState,
    ) -> Result<(), msquic::Status> {
        trace!(
            "ConnectionInner({:p}) Datagram send state changed state:{:?}",
            self,
            state
        );
        match state {
            msquic::DatagramSendState::Sent | msquic::DatagramSendState::Canceled => {
                let mut write_buf = unsafe { WriteBuffer::from_raw(client_context) };
                let mut exclusive = self.exclusive.lock_poison_tolerant();
                write_buf.reset();
                exclusive.write_pool.push(write_buf);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_event_resumption_ticket_received(
        &self,
        _resumption_ticket: &[u8],
    ) -> Result<(), msquic::Status> {
        trace!("ConnectionInner({:p}) Resumption ticket received", self);
        let mut exclusive = self.exclusive.lock().unwrap();
        exclusive
            .events
            .push_back(ConnectionEvent::ResumptionTicketReceived {
                resumption_ticket: _resumption_ticket.to_vec(),
            });
        exclusive
            .event_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        Ok(())
    }

    #[cfg(feature = "msquic-seera")]
    fn handle_event_notify_observed_address(
        &self,
        local_address: &msquic::Addr,
        observed_address: &msquic::Addr,
    ) -> Result<(), msquic::Status> {
        let (Some(local_address), Some(observed_address)) =
            (local_address.as_socket(), observed_address.as_socket())
        else {
            error!(
                "ConnectionInner({:p}) Notify observed address with non-socket address",
                self
            );
            return Ok(());
        };
        trace!(
            "ConnectionInner({:p}) Notify observed address local_address:{} observed_address:{}",
            self,
            local_address,
            observed_address
        );
        let mut exclusive = self.exclusive.lock_poison_tolerant();
        exclusive
            .events
            .push_back(ConnectionEvent::NotifyObservedAddress {
                local_address,
                observed_address,
            });
        exclusive
            .event_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        Ok(())
    }

    #[cfg(feature = "msquic-seera")]
    fn handle_event_notify_remote_address_added(
        &self,
        address: &msquic::Addr,
        sequence_number: u64,
    ) -> Result<(), msquic::Status> {
        let Some(address) = address.as_socket() else {
            error!(
                "ConnectionInner({:p}) Notify remote address added with non-socket address",
                self
            );
            return Ok(());
        };
        trace!(
            "ConnectionInner({:p}) Notify remote address added address:{} sequence_number:{}",
            self,
            address,
            sequence_number
        );
        let mut exclusive = self.exclusive.lock_poison_tolerant();
        exclusive
            .events
            .push_back(ConnectionEvent::NotifyRemoteAddressAdded {
                address,
                sequence_number,
            });
        exclusive
            .event_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        Ok(())
    }

    #[cfg(feature = "msquic-seera")]
    fn handle_event_path_validated(
        &self,
        local_address: &msquic::Addr,
        remote_address: &msquic::Addr,
    ) -> Result<(), msquic::Status> {
        let (Some(local_address), Some(remote_address)) =
            (local_address.as_socket(), remote_address.as_socket())
        else {
            error!(
                "ConnectionInner({:p}) path validated with non-socket address",
                self
            );
            return Ok(());
        };
        trace!(
            "ConnectionInner({:p}) path validated local_address:{} remote_address:{}",
            self,
            local_address,
            remote_address
        );
        let mut exclusive = self.exclusive.lock_poison_tolerant();
        exclusive.events.push_back(ConnectionEvent::PathValidated {
            local_address,
            remote_address,
        });
        exclusive
            .event_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        Ok(())
    }

    #[cfg(feature = "msquic-seera")]
    fn handle_event_notify_remote_address_removed(
        &self,
        sequence_number: u64,
    ) -> Result<(), msquic::Status> {
        trace!(
            "ConnectionInner({:p}) Notify remote address removed sequence_number:{}",
            self,
            sequence_number
        );
        let mut exclusive = self.exclusive.lock_poison_tolerant();
        exclusive
            .events
            .push_back(ConnectionEvent::NotifyRemoteAddressRemoved { sequence_number });
        exclusive
            .event_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        Ok(())
    }

    fn callback_handler_impl(
        &self,
        connection: msquic::ConnectionRef,
        ev: msquic::ConnectionEvent,
    ) -> Result<(), msquic::Status> {
        // This runs on a MsQuic-owned thread, invoked through an `extern "C"`
        // trampoline. A panic unwinding across that FFI boundary is undefined
        // behavior, so contain any panic here and turn it into an error status.
        catch_unwind(AssertUnwindSafe(|| self.dispatch_event(connection, ev))).unwrap_or_else(
            |_| {
                error!("ConnectionInner({:p}) panic in callback handler", self);
                Err(msquic::StatusCode::QUIC_STATUS_INTERNAL_ERROR.into())
            },
        )
    }

    fn dispatch_event(
        &self,
        _connection: msquic::ConnectionRef,
        ev: msquic::ConnectionEvent,
    ) -> Result<(), msquic::Status> {
        match ev {
            msquic::ConnectionEvent::Connected {
                session_resumed,
                negotiated_alpn,
            } => self.handle_event_connected(session_resumed, negotiated_alpn),
            msquic::ConnectionEvent::ShutdownInitiatedByTransport { status, error_code } => {
                self.handle_event_shutdown_initiated_by_transport(status, error_code)
            }
            msquic::ConnectionEvent::ShutdownInitiatedByPeer { error_code } => {
                self.handle_event_shutdown_initiated_by_peer(error_code)
            }
            msquic::ConnectionEvent::ShutdownComplete {
                handshake_completed,
                peer_acknowledged_shutdown,
                app_close_in_progress,
            } => self.handle_event_shutdown_complete(
                handshake_completed,
                peer_acknowledged_shutdown,
                app_close_in_progress,
            ),
            msquic::ConnectionEvent::PeerStreamStarted { stream, flags } => {
                self.handle_event_peer_stream_started(stream, flags)
            }
            msquic::ConnectionEvent::StreamsAvailable {
                bidirectional_count,
                unidirectional_count,
            } => self.handle_event_streams_available(bidirectional_count, unidirectional_count),
            msquic::ConnectionEvent::DatagramStateChanged {
                send_enabled,
                max_send_length,
            } => self.handle_event_datagram_state_changed(send_enabled, max_send_length),
            msquic::ConnectionEvent::DatagramReceived { buffer, flags } => {
                self.handle_event_datagram_received(buffer, flags)
            }
            msquic::ConnectionEvent::DatagramSendStateChanged {
                client_context,
                state,
            } => self.handle_event_datagram_send_state_changed(client_context, state),
            msquic::ConnectionEvent::ResumptionTicketReceived { resumption_ticket } => {
                self.handle_event_resumption_ticket_received(resumption_ticket)
            }
            #[cfg(feature = "msquic-seera")]
            msquic::ConnectionEvent::NotifyObservedAddress {
                local_address,
                observed_address,
            } => self.handle_event_notify_observed_address(local_address, observed_address),
            #[cfg(feature = "msquic-seera")]
            msquic::ConnectionEvent::NotifyRemoteAddressAdded {
                address,
                sequence_number,
            } => self.handle_event_notify_remote_address_added(address, sequence_number),
            #[cfg(feature = "msquic-seera")]
            msquic::ConnectionEvent::PathValidated {
                local_address,
                remote_address,
            } => self.handle_event_path_validated(local_address, remote_address),
            #[cfg(feature = "msquic-seera")]
            msquic::ConnectionEvent::NotifyRemoteAddressRemoved { sequence_number } => {
                self.handle_event_notify_remote_address_removed(sequence_number)
            }
            _ => {
                trace!("ConnectionInner({:p}) Other callback", self);
                Ok(())
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

/// Events that can occur on a connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionEvent {
    /// A new observed address has been detected.
    NotifyObservedAddress {
        local_address: SocketAddr,
        observed_address: SocketAddr,
    },
    /// A new remote address has been added.
    NotifyRemoteAddressAdded {
        address: SocketAddr,
        sequence_number: u64,
    },
    /// A path has been validated.
    PathValidated {
        local_address: SocketAddr,
        remote_address: SocketAddr,
    },
    /// A remote address has been removed.
    NotifyRemoteAddressRemoved { sequence_number: u64 },
    /// Resumption ticket has been received from the peer.
    ResumptionTicketReceived { resumption_ticket: Vec<u8> },
}

/// Errors that can occur when managing a connection.
#[derive(Debug, Error, Clone)]
pub enum ConnectionError {
    #[error("connection not started yet")]
    ConnectionNotStarted,
    #[error("connection shutdown by transport: status {0:?}, error 0x{1:x}")]
    ShutdownByTransport(msquic::Status, u64),
    #[error("connection shutdown by peer: error 0x{0:x}")]
    ShutdownByPeer(u64),
    #[error("connection shutdown by local")]
    ShutdownByLocal,
    #[error("connection closed")]
    ConnectionClosed,
    #[error("SSL key log file already set")]
    SslKeyLogFileAlreadySet,
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
    #[error("connection not started yet")]
    ConnectionNotStarted,
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

/// Errors that can occur when receiving events on a connection.
#[derive(Debug, Error, Clone)]
pub enum EventError {
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
    conn: &'a ConnectionInstance,
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

        let mut exclusive = conn.inner.exclusive.lock_poison_tolerant();
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
            match Stream::open(
                &conn.msquic_conn,
                stream_type.take().unwrap(),
                RundownGuard::new(conn.rundown.clone()),
            ) {
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
