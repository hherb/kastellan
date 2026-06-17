//! Process-environment test helpers shared across crates.
//!
//! `cargo test` runs all of a crate's unit tests in one binary across
//! several threads, so any test that mutates a process-wide environment
//! variable races every *other* test that reads the same variable. The two
//! helpers here are the standard mitigation, hoisted out of `db`'s test
//! module (issue #127) so every crate uses the *same* RAII guard instead of
//! re-deriving a hand-rolled save / mutate / restore dance:
//!
//! * [`env_lock`] — a process-global mutex; hold it for the whole scope of
//!   an env mutation so concurrent tests cannot observe the transient value.
//! * [`EnvVarGuard`] — restores a variable to its prior value on drop, so a
//!   failing assertion mid-test cannot leak the mutation into whatever runs
//!   next under the same lock.
//!
//! Pair the two: take `env_lock()` first, then mutate via `EnvVarGuard`.
//!
//! Note: a crate whose *production* code reads env vars (e.g. `db`'s
//! `conn::current_os_user` reads `$USER`) keeps its own crate-local
//! `env_lock` so the lock also excludes those internal readers; such a crate
//! still shares this `EnvVarGuard` for the restore half.

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serialise unit tests that mutate process-wide environment variables.
///
/// Hold the returned guard for the entire scope of the mutation; its `Drop`
/// releases the lock. The mutex is poison-resistant via
/// `unwrap_or_else(into_inner)`, so a panicking test cannot wedge the rest of
/// the suite.
pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// RAII guard that restores a process env var to its prior value when it
/// drops — panic-safe, unlike a manual save / `set_var` / restore dance where
/// a failing assertion between the mutation and the restore leaks the value
/// into whatever runs next under the same [`env_lock`]. Always pair it with
/// `env_lock()` so concurrent tests cannot observe the mutation.
pub struct EnvVarGuard {
    key: &'static str,
    prior: Option<String>,
}

impl EnvVarGuard {
    /// Set `key` to `value`, remembering the prior value for restoration.
    pub fn set(key: &'static str, value: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, prior }
    }

    /// Remove `key`, remembering the prior value for restoration.
    pub fn unset(key: &'static str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { key, prior }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{env_lock, EnvVarGuard};

    // A var no production code reads, so these self-tests only race each
    // other — `env_lock()` serialises that.
    const KEY: &str = "KASTELLAN_ENVVARGUARD_SELFTEST";

    /// `set` then drop restores a previously-absent var to absent.
    #[test]
    fn set_restores_to_unset_on_drop() {
        let _lock = env_lock();
        std::env::remove_var(KEY);
        {
            let _g = EnvVarGuard::set(KEY, "transient");
            assert_eq!(std::env::var(KEY).as_deref(), Ok("transient"));
        }
        assert!(std::env::var(KEY).is_err(), "drop must restore the absent prior");
    }

    /// `unset`/`set` then drop restores a previously-present var's value.
    #[test]
    fn restores_prior_value_on_drop() {
        let _lock = env_lock();
        std::env::set_var(KEY, "original");
        {
            let _g = EnvVarGuard::unset(KEY);
            assert!(std::env::var(KEY).is_err(), "unset clears it for the body");
        }
        assert_eq!(
            std::env::var(KEY).as_deref(),
            Ok("original"),
            "drop must restore the prior value"
        );
        std::env::remove_var(KEY);
    }
}
