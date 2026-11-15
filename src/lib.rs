//! Library target for the `codex-sub` sidecar. The binary uses these
//! modules over the sidecar wire; the host tests import the exact same
//! provider declaration so validation is version-locked by tests.

pub mod accounts;
pub mod appserver;
pub mod chat;
pub mod manifest;
pub mod models;
pub mod relay;
pub mod session;
pub mod settings;
pub mod setup;
pub mod status;
pub mod usage;

/// Whether `text` carries HTTP status `code` as a standalone number. A
/// provider's stderr arrives verbatim, and a bare substring match hits
/// timestamps and timings (`13:10:57.429113Z`, `time.busy=429µs`) — enough
/// to bench a healthy account or report a signed-out one. A status code is
/// never glued to a digit, letter, `.` or `_`.
pub(crate) fn has_status(text: &str, code: &str) -> bool {
    let glued = |c: char| c.is_alphanumeric() || matches!(c, '.' | '_');
    text.match_indices(code).any(|(at, m)| {
        !text[..at].chars().next_back().is_some_and(glued)
            && !text[at + m.len()..].chars().next().is_some_and(glued)
    })
}
