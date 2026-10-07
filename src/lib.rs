//! Library target for the `codex-sub` sidecar. The binary uses these
//! modules over the sidecar wire; the host tests import the exact same
//! provider declaration so validation is version-locked by tests.

pub mod chat;
pub mod login;
pub mod manifest;
pub mod models;
pub mod oauth;
pub mod relay;
pub mod session;
pub mod setup;
