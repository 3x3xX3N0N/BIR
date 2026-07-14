//! TOML → [`Formula`], with the checks that make later stages able to assume a
//! well-formed graph.
//!
//! Parsing is not just deserialization here. A formula that deserializes can
//! still be nonsense — two steps with the same id, an edge to a step that does
//! not exist, a version from the future — and every one of those is cheaper to
//! catch here, with the source in hand, than three stages later as a confusing
//! panic. So [`parse`] deserializes *and* validates structure, and everything
//! downstream may assume ids are unique and edges resolve.

use std::collections::BTreeSet;

use crate::types::{Formula, Step};
use crate::{Error, Result};

/// Parse and structurally validate a formula.
pub fn parse(src: &str) -> Result<Formula> {
    let formula: Formula = toml::from_str(src).map_err(|e| Error::Parse(e.to_string()))?;

    if formula.version != 1 {
        return Err(Error::Parse(format!(
            "formula version {} is from the future; this build understands version 1",
            formula.version
        )));
    }
    if formula.formula.trim().is_empty() {
        return Err(Error::Parse("a formula needs a `formula` name".into()));
    }

    validate_steps(&formula.steps)?;
    Ok(formula)
}

/// Ids unique, edges resolvable — across loop bodies too.
///
/// The edge check treats a loop *step* as one node, by its declared id: a `needs`
/// may name the loop, and it means "all of its iterations". It may not name a
/// single iteration, because those ids do not exist until cook time. Enforcing
/// that here keeps a whole class of "why is my dep dangling" out of cook.
fn validate_steps(steps: &[Step]) -> Result<()> {
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    for s in steps {
        if s.id.trim().is_empty() {
            return Err(Error::Invalid("a step has an empty id".into()));
        }
        if !ids.insert(s.id.as_str()) {
            return Err(Error::Invalid(format!("two steps share the id `{}`", s.id)));
        }
    }

    // Loop and gate arity, before edges — a malformed loop is a clearer error
    // than the dangling edge it would otherwise cause.
    for s in steps {
        if let Some(l) = &s.loop_spec {
            match (l.range.is_some(), l.count.is_some()) {
                (false, false) => {
                    return Err(Error::Invalid(format!(
                        "step `{}` loops but gives neither `range` nor `count`",
                        s.id
                    )));
                }
                (true, true) => {
                    return Err(Error::Invalid(format!(
                        "step `{}` gives both `range` and `count`; pick one",
                        s.id
                    )));
                }
                _ => {}
            }
            if l.body.is_empty() {
                return Err(Error::Invalid(format!(
                    "step `{}` loops over an empty body",
                    s.id
                )));
            }
            // A body's ids share the formula's id space once suffixed, but must
            // be unique *within* the body before that.
            let mut body_ids = BTreeSet::new();
            for b in &l.body {
                if !body_ids.insert(b.id.as_str()) {
                    return Err(Error::Invalid(format!(
                        "loop `{}` has two body steps named `{}`",
                        s.id, b.id
                    )));
                }
            }
        }
    }

    for s in steps {
        for dep in s.blockers() {
            if dep == s.id {
                return Err(Error::Invalid(format!("step `{}` depends on itself", s.id)));
            }
            if !ids.contains(dep.as_str()) {
                return Err(Error::Invalid(format!(
                    "step `{}` needs `{dep}`, which is not a step in this formula",
                    s.id
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_workflow_parses() {
        let f = parse(
            r#"
            formula = "quick"
            version = 1
            type = "workflow"
            [[steps]]
            id = "a"
            title = "Do A"
        "#,
        )
        .unwrap();
        assert_eq!(f.formula, "quick");
        assert_eq!(f.steps.len(), 1);
    }

    #[test]
    fn a_bare_string_var_is_its_default() {
        let f = parse(
            r#"
            formula = "v"
            version = 1
            [vars]
            wisp_type = "patrol"
            [[steps]]
            id = "a"
            title = "t"
        "#,
        )
        .unwrap();
        assert_eq!(f.vars["wisp_type"].default.as_deref(), Some("patrol"));
        assert!(!f.vars["wisp_type"].required);
    }

    #[test]
    fn a_table_var_keeps_its_constraints() {
        let f = parse(
            r#"
            formula = "v"
            version = 1
            [vars.component]
            description = "the component"
            required = true
            enum = ["api", "web"]
            [[steps]]
            id = "a"
            title = "t"
        "#,
        )
        .unwrap();
        let v = &f.vars["component"];
        assert!(v.required);
        assert_eq!(v.enum_values, ["api", "web"]);
        assert!(v.default.is_none());
    }

    #[test]
    fn a_future_version_is_refused_not_guessed_at() {
        let err = parse("formula=\"x\"\nversion=2\n").unwrap_err();
        assert!(matches!(err, Error::Parse(_)));
        assert!(err.to_string().contains("version 2"));
    }

    #[test]
    fn duplicate_step_ids_are_caught_here_not_downstream() {
        let err = parse(
            r#"
            formula = "d"
            version = 1
            [[steps]]
            id = "a"
            title = "1"
            [[steps]]
            id = "a"
            title = "2"
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("share the id `a`"));
    }

    #[test]
    fn an_edge_to_nowhere_is_an_error() {
        let err = parse(
            r#"
            formula = "d"
            version = 1
            [[steps]]
            id = "a"
            title = "1"
            needs = ["ghost"]
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("`ghost`"));
    }

    #[test]
    fn a_self_edge_is_an_error() {
        let err = parse(
            r#"
            formula = "d"
            version = 1
            [[steps]]
            id = "a"
            title = "1"
            needs = ["a"]
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("depends on itself"));
    }

    #[test]
    fn a_loop_needs_exactly_one_bound() {
        let both = parse(
            r#"
            formula = "l"
            version = 1
            [[steps]]
            id = "m"
            title = "moves"
            [steps.loop]
            range = "1..3"
            count = 3
            [[steps.loop.body]]
            id = "move"
            title = "Move {i}"
        "#,
        )
        .unwrap_err();
        assert!(both.to_string().contains("pick one"));
    }
}
