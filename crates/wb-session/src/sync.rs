//! Locking that survives a panic somewhere else.
//!
//! `Mutex::lock().unwrap()` turns one panic into a cascade: the thread that panicked while holding the lock
//! poisons it, and then every other thread that touches the lock panics too. In a service that keeps every
//! terminal reachable that is the wrong failure mode. Here a poisoned lock is recovered instead, but never
//! blindly: the caller passes a `repair` that restores the data's invariants, or fails closed (drops a
//! connection, revokes grants, cancels a pending admin step), because a panic may have stopped halfway through
//! a multi-step update. Every recovery is counted so the audit log can say it happened.
//!
//! Rules this module supports (see docs/design/hardening-plan.md):
//! - A lock guarding DATA is never held across blocking I/O (read it, copy what is needed, release, then do I/O).
//! - The one deliberate exception is a CONNECTION lock (`Session::conn`), which exists to serialise calls on one
//!   pipe and is bounded by the call's deadline watchdog; its repair drops the connection.
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, MutexGuard,
};

static RECOVERED: AtomicU64 = AtomicU64::new(0);

/// How many poisoned locks have been recovered in this process so far.
pub fn poison_recoveries() -> u64 {
    RECOVERED.load(Ordering::Relaxed)
}

pub trait LockExt<T> {
    /// Lock, recovering from poison. After a recovery `repair` runs once, with the lock held, before anyone else
    /// sees the data; the poison flag is then cleared so the repair is not repeated.
    fn locked_repairing(&self, repair: impl FnOnce(&mut T)) -> MutexGuard<'_, T>;

    /// Lock, recovering from poison without a repair. Only for data whose every update is a single step (an
    /// assignment, one insert), where a panic cannot leave it half changed.
    fn locked(&self) -> MutexGuard<'_, T> {
        self.locked_repairing(|_| {})
    }
}

impl<T> LockExt<T> for Mutex<T> {
    fn locked_repairing(&self, repair: impl FnOnce(&mut T)) -> MutexGuard<'_, T> {
        match self.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                RECOVERED.fetch_add(1, Ordering::Relaxed);
                let mut guard = poisoned.into_inner();
                repair(&mut guard);
                self.clear_poison();
                guard
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The recovery counter is process-wide and tests run in parallel, so the tests that read it (or cause a recovery)
    /// take turns; without this the counts below were occasionally off by another test's recovery.
    static SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn poison(m: &Arc<Mutex<Vec<u32>>>) {
        let m = m.clone();
        let _ = std::thread::spawn(move || {
            let mut g = m.lock().unwrap_or_else(|p| p.into_inner());
            g.push(999); // half of an update...
            panic!("simulated panic while holding the lock");
        })
        .join();
    }

    #[test]
    fn a_healthy_lock_is_just_a_lock_and_never_runs_the_repair() {
        let _turn = serial();
        let m = Mutex::new(vec![1u32]);
        let before = poison_recoveries();
        m.locked().push(2);
        let g = m.locked_repairing(|_| panic!("repair must not run"));
        assert_eq!(*g, vec![1, 2]);
        assert_eq!(poison_recoveries(), before);
    }

    #[test]
    fn a_poisoned_lock_is_recovered_repaired_once_counted_and_usable_afterwards() {
        let _turn = serial();
        let m = Arc::new(Mutex::new(vec![1u32, 2]));
        poison(&m);
        assert!(m.lock().is_err(), "the panic really did poison it");
        let before = poison_recoveries();
        {
            // The repair sees the half-updated data and restores the invariant (no 999).
            let mut g = m.locked_repairing(|v| v.retain(|x| *x != 999));
            assert_eq!(*g, vec![1, 2]);
            g.push(3);
        }
        assert_eq!(poison_recoveries(), before + 1);
        // Not poisoned any more: no second repair, no second count.
        let g = m.locked_repairing(|_| panic!("repair must not run twice"));
        assert_eq!(*g, vec![1, 2, 3]);
        assert_eq!(poison_recoveries(), before + 1);
    }

    #[test]
    fn plain_locked_also_survives_poison() {
        let _turn = serial();
        let m = Arc::new(Mutex::new(vec![7u32]));
        poison(&m);
        assert!(m.locked().contains(&7));
    }
}
