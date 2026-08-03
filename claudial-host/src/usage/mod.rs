//! Where the usage numbers come from.
//!
//! Two backends, selected at compile time by feature:
//!
//! - `direct` (default) — talk to `api.anthropic.com` with Claude Code's own
//!   OAuth token and read the `anthropic-ratelimit-*` response headers. No
//!   extra infrastructure, but it needs Claude Code logged in on this machine
//!   and it spends a (tiny) API request per poll.
//! - `proxy` — read the snapshot [claude-proxy-rs] already keeps, over its
//!   `/admin/oauth/usage` endpoint. Costs nothing upstream, works on a machine
//!   that has never run Claude Code, and reuses the proxy's cache and its
//!   web-session fallback for the aggressively rate-limited usage endpoint.
//!
//! Cargo features are additive, so `--features proxy` leaves the default
//! `direct` enabled too. Rather than fail that build, `proxy` simply wins.
//!
//! [claude-proxy-rs]: https://github.com/okhsunrog/claude-proxy-rs

#[cfg(all(feature = "direct", not(feature = "proxy")))]
mod direct;
#[cfg(feature = "proxy")]
mod proxy;

#[cfg(all(feature = "direct", not(feature = "proxy")))]
pub use direct::UsageClient;
#[cfg(feature = "proxy")]
pub use proxy::UsageClient;

#[cfg(not(any(feature = "direct", feature = "proxy")))]
compile_error!("enable one of the `direct` or `proxy` features");

/// Round a percentage into the byte the device displays.
fn clamp_percent(percent: f64) -> u8 {
    percent.round().clamp(0.0, 100.0) as u8
}
