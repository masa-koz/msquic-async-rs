use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Waker};

/// Extension trait for locking a [`Mutex`] while tolerating poisoning.
///
/// MsQuic callbacks run on MsQuic-owned threads. If a handler ever panics, the
/// mutex it held would be left poisoned, and a plain `lock().unwrap()` in every
/// later callback (or API call) would panic in turn, permanently wedging the
/// connection — a denial of service. Recovering the guard from a poisoned lock
/// keeps the connection usable; the protected state is always left consistent
/// before any `?`/panic can escape a critical section.
pub(crate) trait LockPoisonTolerant<T> {
    fn lock_poison_tolerant(&self) -> MutexGuard<'_, T>;
}

impl<T> LockPoisonTolerant<T> for Mutex<T> {
    fn lock_poison_tolerant(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Record `cx`'s waker in `waiters`, unless one already there wakes the same task.
///
/// The futures in this crate carry no registration of their own, so a waker put
/// on one of these lists is only removed when the list is drained to wake it.
/// Polling therefore has to be idempotent: `tokio::select!` drops and rebuilds
/// its branch futures on every iteration, and re-polls all of them whenever the
/// task wakes for whichever branch became ready, so pushing unconditionally
/// would grow the list without bound for as long as the awaited event does not
/// arrive.
///
/// Registering the task once is enough. Waking it re-polls whatever future it
/// holds at that point, which registers again if it is still waiting, so the
/// list holds one entry per distinct waiting task rather than one per poll.
/// `select!` polls its branches with the caller's own [`Context`] rather than a
/// per-branch waker, so its repeated polls do collapse onto a single entry.
pub(crate) fn register_waker(waiters: &mut Vec<Waker>, cx: &Context<'_>) {
    if !waiters.iter().any(|waiter| waiter.will_wake(cx.waker())) {
        waiters.push(cx.waker().clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::task::Wake;

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    #[test]
    fn register_waker_keeps_one_entry_per_task() {
        let task_a = Waker::from(Arc::new(NoopWake));
        let task_b = Waker::from(Arc::new(NoopWake));
        let mut waiters = Vec::new();

        for _ in 0..8 {
            register_waker(&mut waiters, &Context::from_waker(&task_a));
        }
        assert_eq!(waiters.len(), 1, "re-polling one task registers it once");

        register_waker(&mut waiters, &Context::from_waker(&task_b));
        assert_eq!(waiters.len(), 2, "another task registers separately");

        // Waking drains the list, and whoever is still waiting registers again.
        waiters.clear();
        register_waker(&mut waiters, &Context::from_waker(&task_a));
        assert_eq!(waiters.len(), 1);
    }
}
