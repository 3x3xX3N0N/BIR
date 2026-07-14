use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// Domain-level validation failures. Storage and CLI errors live in their own
/// crates; this is only about an `Issue` (or edge) being malformed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Error {
    #[error("title is required")]
    TitleEmpty,

    #[error("title exceeds {max} characters (got {got})")]
    TitleTooLong { max: usize, got: usize },

    #[error("priority must be 0-4 (got {0})")]
    PriorityOutOfRange(i32),

    #[error("unknown status: {0}")]
    UnknownStatus(String),

    #[error("unknown issue type: {0}")]
    UnknownIssueType(String),

    #[error("dependency type must be 1-50 characters (got {0})")]
    InvalidDependencyType(String),

    #[error("metadata must be valid JSON: {0}")]
    InvalidMetadata(String),

    /// An issue may be `ephemeral` (lives outside the commit graph) or
    /// `no_history` (kept, but not version-controlled), never both.
    #[error("an issue cannot be both ephemeral and no_history")]
    EphemeralAndNoHistory,

    #[error("dependency cycle: {}", .0.join(" -> "))]
    DependencyCycle(Vec<String>),

    #[error("an issue cannot depend on itself ({0})")]
    SelfDependency(String),

    #[error("{0}")]
    Invalid(String),
}
