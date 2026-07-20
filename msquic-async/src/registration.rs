#[cfg(feature = "msquic-2-5")]
use msquic_v2_5 as msquic;
#[cfg(feature = "msquic-seera")]
use seera_msquic as msquic;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{fence, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use tracing::{trace, warn};

/// Wakers for all outstanding [`WaitIdle`] futures.
#[derive(Debug, Default)]
struct Waiters {
    next: u64,
    map: HashMap<u64, Waker>,
}

/// Shared rundown-tracking state, owned by a [`Registration`] and cloned into
/// every tracked handle (connections, listeners and streams).
#[derive(Debug, Default)]
pub(crate) struct RundownState {
    /// Outstanding reservations for live tracked handles (connections,
    /// listeners and streams). Reserved before the handle is opened and
    /// released after it is closed, so `active == 0` implies every handle that
    /// must outlive `RegistrationClose` has been closed.
    active: AtomicUsize,
    /// Wakers for all outstanding `wait_idle` futures. Drained and woken when
    /// `active` transitions to 0. Supports any number of concurrent waiters.
    waiters: Mutex<Waiters>,
}

/// RAII guard that holds one unit of a [`RundownState`]'s `active` count.
///
/// Store it as the field declared *after* the MsQuic handle it guards so that,
/// on drop, the handle's Close runs before this guard decrements and wakes
/// waiters.
#[derive(Debug)]
pub(crate) struct RundownGuard {
    state: Arc<RundownState>,
}

impl RundownGuard {
    pub(crate) fn new(state: Arc<RundownState>) -> Self {
        state.active.fetch_add(1, Ordering::Relaxed);
        Self { state }
    }

    /// The state this guard belongs to, so a tracked handle can reserve for the
    /// children it creates.
    pub(crate) fn state(&self) -> &Arc<RundownState> {
        &self.state
    }
}

impl Drop for RundownGuard {
    fn drop(&mut self) {
        // Release so the Close that just ran (on the handle field declared
        // before this guard) is visible to a thread that later observes
        // `active == 0` with Acquire.
        if self.state.active.fetch_sub(1, Ordering::Release) == 1 {
            fence(Ordering::Acquire);
            // Collect under the lock, wake after releasing it.
            let woken: Vec<Waker> = {
                let mut waiters = self.state.waiters.lock().unwrap();
                waiters.map.drain().map(|(_, waker)| waker).collect()
            };
            for waker in woken {
                waker.wake();
            }
        }
    }
}

/// MsQuic `Registration` wrapper that tracks live connections, listeners and
/// streams, so teardown can wait for them before the blocking
/// `RegistrationClose`.
///
/// Dropping a raw [`msquic::Registration`] blocks until every child handle has
/// been closed. Neither `shutdown()` nor the `SHUTDOWN_COMPLETE` /
/// `STOP_COMPLETE` events close a handle, so there is no way to observe that
/// point from the outside. This wrapper counts every [`crate::Connection`],
/// [`crate::Listener`] and [`crate::Stream`] created from it and exposes
/// [`Registration::wait_idle`], which resolves once they are all closed.
///
/// The raw handle is intentionally not public: every rundown-holding handle
/// must be created through a controlled entry point ([`crate::Connection::new`],
/// [`crate::Listener::new`], [`Registration::open_configuration`]) so the count
/// can never be bypassed.
///
/// # Teardown
///
/// ```no_run
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// # use msquic_async::msquic;
/// let registration = msquic_async::Registration::new(&msquic::RegistrationConfig::default())?;
/// let alpn = [msquic::BufferRef::from("sample")];
/// let configuration = registration.open_configuration(&alpn, None)?;
/// // ... use connections and listeners ...
///
/// registration.shutdown();
/// registration.wait_idle().await;
/// drop(configuration);
/// drop(registration);
/// # Ok(())
/// # }
/// ```
pub struct Registration {
    inner: msquic::Registration,
    state: Arc<RundownState>,
}

impl Registration {
    /// Open a registration and its rundown tracking state.
    pub fn new(config: &msquic::RegistrationConfig) -> Result<Self, msquic::Status> {
        let inner = msquic::Registration::new(config)?;
        let registration = Self {
            inner,
            state: Arc::new(RundownState::default()),
        };
        trace!("Registration({:p}) new", registration.state);
        Ok(registration)
    }

    /// Queue shutdown on all connections and stop all listeners.
    ///
    /// Non-blocking, and closes nothing: pair it with [`Registration::wait_idle`].
    pub fn shutdown(&self) {
        trace!("Registration({:p}) shutdown", self.state);
        self.inner.shutdown();
    }

    /// Open a [`msquic::Configuration`] against this registration.
    ///
    /// Configurations are deliberately *not* tracked by [`Registration::wait_idle`].
    /// `ConfigurationClose` only drops the application's reference, while each
    /// connection holds its own; the native rundown reference is released when
    /// that refcount reaches zero. A tracked configuration would therefore
    /// report idle too early. Drop the returned `Configuration` after
    /// `wait_idle()` resolves and before dropping the `Registration`.
    pub fn open_configuration(
        &self,
        alpn: &[msquic::BufferRef],
        settings: Option<&msquic::Settings>,
    ) -> Result<msquic::Configuration, msquic::Status> {
        msquic::Configuration::open(&self.inner, alpn, settings)
    }

    /// Resolves once every [`crate::Connection`], [`crate::Listener`] and
    /// [`crate::Stream`] created from this registration has been closed, i.e.
    /// once it is safe to drop the `Registration`.
    ///
    /// Call it after [`Registration::shutdown`] (and after dropping any
    /// `Listener`); on its own it will simply stay pending, because nothing has
    /// asked the connections to go away.
    ///
    /// Streams are included even though they hold no native registration
    /// rundown reference: `StreamClose` queues work onto the connection's
    /// worker, and that worker pool is freed by `RegistrationClose`.
    pub fn wait_idle(&self) -> WaitIdle {
        WaitIdle {
            state: self.state.clone(),
            key: None,
        }
    }

    /// Raw handle, for crate-internal constructors only.
    pub(crate) fn raw(&self) -> &msquic::Registration {
        &self.inner
    }

    pub(crate) fn state(&self) -> &Arc<RundownState> {
        &self.state
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let active = self.state.active.load(Ordering::Acquire);
        if active != 0 {
            warn!(
                "Registration({:p}) dropped with {} live handle(s); RegistrationClose will block. \
                 Call shutdown() and await wait_idle() before dropping.",
                self.state, active
            );
        }
        trace!("Registration({:p}) dropping", self.state);
    }
}

/// Future returned by [`Registration::wait_idle`].
///
/// Resolves when every tracked connection and listener has released the
/// rundown.
pub struct WaitIdle {
    state: Arc<RundownState>,
    /// Slot in [`RundownState::waiters`], allocated on the first `Pending` poll.
    key: Option<u64>,
}

impl WaitIdle {
    fn deregister(&mut self) {
        if let Some(key) = self.key.take() {
            self.state.waiters.lock().unwrap().map.remove(&key);
        }
    }
}

impl Future for WaitIdle {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.state.active.load(Ordering::Acquire) == 0 {
            this.deregister();
            return Poll::Ready(());
        }
        {
            let mut waiters = this.state.waiters.lock().unwrap();
            match this.key {
                Some(key) => {
                    waiters.map.insert(key, cx.waker().clone());
                }
                None => {
                    let key = waiters.next;
                    waiters.next += 1;
                    waiters.map.insert(key, cx.waker().clone());
                    this.key = Some(key);
                }
            }
        }
        // Re-check after registering, to avoid a lost wakeup against a guard
        // that drained to zero between the fast-path load and taking the lock.
        if this.state.active.load(Ordering::Acquire) == 0 {
            this.deregister();
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for WaitIdle {
    fn drop(&mut self) {
        self.deregister();
    }
}

#[cfg(test)]
mod tests {
    use super::{RundownGuard, RundownState, WaitIdle};

    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    fn wait_idle(state: &Arc<RundownState>) -> WaitIdle {
        WaitIdle {
            state: state.clone(),
            key: None,
        }
    }

    fn poll_with(fut: &mut WaitIdle, waker: &Waker) -> Poll<()> {
        let mut cx = Context::from_waker(waker);
        std::pin::Pin::new(fut).poll(&mut cx)
    }

    struct FlagWaker(Arc<AtomicBool>);
    impl Wake for FlagWaker {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn flag_waker() -> (Waker, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        (Waker::from(Arc::new(FlagWaker(flag.clone()))), flag)
    }

    fn poll(fut: &mut WaitIdle) -> Poll<()> {
        let (waker, _) = flag_waker();
        poll_with(fut, &waker)
    }

    #[test]
    fn idle_from_start_resolves_immediately() {
        let state = Arc::new(RundownState::default());
        assert_eq!(poll(&mut wait_idle(&state)), Poll::Ready(()));
    }

    #[test]
    fn reservation_keeps_pending_until_dropped() {
        let state = Arc::new(RundownState::default());
        let guard = RundownGuard::new(state.clone());
        let mut fut = wait_idle(&state);
        assert_eq!(poll(&mut fut), Poll::Pending);
        drop(guard);
        assert_eq!(poll(&mut fut), Poll::Ready(()));
    }

    #[test]
    fn guard_drop_wakes_registered_waiter() {
        let state = Arc::new(RundownState::default());
        let guard = RundownGuard::new(state.clone());
        let mut fut = wait_idle(&state);

        let (waker, woken) = flag_waker();
        assert_eq!(poll_with(&mut fut, &waker), Poll::Pending);
        assert!(!woken.load(Ordering::SeqCst));

        drop(guard);
        assert!(woken.load(Ordering::SeqCst), "waiter should be woken");
        assert_eq!(poll(&mut fut), Poll::Ready(()));
    }

    #[test]
    fn concurrent_waiters_all_complete() {
        let state = Arc::new(RundownState::default());
        let guard = RundownGuard::new(state.clone());
        let (mut a, mut b) = (wait_idle(&state), wait_idle(&state));
        assert_eq!(poll(&mut a), Poll::Pending);
        assert_eq!(poll(&mut b), Poll::Pending);
        drop(guard);
        assert_eq!(poll(&mut a), Poll::Ready(()));
        assert_eq!(poll(&mut b), Poll::Ready(()));
    }

    #[test]
    fn dropped_waiter_deregisters_its_slot() {
        let state = Arc::new(RundownState::default());
        let guard = RundownGuard::new(state.clone());
        let mut fut = wait_idle(&state);
        assert_eq!(poll(&mut fut), Poll::Pending);
        assert_eq!(state.waiters.lock().unwrap().map.len(), 1);
        drop(fut);
        assert_eq!(
            state.waiters.lock().unwrap().map.len(),
            0,
            "slot must be removed on drop"
        );
        drop(guard);
    }

    #[test]
    fn nested_reservations_need_all_released() {
        let state = Arc::new(RundownState::default());
        let first = RundownGuard::new(state.clone());
        let second = RundownGuard::new(state.clone());
        let mut fut = wait_idle(&state);
        assert_eq!(poll(&mut fut), Poll::Pending);
        drop(first);
        assert_eq!(poll(&mut fut), Poll::Pending);
        drop(second);
        assert_eq!(poll(&mut fut), Poll::Ready(()));
    }
}
