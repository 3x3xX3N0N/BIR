//! Core domain model for beads: the `Issue` (a "bead"), the dependency graph
//! that connects them, and the identity scheme that names them.
//!
//! This crate is pure data plus validation. It has no storage, no I/O, and no
//! knowledge of any backend. Everything above it — storage, query, CLI — is
//! defined in terms of these types.

pub mod error;
pub mod filter;
pub mod idgen;
pub mod types;

pub use error::{Error, Result};
pub use filter::{IssueFilter, SortPolicy};
pub use types::{
    BondRef, BondType, Comment, DependencyType, Dependency, Event, EventType, Issue, IssueType,
    MolType, Priority, Status, StatusCategory, WispType, WorkType,
};
