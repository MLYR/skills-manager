// Keep AI storage, credentials, provider traffic, and log retention behind one
// module boundary so later runner code cannot bypass their security checks.
pub mod config;
pub mod logs;
pub mod provider;
pub mod repository;
pub mod secret_store;
pub mod types;

use types::{AiCommandError, AiErrorCode, AiErrorKind};

/// Central construction keeps every service error on the frozen structured
/// contract instead of leaking dependency-specific strings to the frontend.
pub(crate) fn command_error(
    kind: AiErrorKind,
    code: AiErrorCode,
    message: impl Into<String>,
    retryable: bool,
) -> AiCommandError {
    AiCommandError {
        kind,
        code,
        message: message.into(),
        retryable,
        next_retry_at: None,
    }
}
