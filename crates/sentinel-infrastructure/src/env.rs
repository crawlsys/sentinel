//! Real environment-variable adapter — implements `EnvPort`.
//!
//! Thin delegation to `std::env`. Hooks read env vars through `ctx.env`
//! so tests can inject a `StubEnv` instead of mutating process-global state.

use sentinel_domain::ports::EnvPort;
use std::ffi::OsString;

/// Infrastructure adapter implementing `EnvPort` via real `std::env`.
pub struct RealEnv;

impl EnvPort for RealEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn var_os(&self, key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }
}
