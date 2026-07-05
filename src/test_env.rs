//! Shared test helpers for process-global env mutation.

#[cfg(test)]
use std::sync::Mutex;

#[cfg(test)]
static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Serialize tests that read or write process-global environment variables.
#[cfg(test)]
pub fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
