use crate::buffer::WriteBuffer;
use crate::registration::{Registration, RundownGuard, RundownState};
use crate::stream::{ReadStream, StartError as StreamStartError, Stream, StreamType};
use crate::sync::{register_waker, LockPoisonTolerant};

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
        register_waker(&mut exclusive.start_waiters, cx);
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
        register_waker(&mut exclusive.start_waiters, cx);
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
                register_waker(&mut exclusive.start_waiters, cx);
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
        register_waker(&mut exclusive.inbound_stream_waiters, cx);
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
                register_waker(&mut exclusive.start_waiters, cx);
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
        register_waker(&mut exclusive.inbound_uni_stream_waiters, cx);
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
                register_waker(&mut exclusive.start_waiters, cx);
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
            register_waker(&mut exclusive.recv_waiters, cx);
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
                register_waker(&mut exclusive.start_waiters, cx);
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
                register_waker(&mut exclusive.start_waiters, cx);
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

        register_waker(&mut exclusive.shutdown_waiters, cx);
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

    /// Set the local address of the connection.
    ///
    /// Only valid on a client connection. Before [`Connection::start()`] this just
    /// records the address to bind to; afterwards it migrates the connection, which
    /// requires the handshake to be confirmed. Calling it on a server, on a locally
    /// closed connection, or after start but before handshake confirmation fails with
    /// `QUIC_STATUS_INVALID_STATE`.
    pub fn set_local_addr(&self, addr: SocketAddr) -> Result<(), ConnectionError> {
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_LOCAL_ADDRESS,
                std::mem::size_of::<msquic::Addr>() as u32,
                &msquic::Addr::from(addr) as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// Set the remote address of the connection.
    ///
    /// Only valid on a client connection, before [`Connection::start()`]. Setting it
    /// makes `start`'s host argument a *name* only: msquic skips resolving it and
    /// still uses it for SNI and certificate validation. That matters because msquic
    /// resolves the name with a blocking `getaddrinfo` on the connection's worker
    /// thread, which stalls every other connection on that worker for as long as the
    /// resolver takes — so a caller that already knows the address should say so.
    ///
    /// A wildcard address, a server connection, or a call after start fails with
    /// `QUIC_STATUS_INVALID_PARAMETER` / `QUIC_STATUS_INVALID_STATE`.
    pub fn set_remote_addr(&self, addr: SocketAddr) -> Result<(), ConnectionError> {
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_REMOTE_ADDRESS,
                std::mem::size_of::<msquic::Addr>() as u32,
                &msquic::Addr::from(addr) as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// How many events of this kind are waiting to be polled. Lets a test assert on
    /// what the queue holds without draining it, which is how coalescing is checked.
    #[cfg(test)]
    pub(crate) fn queued_event_count(&self, matching: impl Fn(&ConnectionEvent) -> bool) -> usize {
        self.0
            .exclusive
            .lock_poison_tolerant()
            .events
            .iter()
            .filter(|event| matching(event))
            .count()
    }

    /// Get connection statistics (RTT, byte counters, loss, etc.).
    pub fn get_stats(&self) -> Result<msquic::ffi::QUIC_STATISTICS, ConnectionError> {
        self.0
            .msquic_conn
            .get_stats()
            .map_err(ConnectionError::OtherError)
    }

    /// Get statistics for each of the connection's paths.
    ///
    /// One entry per path, carrying that path's `PathId`, smoothed/min/max RTT, MTU and
    /// network statistics. [`Connection::get_stats()`] and MsQuic's connection-wide
    /// network statistics only ever describe the first path, so this is the way to see
    /// the others on a connection built up with [`Connection::add_path()`]. It works
    /// with or without multipath negotiated — a single-path connection returns one
    /// entry.
    ///
    /// Paths with no path ID yet are not reported, having nothing to identify them by
    /// and no congestion control to read. That means a path added before the connection
    /// reaches its `Connected` state: [`Connection::add_path()`] leaves it pending, and
    /// it gains an id when the handshake is confirmed. One added after that is opened —
    /// and reported — straight away.
    ///
    /// Two properties of the entries are worth knowing, both of them MsQuic's:
    ///
    /// - **`PathId` does not identify an entry on its own.** Without multipath
    ///   negotiated the core gives every added path the first path's path ID outright,
    ///   so entries share an id for the connection's whole life; with multipath, a
    ///   rebinding path is given the id of the one it replaces without it being
    ///   cleared, so two entries carry the same id for the duration. Either way all of
    ///   them are real paths, and sharing an id means sharing congestion control, so
    ///   their `NetworkStatistics` agreeing is expected; `Rtt`, `MinRtt`, `MaxRtt` and
    ///   `Mtu` are per path and tell them apart. A caller keying a map on `PathId`
    ///   alone will lose paths.
    /// - `MinRtt` and `MaxRtt` are zero until the path has produced an RTT sample,
    ///   rather than carrying the sentinel `QUIC_STATISTICS_V2` exposes. `Rtt` starts
    ///   from the configured initial RTT.
    ///
    /// The path count is not known ahead of time and changes over the connection's
    /// life, so this starts from a buffer big enough for the paths MsQuic allows and
    /// grows it if MsQuic ever asks for more, retrying a bounded number of times.
    #[cfg(feature = "msquic-seera")]
    pub fn get_path_statistics(
        &self,
    ) -> Result<Vec<msquic::ffi::QUIC_PATH_STATISTICS>, ConnectionError> {
        const ENTRY_SIZE: usize = std::mem::size_of::<msquic::ffi::QUIC_PATH_STATISTICS>();
        /// QUIC_MAX_PATH_COUNT in the core, so the first call is normally the only one.
        const INITIAL_ENTRIES: usize = 4;
        /// Bounded because growing is for a path appearing mid-call, not for a
        /// connection that keeps adding them.
        const ATTEMPTS: usize = 4;

        // SAFETY: the handle is only used for the `get_param` calls below, which do not
        // outlive this borrow of the connection.
        let handle = unsafe { self.0.msquic_conn.as_raw() };
        let mut entries = INITIAL_ENTRIES;
        let mut last_status = None;

        for _ in 0..ATTEMPTS {
            let mut stats = Vec::<msquic::ffi::QUIC_PATH_STATISTICS>::with_capacity(entries);
            let capacity_bytes = (entries * ENTRY_SIZE) as u32;
            let mut length = capacity_bytes;

            // SAFETY: `stats` has room for `length` bytes, and MsQuic writes at most
            // that, reporting in `length` how much it wrote. On a short buffer it
            // writes nothing and reports what it needs instead.
            let result = unsafe {
                msquic::Api::get_param(
                    handle,
                    msquic::ffi::QUIC_PARAM_CONN_PATH_STATISTICS,
                    std::ptr::addr_of_mut!(length) as *const u32,
                    stats.as_mut_ptr() as *mut c_void,
                )
            };

            match result {
                Ok(()) => {
                    // SAFETY: MsQuic wrote `length` bytes of initialized entries, and
                    // `length` on success is what it wrote, which is bounded by the
                    // capacity passed in.
                    unsafe { stats.set_len(length.min(capacity_bytes) as usize / ENTRY_SIZE) };
                    return Ok(stats);
                }
                // Grow and retry when MsQuic asked for more room than was offered —
                // which is the only reason it rewrites `length` upwards. The status is
                // deliberately not consulted: QUIC_STATUS_BUFFER_TOO_SMALL is EOVERFLOW
                // on POSIX, whose value differs between Linux and macOS while these
                // bindings carry the Linux one on both, so matching on it would work on
                // one platform and not the other.
                Err(status) => {
                    if length > capacity_bytes {
                        entries = length as usize / ENTRY_SIZE;
                        last_status = Some(status);
                    } else {
                        return Err(ConnectionError::OtherError(status));
                    }
                }
            }
        }

        Err(ConnectionError::OtherError(last_status.expect(
            "the loop only continues after recording the status it grew on",
        )))
    }

    /// Validate the peer's certificate yourself.
    ///
    /// The handler runs during the handshake, from the MsQuic thread, before the
    /// connection is established — so it has to be set before [`Connection::start()`].
    /// Without a handler every certificate that reaches this event is accepted.
    ///
    /// MsQuic only raises the event when the configuration's credentials carry
    /// `CredentialFlags::INDICATE_CERTIFICATE_RECEIVED`. Pair it with
    /// `NO_CERTIFICATE_VALIDATION` to replace MsQuic's own checks, or with
    /// `DEFER_CERTIFICATE_VALIDATION` to let them run first and report their verdict
    /// through the `deferred_*` arguments.
    ///
    /// # Verdict
    ///
    /// `Ok(())` accepts. `Err(status)` rejects **only if `status` is one MsQuic
    /// classifies as a failure** — the status is passed through untouched, so
    /// `Err(Status(QUIC_STATUS_SUCCESS))` accepts just as `Ok(())` does, and
    /// `Err(Status(QUIC_STATUS_PENDING))` leaves validation outstanding until the
    /// connection times out, because the completion call that would answer it is not
    /// exposed. Reject with something like `QUIC_STATUS_BAD_CERTIFICATE`.
    ///
    /// Asynchronous validation — returning `QUIC_STATUS_PENDING` and answering later
    /// through MsQuic's `certificate_validation_complete()` — is not wired up here; the
    /// handler has to reach its verdict before it returns.
    ///
    /// # Arguments
    ///
    /// The four arguments are the event's fields, passed through as MsQuic gives them:
    ///
    /// - `certificate`, the peer's certificate, **which may be null**. Its type is
    ///   platform specific: a `QUIC_BUFFER*` holding the DER encoding when the
    ///   credentials carry `USE_PORTABLE_CERTIFICATES`, and otherwise the platform's own
    ///   handle — an OpenSSL `X509*`, a Schannel `PCCERT_CONTEXT`. Passing that flag is
    ///   what makes the certificate parseable without platform-specific code.
    /// - `deferred_error_flags`, a bitmask of the errors MsQuic's own validation found.
    ///   Schannel only; zero on every other platform, whatever the credential flags.
    /// - `deferred_status`, the most severe of those errors. Unlike the flags this is
    ///   filled on OpenSSL too, whenever `DEFER_CERTIFICATE_VALIDATION` or
    ///   `REQUIRE_CLIENT_AUTHENTICATION` is set, and is then the only field carrying
    ///   MsQuic's verdict.
    /// - `chain`, the certificate chain, **which may also be null** — including when
    ///   `certificate` is not. It is not the same shape as `certificate`: with
    ///   `USE_PORTABLE_CERTIFICATES` it is a `QUIC_BUFFER*` holding a **PKCS#7** blob
    ///   rather than a bare certificate, and without the flag an OpenSSL
    ///   `X509_STORE_CTX*` or a Schannel `HCERTSTORE`.
    ///
    /// A peer that sent no certificate reaches the handler with nulls rather than being
    /// rejected first, since the check that would have rejected it is the validation
    /// `NO_CERTIFICATE_VALIDATION` turns off. Check before dereferencing.
    ///
    /// Both pointers are owned by MsQuic and valid only for the duration of the call, so
    /// anything needed afterwards has to be copied out.
    ///
    /// # What the handler must not do
    ///
    /// It must not capture this [`Connection`], or any clone of it. The handler is
    /// stored inside the connection, so capturing one makes a reference cycle: the
    /// connection is never dropped, `ConnectionClose` is never called, and the
    /// registration's rundown never completes — failing at shutdown, far from the cause.
    /// Capture what it needs (a channel, a root store) instead.
    ///
    /// It must not call [`Connection::set_peer_certificate_received_callback()`] on this
    /// same connection, which would deadlock on the handler's own lock. Other methods
    /// are fine: the handler is kept under a lock of its own, not the one the rest of
    /// the connection uses. It should still return promptly — it runs on a MsQuic worker
    /// thread, and blocking there on network I/O (OCSP, CRL) stalls that worker.
    ///
    /// # Server side
    ///
    /// Effectively client-only today. On a connection from [`crate::Listener::accept()`]
    /// the handshake is already under way by the time the application receives it, so
    /// with `REQUIRE_CLIENT_AUTHENTICATION` the event can be raised before a handler
    /// can be installed — and an event with no handler accepts. Handing the handler in
    /// through the listener, so it is in place before the connection is indicated, is
    /// left to a later change.
    pub fn set_peer_certificate_received_callback<F>(&self, handler: F)
    where
        F: FnMut(*mut c_void, u32, msquic::Status, *mut c_void) -> Result<(), msquic::Status>
            + 'static
            + Send,
    {
        *self
            .0
            .peer_certificate_received_callback
            .lock_poison_tolerant() = Some(Box::new(handler));
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

    /// Set whether to use the unconnected UDP socket.
    ///
    /// Must be called on a client connection before [`Connection::start()`], and only
    /// after [`Connection::set_share_binding(true)`](Connection::set_share_binding):
    /// an unconnected socket receives datagrams from any remote address, so the
    /// connection has to be identifiable by its connection ID alone, which requires a
    /// non-zero length source connection ID that only a shared binding gives it.
    /// Otherwise this fails with `QUIC_STATUS_INVALID_STATE`.
    ///
    /// It also requires a specific local address, set with
    /// [`Connection::set_local_addr()`](Connection::set_local_addr). A connected socket
    /// takes its source address from the kernel when it is connected; an unconnected one
    /// does not, and the connection's first packet goes out before anything has been
    /// learned from the peer, so the address to send from has to be named. Starting a
    /// connection with no local address, or a wildcard one (`0.0.0.0` / `::`), fails with
    /// `QUIC_STATUS_INVALID_PARAMETER`; the port may be left as 0 to let the stack choose
    /// one. The same requirement applies to every path added with
    /// [`Connection::add_path()`](Connection::add_path).
    #[cfg(feature = "msquic-seera")]
    pub fn set_unconnected_socket(&self, unconnected: bool) -> Result<(), ConnectionError> {
        let unconnected: u8 = if unconnected { 1 } else { 0 };
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_UNCONNECTED_UDP_SOCKET,
                std::mem::size_of::<u8>() as u32,
                &unconnected as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// Add a new path to the connection.
    ///
    /// When the connection was configured with
    /// [`Connection::set_unconnected_socket(true)`](Connection::set_unconnected_socket),
    /// `local_addr` has to name a specific address for the same reason the connection's
    /// own local address does: an unconnected socket has no source address of its own,
    /// and the path's first packet goes out before anything has been learned from the
    /// peer. A wildcard address (`0.0.0.0` / `::`) fails with
    /// `QUIC_STATUS_INVALID_PARAMETER`; the port may be left as 0 to let the stack choose
    /// one. Given a specific address, the path shares the connection's binding — and so
    /// its local port — rather than opening one of its own.
    ///
    /// The address is checked when the path's UDP binding is opened, which happens here
    /// once the handshake has completed, and otherwise on handshake completion. A path
    /// added before [`Connection::start()`] becomes the connection's first path, whose
    /// address `start()` itself checks.
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

    /// Declare a path available or backup to the peer.
    ///
    /// `path_id` is the one carried by [`ConnectionEvent::PathAdded`]. Marking a path
    /// active sends a PATH_AVAILABLE frame, marking it inactive a PATH_BACKUP one; a
    /// call that does not change the status sends nothing.
    ///
    /// Requires multipath to have been negotiated, and fails with
    /// `QUIC_STATUS_INVALID_STATE` otherwise. An unknown `path_id` — one never seen, or
    /// belonging to a path since removed — fails with `QUIC_STATUS_INVALID_PARAMETER`.
    ///
    /// There is no counterpart for reading the status back: `QUIC_PARAM_CONN_PATH_STATUS`
    /// is set-only in the core, which answers a get with `QUIC_STATUS_INVALID_PARAMETER`.
    /// Track it from what this call sets and from [`ConnectionEvent::PathStatusChanged`],
    /// which reports the peer's declarations about a path.
    #[cfg(feature = "msquic-seera")]
    pub fn set_path_status(&self, path_id: u32, active: bool) -> Result<(), ConnectionError> {
        unsafe {
            msquic::Api::set_param(
                self.0.msquic_conn.as_raw(),
                msquic::ffi::QUIC_PARAM_CONN_PATH_STATUS,
                std::mem::size_of::<msquic::ffi::QUIC_PATH_STATUS>() as u32,
                &msquic::ffi::QUIC_PATH_STATUS {
                    PathId: path_id,
                    Active: if active { 1 } else { 0 },
                } as *const _ as *const _,
            )
        }
        .map_err(ConnectionError::OtherError)
    }

    /// Add a bound address to the connection.
    ///
    /// A UDP socket is bound to `addr` as given, and the address is advertised to the
    /// peer in an ADD_ADDRESS frame. Passing port 0 requests an ephemeral port, which
    /// is read back off the binding and recorded as the advertised address. Note that
    /// the address is bound as specified rather than on a dual-stack wildcard socket,
    /// so an IPv4 address yields an IPv4-only socket.
    ///
    /// Requires server migration to have been negotiated on a client, and not to have
    /// been negotiated on a server; on a locally closed connection, an address already
    /// bound on this connection, or more than 128 bound addresses, this fails with
    /// `QUIC_STATUS_INVALID_STATE`, `QUIC_STATUS_ADDRESS_IN_USE` or
    /// `QUIC_STATUS_OUT_OF_MEMORY` respectively.
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
                register_waker(&mut exclusive.start_waiters, cx);
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
            register_waker(&mut exclusive.event_waiters, cx);
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

/// Handler installed by [`Connection::set_peer_certificate_received_callback()`],
/// which documents the arguments. Boxed because it is stored per connection, and
/// `Send` because it runs on a MsQuic thread rather than the one that set it.
type PeerCertificateReceivedCallback = dyn FnMut(*mut c_void, u32, msquic::Status, *mut c_void) -> Result<(), msquic::Status>
    + 'static
    + Send;

struct ConnectionInner {
    exclusive: Mutex<ConnectionInnerExclusive>,
    /// Kept here, rather than only on `ConnectionInstance`, so the callback
    /// context can reserve for peer-initiated streams.
    rundown: Arc<RundownState>,
    /// Deliberately its own lock rather than a field of `exclusive`. The handler
    /// is arbitrary application code called while this is held, so putting it in
    /// `exclusive` would mean every `Connection` method it touched deadlocked the
    /// MsQuic thread inside the TLS callback.
    peer_certificate_received_callback: Mutex<Option<Box<PeerCertificateReceivedCallback>>>,
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
            peer_certificate_received_callback: Mutex::new(None),
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

    /// Hand the peer's certificate to the application's handler, if it set one.
    ///
    /// The status returned here is the event's status, which MsQuic reads as the
    /// verdict: anything failing rejects the certificate and fails the handshake. With
    /// no handler installed the event succeeds, leaving whatever validation the
    /// credentials asked for as the only check.
    ///
    /// The handler's own lock is held across the call, which is what makes the `&mut`
    /// borrow sound — MsQuic can raise this on any of its threads. That lock guards
    /// nothing else, so a handler is free to use the rest of this `Connection`; only
    /// installing another handler from inside one would deadlock.
    fn handle_event_peer_certificate_received(
        &self,
        certificate: *mut c_void,
        deferred_error_flags: u32,
        deferred_status: msquic::Status,
        chain: *mut c_void,
    ) -> Result<(), msquic::Status> {
        trace!("ConnectionInner({:p}) PeerCertificateReceived", self);
        if let Some(callback) = &mut *self
            .peer_certificate_received_callback
            .lock_poison_tolerant()
        {
            callback(certificate, deferred_error_flags, deferred_status, chain)
        } else {
            Ok(())
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
        // Queued as well as recorded: send_datagram() reads the state, but a sender
        // that wants to size its datagrams to the limit, or to know when sending
        // became possible at all, has no way to see it change otherwise.
        //
        // Coalesced onto whichever entry is still waiting, rather than appended.
        // Only the newest state means anything — an older entry describes a limit
        // that no longer applies — and every connection reaches here, on every
        // backend, several times as MTU discovery probes upwards. Appending would
        // grow the queue for the whole life of a connection whose application never
        // calls poll_event(), which is what h3-msquic-async does.
        let queued = exclusive.events.iter_mut().find_map(|event| match event {
            ConnectionEvent::DatagramStateChanged {
                send_enabled,
                max_send_length,
            } => Some((send_enabled, max_send_length)),
            _ => None,
        });
        match queued {
            Some((queued_enabled, queued_length)) => {
                *queued_enabled = send_enabled;
                *queued_length = max_send_length;
            }
            None => exclusive
                .events
                .push_back(ConnectionEvent::DatagramStateChanged {
                    send_enabled,
                    max_send_length,
                }),
        }
        exclusive
            .event_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
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

    /// Queue one of the three multipath path events, which carry the same
    /// addresses and differ only in what they say about the path.
    #[cfg(feature = "msquic-seera")]
    fn handle_event_path(
        &self,
        name: &str,
        local_address: &msquic::Addr,
        peer_address: &msquic::Addr,
        path_id: u32,
        make_event: impl FnOnce(SocketAddr, SocketAddr) -> ConnectionEvent,
    ) -> Result<(), msquic::Status> {
        let (Some(local_address), Some(peer_address)) =
            (local_address.as_socket(), peer_address.as_socket())
        else {
            error!(
                "ConnectionInner({:p}) {} with non-socket address",
                self, name
            );
            return Ok(());
        };
        trace!(
            "ConnectionInner({:p}) {} path_id:{} local_address:{} peer_address:{}",
            self,
            name,
            path_id,
            local_address,
            peer_address
        );
        let mut exclusive = self.exclusive.lock_poison_tolerant();
        exclusive
            .events
            .push_back(make_event(local_address, peer_address));
        exclusive
            .event_waiters
            .drain(..)
            .for_each(|waker| waker.wake());
        Ok(())
    }

    #[cfg(feature = "msquic-seera")]
    fn handle_event_path_added(
        &self,
        local_address: &msquic::Addr,
        peer_address: &msquic::Addr,
        path_id: u32,
    ) -> Result<(), msquic::Status> {
        self.handle_event_path(
            "path added",
            local_address,
            peer_address,
            path_id,
            |local_address, peer_address| ConnectionEvent::PathAdded {
                local_address,
                peer_address,
                path_id,
            },
        )
    }

    #[cfg(feature = "msquic-seera")]
    fn handle_event_path_removed(
        &self,
        local_address: &msquic::Addr,
        peer_address: &msquic::Addr,
        path_id: u32,
    ) -> Result<(), msquic::Status> {
        self.handle_event_path(
            "path removed",
            local_address,
            peer_address,
            path_id,
            |local_address, peer_address| ConnectionEvent::PathRemoved {
                local_address,
                peer_address,
                path_id,
            },
        )
    }

    #[cfg(feature = "msquic-seera")]
    fn handle_event_path_status_changed(
        &self,
        local_address: &msquic::Addr,
        peer_address: &msquic::Addr,
        path_id: u32,
        is_active: bool,
    ) -> Result<(), msquic::Status> {
        self.handle_event_path(
            "path status changed",
            local_address,
            peer_address,
            path_id,
            |local_address, peer_address| ConnectionEvent::PathStatusChanged {
                local_address,
                peer_address,
                path_id,
                is_active,
            },
        )
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
            msquic::ConnectionEvent::PeerCertificateReceived {
                certificate,
                deferred_error_flags,
                deferred_status,
                chain,
            } => self.handle_event_peer_certificate_received(
                certificate,
                deferred_error_flags,
                deferred_status,
                chain,
            ),
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
            #[cfg(feature = "msquic-seera")]
            msquic::ConnectionEvent::PathAdded {
                peer_address,
                local_address,
                path_id,
            } => self.handle_event_path_added(local_address, peer_address, path_id),
            #[cfg(feature = "msquic-seera")]
            msquic::ConnectionEvent::PathRemoved {
                peer_address,
                local_address,
                path_id,
            } => self.handle_event_path_removed(local_address, peer_address, path_id),
            #[cfg(feature = "msquic-seera")]
            msquic::ConnectionEvent::PathStatusChanged {
                peer_address,
                local_address,
                path_id,
                is_active,
            } => self.handle_event_path_status_changed(
                local_address,
                peer_address,
                path_id,
                is_active,
            ),
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
///
/// Marked `#[non_exhaustive]`: MsQuic gains events, and this enum follows it, so a
/// match on it needs a catch-all arm to keep compiling.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
    /// A path has been added to the connection.
    ///
    /// Indicated when a path completes validation while multipath is
    /// negotiated; the path is active at that point.
    PathAdded {
        local_address: SocketAddr,
        peer_address: SocketAddr,
        path_id: u32,
    },
    /// A path has been removed from the connection.
    ///
    /// Indicated when the peer abandons the path, and when a path validation
    /// times out.
    PathRemoved {
        local_address: SocketAddr,
        peer_address: SocketAddr,
        path_id: u32,
    },
    /// The peer has declared a path available or backup.
    ///
    /// This reports the peer's view: it follows a PATH_AVAILABLE or PATH_BACKUP
    /// frame arriving. Setting the status locally with
    /// `Connection::set_path_status()` does not raise it. That method is not linked
    /// here because it only exists on the `msquic-seera` backend, while this variant,
    /// like the other backend-specific ones, is always present.
    PathStatusChanged {
        local_address: SocketAddr,
        peer_address: SocketAddr,
        path_id: u32,
        is_active: bool,
    },
    /// What the connection may send as a datagram has changed.
    ///
    /// Unlike the variants above, which the seera backend alone raises, this one
    /// comes from an event every backend has. MsQuic evaluates it when the peer's
    /// transport parameters arrive — that is where the peer's willingness to
    /// receive datagrams, and its size limit, are settled once for the connection —
    /// and again on every path MTU change, which is what moves the number
    /// afterwards. Expect a run of these while MTU discovery probes upwards.
    ///
    /// The connection acts on this itself: the same numbers are what
    /// [`Connection::send_datagram()`] checks a datagram against, refusing it with
    /// `DgramSendError::Denied` when sending is disabled and
    /// `DgramSendError::TooBig` when it is over the length. Observing the event is
    /// for a sender that would rather size its datagrams, or wait until it may send
    /// at all, than have them rejected.
    ///
    /// It narrows that window rather than closing it. `send_datagram()` reads the
    /// current state while this carries the state at the time it was raised, so a
    /// consumer behind on its events can still size to a stale number — the limit
    /// having grown is harmless, having shrunk is a `TooBig` on a datagram that
    /// looked small enough. Draining the queue before acting keeps that as narrow
    /// as it can be.
    ///
    /// `send_enabled` is false when the peer has not advertised datagram support,
    /// and `max_send_length` is the largest datagram that will be accepted.
    DatagramStateChanged {
        send_enabled: bool,
        max_send_length: u16,
    },
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
                register_waker(&mut exclusive.start_waiters, cx);
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
