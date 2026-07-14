//! The `bd query` filter language.
//!
//! Implementation lands next; this is the public shape the CLI compiles against.

use bd_core::{Issue, IssueFilter};

pub mod error;
pub use error::{Error, Result};

/// A parsed query.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    _private: (),
}

/// Parse a query string such as `status=open AND priority<=1 AND label=infra`.
pub fn parse(_input: &str) -> Result<Query> {
    Err(Error::NotImplemented)
}

impl Query {
    /// The query as a pure SQL filter, when it is fully expressible as one.
    ///
    /// `Some` means the database can answer it alone. `None` means an
    /// in-memory predicate is also required — see [`Query::matches`].
    pub fn as_filter(&self) -> Option<IssueFilter> {
        None
    }

    /// A filter that is *necessary but not sufficient*, used to shrink the
    /// candidate set in SQL before applying [`matches`](Self::matches) in
    /// memory. Never narrower than the query itself.
    pub fn filter_hint(&self) -> IssueFilter {
        IssueFilter::default()
    }

    pub fn matches(&self, _issue: &Issue) -> bool {
        true
    }
}
