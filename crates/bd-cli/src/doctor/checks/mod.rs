//! The check registry, composed per family.
//!
//! # Why it is shaped like this
//!
//! Upstream registers checks by appending to one long list in one file. That
//! works for one author and deadlocks for ten: every person adding a check edits
//! the same lines, and in a single working tree concurrent edits to one file are
//! not a merge conflict — they are a **silently lost write**.
//!
//! So the list is composed instead. Each family owns exactly one file and
//! exposes one function, `checks()`. This module calls them and concatenates.
//! Adding a check means editing *your* file and nothing else; two families can
//! never collide, because they never share a line.
//!
//! A family whose `checks()` returns `vec![]` is honest: it means nobody has
//! written those checks yet. It is not a stub that lies about passing.

pub mod agents;
pub mod core;
pub mod dolt;
pub mod federation;
pub mod git;
pub mod graph;
pub mod identity;
pub mod pollution;
pub mod runtime;

use super::Check;

/// Every check, in display order.
pub fn registry() -> Vec<Box<dyn Check>> {
    let mut all: Vec<Box<dyn Check>> = Vec::new();
    all.extend(core::checks());
    all.extend(graph::checks());
    all.extend(identity::checks());
    all.extend(git::checks());
    all.extend(dolt::checks());
    all.extend(pollution::checks());
    all.extend(runtime::checks());
    all.extend(federation::checks());
    all.extend(agents::checks());
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Check names are the key agents grep for in `--json`. Two checks with one
    /// name make the output ambiguous and `doctor --fix` nondeterministic about
    /// which one it repaired. With nine authors working in parallel this is the
    /// one collision the file split cannot prevent, so it is asserted instead.
    #[test]
    fn every_check_name_is_unique() {
        let mut seen = HashSet::new();
        let mut dupes = Vec::new();
        for c in registry() {
            if !seen.insert(c.name()) {
                dupes.push(c.name());
            }
        }
        assert!(dupes.is_empty(), "duplicate check names: {dupes:?}");
    }
}
