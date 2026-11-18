// Modified by alfypops, 2026: shared test environment guard.
use std::ffi::OsString;

/// The variable `mdm::utils::home_dir()` reads first on this platform.
#[cfg(windows)]
pub(crate) const HOME_ENV: &str = "USERPROFILE";
#[cfg(not(windows))]
pub(crate) const HOME_ENV: &str = "HOME";

/// Sets or unsets one environment variable and restores its original value on drop.
///
/// Callers must be `#[serial]` tests: the process environment is shared.
pub(crate) struct EnvVarGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        let original = std::env::var_os(key);
        // SAFETY: callers are serialized via #[serial], so mutating the
        // process environment is isolated for the duration of each test.
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, original }
    }

    pub(crate) fn unset(key: &'static str) -> Self {
        let original = std::env::var_os(key);
        // SAFETY: callers are serialized via #[serial], so mutating the
        // process environment is isolated for the duration of each test.
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: callers are serialized via #[serial], so restoring the
        // process environment is isolated for the duration of each test.
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
