use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("issue not found: {0}")]
    NotFound(String),

    #[error("issue already exists: {0}")]
    AlreadyExists(String),

    /// The backend cannot do this at all — e.g. `bd dolt push` on SQLite.
    ///
    /// This is a *first-class, expected* outcome, not a failure. Capability
    /// gaps are part of a backend's contract: SQLite genuinely has no commit
    /// graph, and saying so plainly is better than pretending or crashing.
    #[error("{op} is not supported by the {backend} backend{}", .hint.as_ref().map(|h| format!(" ({h})")).unwrap_or_default())]
    Unsupported {
        op: &'static str,
        backend: &'static str,
        hint: Option<String>,
    },

    #[error("could not mint a unique id after exhausting all candidates")]
    IdExhausted,

    #[error("dependency cycle: {}", .0.join(" -> "))]
    Cycle(Vec<String>),

    /// Someone else holds the claim and the lease has not expired.
    #[error("issue {id} is already claimed by {holder}")]
    AlreadyClaimed { id: String, holder: String },

    /// Optimistic concurrency check failed; the row moved under us.
    #[error("issue {0} was modified concurrently; retry")]
    Conflict(String),

    #[error("no beads workspace found (run `bd init`)")]
    NoWorkspace,

    #[error(transparent)]
    Domain(#[from] bd_core::Error),

    #[error("database error: {0}")]
    Db(String),

    #[error(transparent)]
    Other(#[from] anyhow_lite::Error),
}

impl Error {
    pub fn unsupported(op: &'static str, backend: &'static str) -> Self {
        Error::Unsupported {
            op,
            backend,
            hint: None,
        }
    }

    pub fn unsupported_hint(op: &'static str, backend: &'static str, hint: impl Into<String>) -> Self {
        Error::Unsupported {
            op,
            backend,
            hint: Some(hint.into()),
        }
    }

    pub fn is_unsupported(&self) -> bool {
        matches!(self, Error::Unsupported { .. })
    }
}

/// A tiny stand-in so this crate does not take an `anyhow` dependency purely to
/// carry an opaque source error.
pub mod anyhow_lite {
    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    pub struct Error(pub String);
}
