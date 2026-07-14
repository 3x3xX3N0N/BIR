//! Evaluating a step's `condition` — the smallest expression language that the
//! real formulas actually use.
//!
//! Upstream conditions are things like `{{deploy}} == true`, `{{count}} > 3`,
//! `{{env}} != prod`. That is the whole surface: one variable reference, one
//! comparison operator, one literal. This is deliberately **not** a general
//! expression evaluator — a formula is configuration, and a Turing-complete
//! condition language in configuration is a footgun, not a feature. If a real
//! formula ever needs `&&`, it can be split into two conditions.
//!
//! The comparison is *typed by what it looks like*: two things that both parse
//! as numbers compare numerically (so `10 > 9` is true, not false the way string
//! order would have it), `true`/`false` compare as booleans, everything else
//! compares as strings. That matches how the authors plainly expect it to work,
//! and it is the one place a naive string compare produces silently wrong graphs.

use crate::vars::Bindings;
use crate::{Error, Result};

/// Does this condition hold under these bindings?
///
/// An empty condition is vacuously true (the step is unconditional). A malformed
/// one is an error, not a `false`: silently dropping a step because its condition
/// did not parse is exactly the kind of invisible wrong the rest of this crate
/// is built to avoid.
pub fn holds(condition: &str, bindings: &Bindings) -> Result<bool> {
    let cond = condition.trim();
    if cond.is_empty() {
        return Ok(true);
    }

    let (lhs, op, rhs) = split(cond)?;
    let lhs = bindings.substitute(lhs)?;
    let rhs = bindings.substitute(rhs)?;

    Ok(op.apply(lhs.trim(), rhs.trim()))
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl Op {
    fn apply(self, lhs: &str, rhs: &str) -> bool {
        // Numbers if both sides are numbers; bools if both are bools; strings
        // otherwise. Comparing `9` and `10` as strings gives the wrong answer,
        // and that wrong answer silently drops or keeps a step.
        if let (Ok(a), Ok(b)) = (lhs.parse::<f64>(), rhs.parse::<f64>()) {
            return self.apply_ord(a.partial_cmp(&b));
        }
        if let (Some(a), Some(b)) = (parse_bool(lhs), parse_bool(rhs)) {
            // Ordering booleans is meaningless; only eq/ne are honest.
            return match self {
                Op::Eq => a == b,
                Op::Ne => a != b,
                _ => false,
            };
        }
        self.apply_ord(Some(lhs.cmp(rhs)))
    }

    fn apply_ord(self, ord: Option<std::cmp::Ordering>) -> bool {
        use std::cmp::Ordering::*;
        let Some(ord) = ord else { return false }; // NaN: never true.
        match self {
            Op::Eq => ord == Equal,
            Op::Ne => ord != Equal,
            Op::Gt => ord == Greater,
            Op::Ge => ord != Less,
            Op::Lt => ord == Less,
            Op::Le => ord != Greater,
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Split `a OP b` on the first operator found. Two-character operators are tried
/// before one-character ones, or `>=` would parse as `>` followed by a stray `=`.
fn split(cond: &str) -> Result<(&str, Op, &str)> {
    const OPS: &[(&str, Op)] = &[
        ("==", Op::Eq),
        ("!=", Op::Ne),
        (">=", Op::Ge),
        ("<=", Op::Le),
        (">", Op::Gt),
        ("<", Op::Lt),
    ];
    for (sym, op) in OPS {
        if let Some(pos) = cond.find(sym) {
            let lhs = &cond[..pos];
            let rhs = &cond[pos + sym.len()..];
            return Ok((lhs, *op, rhs));
        }
    }
    Err(Error::Invalid(format!(
        "condition {cond:?} has no comparison operator (==, !=, >, >=, <, <=)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;
    use std::collections::BTreeMap;

    fn bindings(pairs: &[(&str, &str)]) -> Bindings {
        // Build real bindings through a tiny formula so var namespacing matches
        // production exactly.
        let mut src = String::from("formula=\"c\"\nversion=1\n");
        for (k, _) in pairs {
            src.push_str(&format!("[vars.{k}]\ndefault=\"\"\n"));
        }
        src.push_str("[[steps]]\nid=\"a\"\ntitle=\"t\"\n");
        let f = parse(&src).unwrap();
        let provided: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Bindings::bind(&f, &provided).unwrap()
    }

    #[test]
    fn an_empty_condition_is_true() {
        assert!(holds("", &bindings(&[])).unwrap());
    }

    #[test]
    fn bool_equality_reads_a_var() {
        let b = bindings(&[("deploy", "true")]);
        assert!(holds("{{deploy}} == true", &b).unwrap());
        assert!(!holds("{{deploy}} != true", &b).unwrap());
    }

    #[test]
    fn the_default_false_excludes_the_step() {
        let b = bindings(&[("deploy", "false")]);
        assert!(!holds("{{deploy}} == true", &b).unwrap());
    }

    #[test]
    fn numbers_compare_as_numbers_not_strings() {
        let b = bindings(&[("n", "10")]);
        // The bug this guards: "10" < "9" as strings, but 10 > 9 as numbers.
        assert!(holds("{{n}} > 9", &b).unwrap());
        assert!(!holds("{{n}} < 9", &b).unwrap());
        assert!(holds("{{n}} >= 10", &b).unwrap());
    }

    #[test]
    fn strings_compare_as_strings() {
        let b = bindings(&[("env", "prod")]);
        assert!(holds("{{env}} == prod", &b).unwrap());
        assert!(holds("{{env}} != staging", &b).unwrap());
    }

    #[test]
    fn a_condition_with_no_operator_is_an_error_not_a_false() {
        let err = holds("{{deploy}}", &bindings(&[("deploy", "true")])).unwrap_err();
        assert!(err.to_string().contains("no comparison operator"));
    }

    #[test]
    fn ge_is_not_read_as_gt_plus_equals() {
        let b = bindings(&[("n", "5")]);
        assert!(holds("{{n}} >= 5", &b).unwrap());
        assert!(!holds("{{n}} > 5", &b).unwrap());
    }
}
