//! Explicit recovery of poisoned standard-library mutexes.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks a mutex and recovers its guard if another holder panicked.
///
/// Call only where the owner permits recovery of its protected state. This does
/// not clear the poison marker; callers that reject poisoned state still fail.
pub fn recover_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_retains_state_and_does_not_hide_poison_from_other_callers() {
        let state = Mutex::new(Vec::new());
        let panic = std::panic::catch_unwind(|| {
            let mut guard = state.lock().expect("initial lock");
            guard.push(1);
            panic!("poison state");
        });
        assert!(panic.is_err());
        recover_lock(&state).push(2);
        assert_eq!(*recover_lock(&state), [1, 2]);
        assert!(
            state.lock().is_err(),
            "recovery must preserve the poison marker"
        );
    }
}
