//! The formula DSL: workflow templates that compile down to issues.
//!
//! A *formula* is a `.formula.toml` file describing a workflow as a graph of
//! steps. **Cooking** a formula turns it into a set of issues and the
//! dependencies between them — which the caller then creates through whatever
//! `Storage` it has. That is the one seam that matters here: **this crate does
//! no I/O and has no idea what a database is.** TOML text and a variable map go
//! in; a [`Plan`] of proto-issues comes out. Everything is pure, so all of it is
//! testable without a store, a server, or a filesystem.
//!
//! # What a formula can say
//!
//! * **vars** — typed, validated inputs (`required`, `default`, `enum`,
//!   `pattern`), substituted into step text as `{{name}}`.
//! * **steps** — the work. Each becomes an issue. `needs`/`depends_on` become
//!   `blocks` edges between them.
//! * **condition** — a step is included only if its condition holds against the
//!   bound variables. A false condition drops the step *and* rewrites the edges
//!   that pointed through it, so the graph never dangles.
//! * **loop** — one step body expands into N steps over a range or count, with
//!   an iteration variable (`{n}`) bound in each copy.
//! * **gate** — an async wait. The step is split into a `gate`-typed issue
//!   capturing the wait, and the original step made to block on it.
//!
//! # What it cannot yet
//!
//! `extends` (inheritance), `advice` (aspect-oriented before/after/around), and
//! the `expansion`/`aspect`/`convoy` formula *types* parse and validate, but
//! cooking them returns [`Error::Unsupported`] rather than half-doing it. They
//! are a compiler's harder half — weaving one graph's steps into another's — and
//! a plausible-looking wrong expansion is worse than an honest refusal. See each
//! one's note in [`cook`].
//!
//! # The shape of the whole thing
//!
//! ```text
//!   parse ─▶ Formula (AST)
//!                │
//!   bind ───────▶ Bindings   (vars provided + defaults, validated)
//!                │
//!   cook ───────▶ Plan       (proto-issues + proto-deps, ready to create)
//! ```

pub mod cook;
pub mod eval;
pub mod parse;
pub mod types;
pub mod vars;

pub use cook::{Plan, ProtoDep, ProtoIssue, cook};
pub use parse::parse;
pub use types::{Formula, FormulaType, GateSpec, LoopSpec, Step, VarDef};
pub use vars::Bindings;

/// Everything that can go wrong turning a formula into a plan.
///
/// The split that matters: [`Parse`](Error::Parse) and [`Var`](Error::Var) are
/// the *author's* or *caller's* fault and name exactly what to fix;
/// [`Unsupported`](Error::Unsupported) is *this port's* gap and must never be
/// confused with the first two, or a user will go hunting for a mistake in a
/// formula that is perfectly correct.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("formula does not parse: {0}")]
    Parse(String),

    /// A variable problem: missing a required one, a value outside its `enum`,
    /// a `{{name}}` with no such var. The message names the variable.
    #[error("{0}")]
    Var(String),

    /// The formula is well-formed but references itself, or two steps share an
    /// id, or an edge points at a step that does not exist. Structural.
    #[error("invalid formula: {0}")]
    Invalid(String),

    /// A construct this port has not built yet. Distinct from every error above:
    /// nothing is wrong with the formula.
    #[error("{0} is not supported by this build yet")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;
