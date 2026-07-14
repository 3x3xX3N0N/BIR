//! Turning the AST into a SQL filter, a *prefilter*, and an in-memory predicate.
//!
//! # The hybrid pushdown
//!
//! Three views of the same tree, and the invariants that keep them honest:
//!
//! - [`build`] returns `true` when the filter it produced is *exactly* the
//!   query. The caller may then hand the whole thing to SQL and skip the
//!   predicate entirely.
//! - When it returns `false`, the filter is still a legal **prefilter**: every
//!   condition in it is implied by the query, so no issue the query would match
//!   can be filtered out by it. Weaker is always safe; narrower is a silent
//!   wrong answer, which is why every branch below that cannot prove implication
//!   pushes nothing at all.
//! - [`matches`] is the ground truth. It must agree with a filter that [`build`]
//!   called exact, for every issue — the tests in `lib.rs` check exactly that.

use bd_core::{DependencyType, Issue, IssueFilter, IssueType, Priority, Status};
use chrono::{DateTime, Datelike, TimeDelta, TimeZone, Utc};

use crate::parser::{Cmp, Field, Node, Op, Value};

/// Values that name the *absence* of a value. `assignee=none` asks for
/// unassigned issues, not for an assignee literally called "none".
fn is_absent(s: &str) -> bool {
    s.is_empty() || s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("null")
}

/// Case-insensitive substring search — *ASCII*-insensitive, deliberately.
///
/// This is what `IssueFilter::text` becomes in SQL (`LIKE`), and SQLite's `LIKE`
/// folds ASCII only. Folding Unicode here would make the predicate match titles
/// the SQL prefilter had already thrown away, which is the prefilter being
/// narrower than the query — the one thing it may never be.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn same_day(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    (a.year(), a.month(), a.day()) == (b.year(), b.month(), b.day())
}

fn midnight(t: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(t.year(), t.month(), t.day(), 0, 0, 0)
        .single()
        .expect("UTC midnight is unambiguous")
}

/// The instants either side of `t`. `IssueFilter`'s date bounds are *strict*
/// (`col > after`, `col < before`), so an inclusive comparison (`created>=t`)
/// has to be nudged by the smallest representable step rather than widened —
/// widening would be safe for the hint but would quietly make `as_filter` a lie.
fn just_before(t: DateTime<Utc>) -> Option<DateTime<Utc>> {
    t.checked_sub_signed(TimeDelta::nanoseconds(1))
}

fn just_after(t: DateTime<Utc>) -> Option<DateTime<Utc>> {
    t.checked_add_signed(TimeDelta::nanoseconds(1))
}

/// Fill an `Option` slot, refusing to overwrite. Two comparisons on one field
/// (`status=open AND status=closed`) cannot both be expressed, so the second
/// reports failure and the first stands — which keeps the result a valid
/// prefilter even when it is not an exact one.
fn set<T: PartialEq>(slot: &mut Option<T>, v: T) -> bool {
    match slot {
        Some(existing) => *existing == v,
        None => {
            *slot = Some(v);
            true
        }
    }
}

/// Push one comparison into `f`. Returns whether `f` now encodes it *exactly*.
///
/// Anything pushed must be implied by the comparison even when this returns
/// `false` — see the module docs.
fn apply(c: &Cmp, f: &mut IssueFilter) -> bool {
    match (c.field, &c.value) {
        (Field::Status, Value::Text(v)) => match c.op {
            Op::Eq => set(&mut f.status, Status::from(v.clone())),
            Op::Ne => {
                f.exclude_statuses.push(Status::from(v.clone()));
                true
            }
            _ => false,
        },

        (Field::Type, Value::Text(v)) => match c.op {
            Op::Eq => set(&mut f.issue_type, IssueType::from(v.clone())),
            Op::Ne => {
                f.exclude_types.push(IssueType::from(v.clone()));
                true
            }
            _ => false,
        },

        // The DSL compares P-*numbers* (`priority<=1` is "P0 or P1"), while the
        // filter's bounds are stated in *urgency* and run the other way:
        // `min_priority` is "at least this important", i.e. `priority <= n`. So
        // `<=` maps to min and `>=` maps to max. Crossing these over is a silent
        // inversion that hands back precisely the issues the user excluded.
        //
        // The parser guarantees 0..=4, so the derived bounds cannot overflow.
        // They *can* fall outside the range (`priority>4` => `priority >= 5`),
        // which is correct: the query has no answers, and saying so in SQL beats
        // scanning to discover it.
        (Field::Priority, Value::Int(v)) => match c.op {
            Op::Eq => set(&mut f.priority, Priority(*v)),
            Op::Le => set(&mut f.min_priority, Priority(*v)),
            Op::Lt => set(&mut f.min_priority, Priority(v - 1)),
            Op::Ge => set(&mut f.max_priority, Priority(*v)),
            Op::Gt => set(&mut f.max_priority, Priority(v + 1)),
            // No "not this priority" column: fall back to memory.
            Op::Ne => false,
        },

        (Field::Assignee, Value::Text(v)) if c.op == Op::Eq && !is_absent(v) => {
            set(&mut f.assignee, v.clone())
        }
        (Field::Owner, Value::Text(v)) if c.op == Op::Eq && !is_absent(v) => {
            set(&mut f.owner, v.clone())
        }
        (Field::Label, Value::Text(v)) if c.op == Op::Eq && !is_absent(v) => {
            f.labels_all.push(v.clone());
            true
        }
        // `IssueFilter::parent` is *transitive* — it selects every descendant of
        // the issue, not just its children — while the predicate below can only
        // see the edges hanging off the issue in hand, i.e. one hop. The filter
        // is therefore strictly wider: a fine prefilter, never an exact answer.
        // (If the DSL is ever meant to mean "descendant of", this is the line
        // that has to change, and the predicate cannot follow it without the
        // graph.)
        (Field::Parent, Value::Text(v)) if c.op == Op::Eq => {
            let _ = set(&mut f.parent, v.clone());
            false
        }
        (Field::HasMetadataKey, Value::Text(v)) if c.op == Op::Eq => {
            set(&mut f.has_metadata_key, v.clone())
        }
        // A trailing `*` is a prefix match, which `spec_id` (an equality) cannot
        // express and must not approximate.
        (Field::Spec, Value::Text(v)) if c.op == Op::Eq && !v.ends_with('*') => {
            set(&mut f.spec_id, v.clone())
        }

        // `text` searches title *and* description, so it is strictly wider than
        // either one alone: a fine prefilter, never an exact answer. Note the
        // `!=` case pushes nothing — the negation of a wider condition is not a
        // wider negation.
        (Field::Title | Field::Description, Value::Text(v))
            if c.op == Op::Eq && !is_absent(v) =>
        {
            let _ = set(&mut f.text, v.clone());
            false
        }

        (Field::Pinned, Value::Bool(v)) => set(&mut f.pinned, *v == (c.op == Op::Eq)),
        (Field::Ephemeral, Value::Bool(v)) => set(&mut f.ephemeral, *v == (c.op == Op::Eq)),
        (Field::Template, Value::Bool(v)) => set(&mut f.is_template, *v == (c.op == Op::Eq)),

        (Field::Created, Value::Time(t)) => {
            apply_date(c.op, *t, &mut f.created_after, &mut f.created_before)
        }
        (Field::Updated, Value::Time(t)) => {
            apply_date(c.op, *t, &mut f.updated_after, &mut f.updated_before)
        }
        (Field::Closed, Value::Time(t)) => {
            apply_date(c.op, *t, &mut f.closed_after, &mut f.closed_before)
        }

        // `id`, `notes`, and `started` have no column in `IssueFilter`, and
        // `!=` on a text field has no column either. Push nothing; the predicate
        // will sort it out.
        _ => false,
    }
}

fn apply_date(
    op: Op,
    t: DateTime<Utc>,
    after: &mut Option<DateTime<Utc>>,
    before: &mut Option<DateTime<Utc>>,
) -> bool {
    // Both filter bounds are strict (`col > after`, `col < before`), so the
    // strict operators map straight across and the inclusive ones are nudged by
    // a nanosecond.
    match op {
        Op::Gt => set(after, t),
        Op::Ge => just_before(t).is_some_and(|x| set(after, x)),
        Op::Lt => set(before, t),
        Op::Le => just_after(t).is_some_and(|x| set(before, x)),
        // `created=<date>` means "on that day": the half-open interval
        // [midnight, midnight+1d), expressed with strict bounds. Both ends must
        // land for it to be exact, but either end alone is still implied by the
        // query, so `&` (not `&&`) — a failed half must not skip the half that
        // would have fit, since the hint wants everything it can get.
        Op::Eq => {
            let start = midnight(t);
            match (just_before(start), start.checked_add_signed(TimeDelta::days(1))) {
                (Some(open), Some(end)) => set(after, open) & set(before, end),
                _ => false,
            }
        }
        // "Not on that day" is a hole, not a range.
        Op::Ne => false,
    }
}

/// The label values of an OR chain, if that is all it is.
///
/// `label=a OR label=b` is the one OR the filter can express, via `labels_any`.
/// Anything else in the chain — another field, a `!=`, an AND, a NOT — makes the
/// whole chain unrepresentable and returns `None`.
fn collect_or_labels(node: &Node) -> Option<Vec<String>> {
    match node {
        Node::Cmp(Cmp {
            field: Field::Label,
            op: Op::Eq,
            value: Value::Text(v),
        }) if !is_absent(v) => Some(vec![v.clone()]),
        Node::Or(l, r) => {
            let mut left = collect_or_labels(l)?;
            left.extend(collect_or_labels(r)?);
            Some(left)
        }
        _ => None,
    }
}

/// Build a filter from the tree. `true` means it is the whole query.
pub(crate) fn build(node: &Node, f: &mut IssueFilter) -> bool {
    match node {
        Node::Cmp(c) => apply(c, f),
        Node::And(l, r) => {
            // Short-circuits, but the caller throws `f` away when this is false,
            // so a half-built filter never escapes.
            build(l, f) && build(r, f)
        }
        Node::Not(inner) => apply_not(inner, f),
        Node::Or(..) => match collect_or_labels(node) {
            // `labels_any` is a single slot: two ORed label groups cannot both
            // live in it, and merging them would answer a different question.
            Some(labels) if f.labels_any.is_empty() => {
                f.labels_any = labels;
                true
            }
            _ => false,
        },
    }
}

/// `NOT status=x` and `NOT type=x` are the only negations with a column behind
/// them. Everything else under a `NOT` pushes *nothing*: pushing the operand
/// would prefilter for precisely the issues the query rejects.
fn apply_not(inner: &Node, f: &mut IssueFilter) -> bool {
    match inner {
        Node::Cmp(Cmp {
            field: Field::Status,
            op: Op::Eq,
            value: Value::Text(v),
        }) => {
            f.exclude_statuses.push(Status::from(v.clone()));
            true
        }
        Node::Cmp(Cmp {
            field: Field::Type,
            op: Op::Eq,
            value: Value::Text(v),
        }) => {
            f.exclude_types.push(IssueType::from(v.clone()));
            true
        }
        _ => false,
    }
}

/// Build the prefilter: everything on the AND spine that is safe to push.
///
/// The only difference from [`build`] is that failure is not fatal — a conjunct
/// that cannot be expressed is simply left out, which weakens the filter and
/// therefore stays correct.
pub(crate) fn build_hint(node: &Node, f: &mut IssueFilter) {
    match node {
        Node::Cmp(c) => {
            let _ = apply(c, f);
        }
        Node::And(l, r) => {
            build_hint(l, f);
            build_hint(r, f);
        }
        Node::Not(inner) => {
            let _ = apply_not(inner, f);
        }
        Node::Or(..) => {
            // An OR is dropped — *except* an all-labels chain, which is a
            // genuine necessary condition (every match carries one of these
            // labels) and is the difference between an indexed lookup and a
            // full scan for the query it appears in.
            if f.labels_any.is_empty()
                && let Some(labels) = collect_or_labels(node)
            {
                f.labels_any = labels;
            }
        }
    }
}

pub(crate) fn matches(node: &Node, issue: &Issue) -> bool {
    match node {
        Node::Cmp(c) => matches_cmp(c, issue),
        Node::And(l, r) => matches(l, issue) && matches(r, issue),
        Node::Or(l, r) => matches(l, issue) || matches(r, issue),
        Node::Not(x) => !matches(x, issue),
    }
}

fn matches_cmp(c: &Cmp, issue: &Issue) -> bool {
    // Every field below is `=`-shaped: compute the positive answer, then let
    // `!=` invert it. Date fields are the exception (six operators), and are
    // handled before this.
    let positive = match (c.field, &c.value) {
        (Field::Created, Value::Time(t)) => return cmp_time(c.op, issue.created_at, *t),
        (Field::Updated, Value::Time(t)) => return cmp_time(c.op, issue.updated_at, *t),
        // A NULL timestamp answers no question about itself, not even a negative
        // one — an issue that was never closed is not "closed on some other day".
        // SQL agrees: `closed_at < t` is NULL, and NULL is not TRUE.
        (Field::Closed, Value::Time(t)) => {
            return issue.closed_at.is_some_and(|a| cmp_time(c.op, a, *t));
        }
        (Field::Started, Value::Time(t)) => {
            return issue.started_at.is_some_and(|a| cmp_time(c.op, a, *t));
        }

        (Field::Priority, Value::Int(v)) => {
            return match c.op {
                Op::Eq => issue.priority.0 == *v,
                Op::Ne => issue.priority.0 != *v,
                Op::Lt => issue.priority.0 < *v,
                Op::Le => issue.priority.0 <= *v,
                Op::Gt => issue.priority.0 > *v,
                Op::Ge => issue.priority.0 >= *v,
            };
        }

        (Field::Status, Value::Text(v)) => issue.status == Status::from(v.clone()),
        (Field::Type, Value::Text(v)) => issue.issue_type == IssueType::from(v.clone()),

        // Wildcards are a prefix match, and only at the end: `id=bd-1*`.
        (Field::Id, Value::Text(v)) => match v.strip_suffix('*') {
            Some(prefix) => issue.id.starts_with(prefix),
            None => issue.id == *v,
        },
        (Field::Spec, Value::Text(v)) => match v.strip_suffix('*') {
            Some(prefix) => issue.spec_id.starts_with(prefix),
            None => issue.spec_id == *v,
        },

        // Substring, case-insensitively — matching `IssueFilter::text`, which is
        // what these get prefiltered by.
        (Field::Title, Value::Text(v)) => contains_ci(&issue.title, v),
        (Field::Notes, Value::Text(v)) => contains_ci(&issue.notes, v),
        (Field::Description, Value::Text(v)) => {
            if is_absent(v) {
                issue.description.is_empty()
            } else {
                contains_ci(&issue.description, v)
            }
        }

        // Exact, case-*sensitively*. The filter's `assignee = ?` is a plain SQL
        // equality; folding case here and not there would make the prefilter
        // narrower than the predicate, which is the one thing a prefilter may
        // never be.
        (Field::Assignee, Value::Text(v)) => {
            if is_absent(v) {
                issue.assignee.is_empty()
            } else {
                issue.assignee == *v
            }
        }
        (Field::Owner, Value::Text(v)) => {
            if is_absent(v) {
                issue.owner.is_empty()
            } else {
                issue.owner == *v
            }
        }
        (Field::Label, Value::Text(v)) => {
            if is_absent(v) {
                issue.labels.is_empty()
            } else {
                issue.labels.iter().any(|l| l == v)
            }
        }

        // The parent edge lives in the dependency graph, so this answer is only
        // as good as the issue's hydration. The filter expresses `parent`
        // exactly, so SQL answers it whenever it can and this path is the
        // fallback for queries that also need memory.
        (Field::Parent, Value::Text(v)) => issue.dependencies.iter().any(|d| {
            d.dep_type == DependencyType::ParentChild && d.depends_on_id == *v
        }),

        (Field::HasMetadataKey, Value::Text(v)) => issue
            .metadata
            .as_ref()
            .and_then(|m| m.as_object())
            .is_some_and(|o| o.contains_key(v)),

        (Field::Pinned, Value::Bool(v)) => issue.pinned == *v,
        (Field::Ephemeral, Value::Bool(v)) => issue.ephemeral == *v,
        (Field::Template, Value::Bool(v)) => issue.is_template == *v,

        // The parser admits no other (field, value) pairing.
        _ => unreachable!("unresolved comparison: {c:?}"),
    };

    match c.op {
        Op::Eq => positive,
        Op::Ne => !positive,
        _ => unreachable!("{} takes only = and !=", c.field.as_str()),
    }
}

fn cmp_time(op: Op, actual: DateTime<Utc>, target: DateTime<Utc>) -> bool {
    match op {
        // A date is a day, not an instant: `created=2026-07-14` asks about the
        // whole day, which is what the filter's half-open bounds encode too.
        Op::Eq => same_day(actual, target),
        Op::Ne => !same_day(actual, target),
        Op::Lt => actual < target,
        Op::Le => actual <= target,
        Op::Gt => actual > target,
        Op::Ge => actual >= target,
    }
}
