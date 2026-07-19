use std::sync::{Mutex, MutexGuard};

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
