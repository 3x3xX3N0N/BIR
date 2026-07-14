//! The `bd query` filter language.
//!
//! ```text
//! query      := or_expr
//! or_expr    := and_expr ( "OR" and_expr )*
//! and_expr   := not_expr ( "AND" not_expr )*
//! not_expr   := "NOT" not_expr | primary        -- right-associative
//! primary    := "(" or_expr ")" | comparison
//! comparison := IDENT op value
//! op         := "=" | "!=" | "<" | "<=" | ">" | ">="
//! value      := IDENT | STRING | NUMBER | DURATION
//! ```
//!
//! # How a query is answered
//!
//! Not every query fits in an [`IssueFilter`], so evaluation is a **hybrid
//! pushdown** with two shapes:
//!
//! ```text
//! match q.as_filter() {
//!     Some(f) => store.list_issues(&f).await?,                 // SQL alone
//!     None    => store.list_issues(&q.filter_hint()).await?    // SQL prefilter,
//!                     .into_iter().filter(|i| q.matches(i))    // then memory
//!                     .collect(),
//! }
//! ```
//!
//! The prefilter is the load-bearing half. It must be **weaker than the query,
//! never narrower**: it may admit issues the query rejects (memory throws those
//! away) but must never reject one the query would have kept, because nothing
//! downstream can recover a row SQL never returned.
//!
//! A duration on a date field is relative to the moment of parsing, and is
//! resolved to an absolute instant right there — so the filter and the predicate
//! cannot drift apart on a slow query, and a `Query` means the same thing every
//! time you evaluate it.

use bd_core::{Issue, IssueFilter};
use chrono::{DateTime, Utc};

pub mod error;
pub use error::{Error, Result};

mod eval;
mod lexer;
mod parser;

use parser::Node;

/// A parsed query.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    root: Node,
}

/// Parse a query string such as `status=open AND priority<=1 AND label=infra`.
pub fn parse(input: &str) -> Result<Query> {
    parse_at(input, Utc::now())
}

/// [`parse`], with "now" supplied — durations (`created>7d`) resolve against it.
///
/// Callers that need a reproducible query (tests, a scheduled report that must
/// mean the same thing on every run) should use this rather than `parse`.
pub fn parse_at(input: &str, now: DateTime<Utc>) -> Result<Query> {
    Ok(Query {
        root: parser::Parser::new(input, now)?.parse()?,
    })
}

impl Query {
    /// The query as a pure SQL filter, when it is fully expressible as one.
    ///
    /// `Some` means the database can answer it alone. `None` means an
    /// in-memory predicate is also required — see [`Query::matches`].
    ///
    /// Expressible means: comparisons and `AND` chains of them; `NOT` on
    /// `status`/`type` with `=`; and `OR` only as an all-labels chain, which is
    /// the one disjunction [`IssueFilter`] has a column for (`labels_any`).
    /// Everything else — `OR` across fields, `NOT` over a group, `id`, `notes`,
    /// `started`, `!=` on a text field, a `title` search (which SQL can only
    /// widen into a title-or-description one) — falls back to memory.
    pub fn as_filter(&self) -> Option<IssueFilter> {
        let mut f = IssueFilter::default();
        eval::build(&self.root, &mut f).then_some(f)
    }

    /// A filter that is *necessary but not sufficient*, used to shrink the
    /// candidate set in SQL before applying [`matches`](Self::matches) in
    /// memory. Never narrower than the query itself.
    pub fn filter_hint(&self) -> IssueFilter {
        let mut f = IssueFilter::default();
        eval::build_hint(&self.root, &mut f);
        f
    }

    pub fn matches(&self, issue: &Issue) -> bool {
        eval::matches(&self.root, issue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bd_core::{Dependency, DependencyType, IssueType, Priority, Status};
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 14, 12, 0, 0).unwrap()
    }

    fn q(input: &str) -> Query {
        parse_at(input, now()).expect(input)
    }

    fn days_ago(n: i64) -> DateTime<Utc> {
        now() - chrono::TimeDelta::days(n)
    }

    // -----------------------------------------------------------------------
    // A stand-in for the storage layer.
    //
    // This mirrors `bd-sqlite`'s `sqlfilter::push_filter` clause for clause: it
    // is what `IssueFilter` *means*, spelled out so the properties below can be
    // checked without a database. The whole pushdown rests on this crate and the
    // SQL agreeing, so when `push_filter` changes, this changes with it — and a
    // disagreement shows up here as a failing property rather than in production
    // as a missing row.
    // -----------------------------------------------------------------------
    fn admits(f: &IssueFilter, i: &Issue) -> bool {
        let has_label = |l: &String| i.labels.contains(l);

        if f.status.as_ref().is_some_and(|s| i.status != *s) {
            return false;
        }
        if !f.statuses.is_empty() && !f.statuses.contains(&i.status) {
            return false;
        }
        if f.exclude_statuses.contains(&i.status) {
            return false;
        }
        if f.priority.is_some_and(|p| i.priority != p) {
            return false;
        }
        // Urgency bounds, so they read backwards as numbers: `min_priority` is
        // `priority <= n`. See `IssueFilter`.
        if f.min_priority.is_some_and(|p| i.priority.0 > p.0) {
            return false;
        }
        if f.max_priority.is_some_and(|p| i.priority.0 < p.0) {
            return false;
        }
        if f.issue_type.as_ref().is_some_and(|t| i.issue_type != *t) {
            return false;
        }
        if f.exclude_types.contains(&i.issue_type) {
            return false;
        }
        if f.assignee.as_ref().is_some_and(|a| i.assignee != *a) {
            return false;
        }
        if f.owner.as_ref().is_some_and(|o| i.owner != *o) {
            return false;
        }
        if !f.labels_all.iter().all(has_label) {
            return false;
        }
        if !f.labels_any.is_empty() && !f.labels_any.iter().any(has_label) {
            return false;
        }
        // SQL walks the parent-child edges transitively; the corpus is shallow
        // enough that one hop is the same set, and the *predicate* is the one
        // that only sees one hop anyway.
        if f.parent.as_ref().is_some_and(|p| {
            !i.dependencies
                .iter()
                .any(|d| d.dep_type == DependencyType::ParentChild && d.depends_on_id == *p)
        }) {
            return false;
        }
        if f.spec_id.as_ref().is_some_and(|s| i.spec_id != *s) {
            return false;
        }
        if f.has_metadata_key.as_ref().is_some_and(|k| {
            !i.metadata
                .as_ref()
                .and_then(|m| m.as_object())
                .is_some_and(|o| o.contains_key(k))
        }) {
            return false;
        }
        // Strict on both sides, and a NULL timestamp satisfies neither.
        if f.created_after.is_some_and(|t| i.created_at <= t) {
            return false;
        }
        if f.created_before.is_some_and(|t| i.created_at >= t) {
            return false;
        }
        if f.updated_after.is_some_and(|t| i.updated_at <= t) {
            return false;
        }
        if f.updated_before.is_some_and(|t| i.updated_at >= t) {
            return false;
        }
        if f.closed_after
            .is_some_and(|t| i.closed_at.is_none_or(|c| c <= t))
        {
            return false;
        }
        if f.closed_before
            .is_some_and(|t| i.closed_at.is_none_or(|c| c >= t))
        {
            return false;
        }
        // `LIKE '%x%'`, which folds ASCII only.
        if f.text.as_ref().is_some_and(|s| {
            let s = s.to_ascii_lowercase();
            !i.title.to_ascii_lowercase().contains(&s)
                && !i.description.to_ascii_lowercase().contains(&s)
        }) {
            return false;
        }
        if f.pinned.is_some_and(|p| i.pinned != p) {
            return false;
        }
        if f.ephemeral.is_some_and(|e| i.ephemeral != e) {
            return false;
        }
        if f.is_template.is_some_and(|t| i.is_template != t) {
            return false;
        }
        true
    }

    fn issue(id: &str) -> Issue {
        Issue {
            id: id.into(),
            title: "a title".into(),
            created_at: days_ago(30),
            updated_at: days_ago(30),
            ..Default::default()
        }
    }

    /// Deliberately varied: statuses, priorities, labels, timestamps, an
    /// unassigned issue, an issue with no labels, a parented one, a metadata
    /// one. The properties below quantify over all of them.
    fn corpus() -> Vec<Issue> {
        vec![
            Issue {
                status: Status::Open,
                priority: Priority::CRITICAL,
                issue_type: IssueType::Bug,
                assignee: "alice".into(),
                labels: vec!["infra".into(), "gt:merge-request".into()],
                created_at: days_ago(2),
                updated_at: days_ago(1),
                ..issue("bd-1")
            },
            Issue {
                status: Status::Closed,
                priority: Priority::HIGH,
                issue_type: IssueType::Feature,
                assignee: "bob".into(),
                owner: "alice".into(),
                labels: vec!["ui".into()],
                created_at: days_ago(40),
                updated_at: days_ago(3),
                closed_at: Some(days_ago(3)),
                ..issue("bd-2")
            },
            Issue {
                status: Status::InProgress,
                priority: Priority::NORMAL,
                issue_type: IssueType::Task,
                labels: vec!["infra".into(), "ui".into()],
                title: "Fix the INFRA pipeline".into(),
                description: "flaky".into(),
                created_at: days_ago(1),
                updated_at: days_ago(1),
                started_at: Some(days_ago(1)),
                ..issue("bd-3")
            },
            Issue {
                status: Status::Blocked,
                priority: Priority::TRIVIAL,
                issue_type: IssueType::Epic,
                assignee: "Alice".into(), // case differs from bd-1 on purpose
                labels: vec!["1-alpha".into()],
                pinned: true,
                created_at: days_ago(10),
                updated_at: days_ago(10),
                ..issue("bd-4")
            },
            Issue {
                status: Status::Custom("triage".into()),
                priority: Priority::LOW,
                spec_id: "spec-7".into(),
                ephemeral: true,
                is_template: true,
                metadata: Some(serde_json::json!({"sprint": "s1"})),
                dependencies: vec![
                    Dependency::new("bd-5", "bd-1", DependencyType::ParentChild).unwrap(),
                ],
                created_at: days_ago(400),
                updated_at: days_ago(400),
                ..issue("bd-5")
            },
            // A bare issue: everything empty or default. Catches predicates that
            // accidentally treat "absent" as "matches".
            issue("bd-6"),
        ]
    }

    /// Every query below is valid and is run against the whole corpus by the two
    /// properties. Keep adding to it — it is cheaper than a new test.
    const QUERIES: &[&str] = &[
        "status=open",
        "status!=closed",
        "NOT status=closed",
        "NOT type=epic",
        "NOT NOT status=open",
        "NOT (status=open AND priority=0)",
        "priority<=1",
        "priority<1",
        "priority>2",
        "priority>=2",
        "priority!=2",
        "priority>4",
        "status=open AND priority<=1 AND label=infra",
        "label=infra AND label=ui",
        "label=a OR label=b",
        "label=infra OR label=ui",
        "label=infra OR label=ui OR label=1-alpha",
        "status=open OR label=ui",
        "status=open OR status=closed",
        "status=open OR type=bug AND priority=1",
        "(status=open OR status=blocked) AND priority<=1",
        "status=open AND (label=infra OR label=ui)",
        "(label=infra OR label=ui) AND (label=1-alpha OR label=ui)",
        "assignee=alice",
        "assignee=none",
        "assignee!=alice",
        "owner=alice",
        "label=none",
        "label!=infra",
        "title=infra",
        "title!=infra",
        "desc=flaky",
        "desc=none",
        "notes=x",
        "id=bd-1",
        r#"id="bd-*""#,
        "id!=bd-1",
        "spec=spec-7",
        r#"spec="spec-*""#,
        "parent=bd-1",
        "pinned=true",
        "pinned!=true",
        "ephemeral=false",
        "template=yes",
        "has_metadata_key=sprint",
        "created>7d",
        "created<7d",
        "created>=7d",
        "created<=7d",
        "updated>24h",
        "closed>7d",
        "closed<7d",
        "started>7d",
        "created=2026-07-12",
        "created!=2026-07-12",
        "created>2026-01-01 AND created<2026-12-31",
        "status=open AND status=closed",
        "created>7d AND created>3d",
        "status=open AND title=infra",
        "status=closed AND NOT type=epic AND label=ui",
        "type=bug AND (label=infra OR label=ui) AND created>7d",
        "NOT (label=infra OR label=ui)",
        "label=gt:merge-request",
        "status=OPEN",
    ];

    // -----------------------------------------------------------------------
    // The two properties that make the pushdown sound.
    // -----------------------------------------------------------------------

    #[test]
    fn filter_hint_is_never_narrower_than_the_query() {
        for src in QUERIES {
            let query = q(src);
            let hint = query.filter_hint();
            for i in corpus() {
                if query.matches(&i) {
                    assert!(
                        admits(&hint, &i),
                        "{src}: prefilter would have dropped {}, which the query matches\n\
                         hint: {hint:?}",
                        i.id
                    );
                }
            }
        }
    }

    #[test]
    fn an_exact_filter_agrees_with_the_predicate_on_every_issue() {
        for src in QUERIES {
            let query = q(src);
            let Some(f) = query.as_filter() else { continue };
            for i in corpus() {
                assert_eq!(
                    admits(&f, &i),
                    query.matches(&i),
                    "{src}: SQL and memory disagree about {}\nfilter: {f:?}",
                    i.id
                );
            }
        }
    }

    /// The specific case the design turns on: an OR the filter cannot express
    /// must leave the prefilter wide open, not half-applied. Pushing either arm
    /// of `status=open OR label=ui` would silently delete the other arm's hits.
    #[test]
    fn a_mixed_or_pushes_nothing_at_all() {
        let query = q("status=open OR label=ui");
        assert_eq!(query.as_filter(), None);
        assert!(query.filter_hint().is_empty());

        // bd-2 is closed, so it fails the left arm and survives only on `label=ui`.
        let closed_with_ui = &corpus()[1];
        assert!(query.matches(closed_with_ui));
        assert!(admits(&query.filter_hint(), closed_with_ui));

        // Same trap one level down: the OR is under an AND, so the AND's own
        // conjunct is pushable but nothing from inside the OR is.
        let query = q("type=bug AND (status=open OR label=ui)");
        let hint = query.filter_hint();
        assert_eq!(hint.issue_type, Some(IssueType::Bug));
        assert_eq!(hint.status, None);
        assert!(hint.labels_all.is_empty() && hint.labels_any.is_empty());
    }

    #[test]
    fn negation_is_dropped_from_the_prefilter_unless_it_has_a_column() {
        // `NOT status=closed` has one: it excludes.
        let hint = q("NOT status=closed").filter_hint();
        assert_eq!(hint.exclude_statuses, vec![Status::Closed]);
        assert_eq!(hint.status, None, "the operand must not be pushed as-is");

        // `NOT priority=0` has none, so nothing is pushed. Pushing `priority=0`
        // would prefilter for exactly what the query rejects.
        let hint = q("NOT priority=0").filter_hint();
        assert!(hint.is_empty());

        // A negated group is dropped whole.
        let hint = q("NOT (status=open AND label=ui)").filter_hint();
        assert!(hint.is_empty());
    }

    // -----------------------------------------------------------------------
    // as_filter: what SQL can and cannot answer alone
    // -----------------------------------------------------------------------

    #[test]
    fn and_chains_push_down_whole() {
        let f = q("status=open AND priority<=1 AND label=infra")
            .as_filter()
            .expect("an AND chain of simple comparisons is pure SQL");
        assert_eq!(f.status, Some(Status::Open));
        assert_eq!(f.min_priority, Some(Priority::HIGH));
        assert_eq!(f.labels_all, vec!["infra".to_string()]);
    }

    /// The DSL compares P-numbers; `IssueFilter` states bounds in urgency, which
    /// runs the other way. `priority<=1` ("P0 or P1") is therefore
    /// `min_priority`, not `max_priority`. Crossing these over inverts the filter
    /// and silently returns the backlog instead of the fires.
    #[test]
    fn priority_bounds_map_onto_urgency_not_onto_the_number() {
        assert_eq!(
            q("priority<=1").as_filter().unwrap().min_priority,
            Some(Priority(1))
        );
        assert_eq!(
            q("priority<1").as_filter().unwrap().min_priority,
            Some(Priority(0))
        );
        assert_eq!(
            q("priority>=2").as_filter().unwrap().max_priority,
            Some(Priority(2))
        );
        assert_eq!(
            q("priority>2").as_filter().unwrap().max_priority,
            Some(Priority(3))
        );
        // Satisfiable by nothing, and said so in SQL rather than by scanning.
        assert_eq!(
            q("priority>4").as_filter().unwrap().max_priority,
            Some(Priority(5))
        );
        // No "not this priority" column.
        assert_eq!(q("priority!=2").as_filter(), None);

        // The direction that matters, end to end: a P0 is urgent, so it is in
        // `priority<=1` and out of `priority>=2`.
        let p0 = &corpus()[0];
        assert_eq!(p0.priority, Priority::CRITICAL);
        assert!(q("priority<=1").matches(p0));
        assert!(!q("priority>=2").matches(p0));
        assert!(admits(&q("priority<=1").as_filter().unwrap(), p0));
        assert!(!admits(&q("priority>=2").as_filter().unwrap(), p0));
    }

    #[test]
    fn not_on_status_and_type_pushes_down_but_nothing_else_does() {
        let f = q("NOT status=closed AND NOT type=epic").as_filter().unwrap();
        assert_eq!(f.exclude_statuses, vec![Status::Closed]);
        assert_eq!(f.exclude_types, vec![IssueType::Epic]);
        // `!=` is the same thing said differently.
        let g = q("status!=closed AND type!=epic").as_filter().unwrap();
        assert_eq!(f, g);

        assert_eq!(q("NOT assignee=alice").as_filter(), None);
        assert_eq!(q("NOT (status=open OR status=blocked)").as_filter(), None);
        assert_eq!(q("NOT NOT status=open").as_filter(), None);
    }

    #[test]
    fn an_all_labels_or_becomes_labels_any() {
        let f = q("label=infra OR label=ui OR label=1-alpha")
            .as_filter()
            .unwrap();
        assert_eq!(f.labels_any, vec!["infra", "ui", "1-alpha"]);

        // Under an AND, and mixed with labels_all.
        let f = q("status=open AND label=x AND (label=infra OR label=ui)")
            .as_filter()
            .unwrap();
        assert_eq!(f.labels_all, vec!["x"]);
        assert_eq!(f.labels_any, vec!["infra", "ui"]);

        // Two OR groups need two `labels_any` slots and there is only one.
        assert_eq!(
            q("(label=a OR label=b) AND (label=c OR label=d)").as_filter(),
            None
        );
        // Not all-labels, not pushable.
        assert_eq!(q("label=a OR status=open").as_filter(), None);
        assert_eq!(q("label=a OR label!=b").as_filter(), None);
    }

    #[test]
    fn fields_with_no_column_fall_back_to_memory() {
        for src in ["id=bd-1", "notes=x", "started>7d", r#"spec="spec-*""#] {
            assert_eq!(q(src).as_filter(), None, "{src}");
        }
        // `text` searches title *and* description, so it is only ever a hint:
        // an exact filter would return issues whose description matched.
        let query = q("title=infra");
        assert_eq!(query.as_filter(), None);
        assert_eq!(query.filter_hint().text, Some("infra".into()));
        // ...and the negation of a widened condition cannot be pushed at all.
        assert!(q("title!=infra").filter_hint().is_empty());
    }

    /// `IssueFilter::parent` selects every *descendant*; the predicate can only
    /// see the edges on the issue in hand, so it selects direct children. The
    /// filter is the wider of the two, which makes it a sound prefilter and an
    /// unsound exact answer — claiming exactness here would hand grandchildren
    /// back to a caller that had skipped the predicate.
    #[test]
    fn parent_is_a_prefilter_because_sql_walks_the_graph_and_memory_cannot() {
        let query = q("parent=bd-1");
        assert_eq!(query.as_filter(), None);
        assert_eq!(query.filter_hint().parent, Some("bd-1".into()));

        let child = &corpus()[4];
        assert!(query.matches(child));
        assert!(admits(&query.filter_hint(), child));
    }

    #[test]
    fn contradictory_conjuncts_do_not_silently_lose_one() {
        // Upstream overwrites the slot here and answers `status=closed`. Refuse
        // instead: the filter is not the query, so memory has the last word.
        let query = q("status=open AND status=closed");
        assert_eq!(query.as_filter(), None);
        // The prefilter keeps the first, which is still a necessary condition
        // (vacuously — nothing matches).
        assert_eq!(query.filter_hint().status, Some(Status::Open));
        assert!(corpus().iter().all(|i| !query.matches(i)));
    }

    // -----------------------------------------------------------------------
    // Semantics
    // -----------------------------------------------------------------------

    #[test]
    fn a_duration_looks_backwards() {
        // `created>7d` is "created within the last 7 days", i.e.
        // created_at > now - 7d. Inverting this is the classic bug: it turns
        // "recent" into "ancient" and quietly returns the wrong half of the DB.
        let recent = Issue {
            created_at: days_ago(2),
            ..issue("bd-recent")
        };
        let ancient = Issue {
            created_at: days_ago(400),
            ..issue("bd-ancient")
        };

        let within = q("created>7d");
        assert!(within.matches(&recent));
        assert!(!within.matches(&ancient));
        // The filter's bounds are strict, so `>` maps straight across.
        assert_eq!(within.as_filter().unwrap().created_after, Some(days_ago(7)));

        let older = q("created<7d");
        assert!(!older.matches(&recent));
        assert!(older.matches(&ancient));
        assert_eq!(older.as_filter().unwrap().created_before, Some(days_ago(7)));

        // An inclusive operator against a strict bound is nudged, not widened.
        assert_eq!(
            q("created>=7d").as_filter().unwrap().created_after,
            Some(days_ago(7) - chrono::TimeDelta::nanoseconds(1)),
        );
    }

    #[test]
    fn a_date_equality_is_a_whole_day() {
        let f = q("created=2026-07-12").as_filter().unwrap();
        let midnight = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        assert_eq!(
            f.created_after,
            Some(midnight - chrono::TimeDelta::nanoseconds(1))
        );
        assert_eq!(
            f.created_before,
            Some(Utc.with_ymd_and_hms(2026, 7, 13, 0, 0, 0).unwrap())
        );
        // The bounds have to include midnight itself, not skim past it.
        let at_midnight = Issue {
            created_at: midnight,
            ..issue("bd-midnight")
        };
        assert!(q("created=2026-07-12").matches(&at_midnight));
        assert!(admits(&f, &at_midnight));

        let noon = Issue {
            created_at: Utc.with_ymd_and_hms(2026, 7, 12, 12, 0, 0).unwrap(),
            ..issue("bd-noon")
        };
        assert!(q("created=2026-07-12").matches(&noon));
        assert!(!q("created=2026-07-13").matches(&noon));
    }

    #[test]
    fn a_missing_timestamp_answers_no_question_about_itself() {
        // An issue that was never closed is not "closed on some other day", any
        // more than SQL's NULL is. Both `closed<7d` and `closed!=<date>` reject
        // it — and the filter, which relies on NULL comparing false, agrees.
        let open = issue("bd-open");
        assert!(!q("closed<7d").matches(&open));
        assert!(!q("closed>7d").matches(&open));
        assert!(!q("closed!=2026-01-01").matches(&open));
        assert!(!q("started>7d").matches(&open));
    }

    #[test]
    fn exact_string_matching_is_case_sensitive_but_search_is_not() {
        let c = corpus();
        let (alice_lower, alice_upper) = (&c[0], &c[3]);
        assert_eq!(alice_lower.assignee, "alice");
        assert_eq!(alice_upper.assignee, "Alice");

        // Equality is what SQL's `=` does. Folding case here would make the
        // prefilter (which does not fold) narrower than the predicate.
        assert!(q("assignee=alice").matches(alice_lower));
        assert!(!q("assignee=alice").matches(alice_upper));
        assert!(!q("label=INFRA").matches(alice_lower));

        // Title/description search is a substring match, and is case-insensitive
        // like the `text` column it prefilters through.
        let infra_title = &c[2];
        assert_eq!(infra_title.title, "Fix the INFRA pipeline");
        assert!(q("title=infra").matches(infra_title));

        // Status and type values are normalized, though: `Status::from` does not
        // lowercase, so an unnormalized `status=OPEN` would match nothing.
        assert!(q("status=OPEN").matches(alice_lower));
        assert_eq!(q("status=OPEN").as_filter().unwrap().status, Some(Status::Open));
    }

    #[test]
    fn absent_values_mean_absence() {
        let c = corpus();
        let bare = c.last().unwrap();
        assert!(q("assignee=none").matches(bare));
        assert!(q("label=none").matches(bare));
        assert!(q("desc=none").matches(bare));
        assert!(!q("assignee=none").matches(&c[0]));
        // ...and there is no column for "is null", so they stay in memory.
        assert_eq!(q("assignee=none").as_filter(), None);
        assert!(q("assignee=none").filter_hint().is_empty());
    }

    #[test]
    fn wildcards_are_a_prefix_match() {
        let c = corpus();
        // `*` is not an identifier character, so a wildcard has to be quoted.
        // That is deliberate: leaving it out of the identifier set is what keeps
        // `a*b` from lexing as one token in some future glob syntax.
        assert!(matches!(
            parse("id=bd-*"),
            Err(Error::UnexpectedChar { ch: '*', .. })
        ));

        assert!(q(r#"id="bd-*""#).matches(&c[0]));
        assert!(q("id=bd-1").matches(&c[0]));
        assert!(!q("id=bd-1").matches(&c[1]));
        assert!(q(r#"spec="spec-*""#).matches(&c[4]));
        // A prefix is not an equality, so it must not be pushed as one.
        assert_eq!(q(r#"spec="spec-*""#).as_filter(), None);
        assert!(q(r#"spec="spec-*""#).filter_hint().is_empty());
        assert_eq!(
            q("spec=spec-7").as_filter().unwrap().spec_id,
            Some("spec-7".into())
        );
    }

    #[test]
    fn precedence_and_grouping_decide_the_answer() {
        // `a OR b AND c` is `a OR (b AND c)` — bd-1 is open but is not a P1
        // feature, and matches only because of the OR's left arm.
        let c = corpus();
        let open_bug = &c[0];
        assert!(q("status=open OR type=feature AND priority=1").matches(open_bug));
        // Parenthesized, the same tokens exclude it.
        assert!(!q("(status=open OR type=feature) AND priority=1").matches(open_bug));
    }

    #[test]
    fn the_public_errors_are_the_documented_ones() {
        assert_eq!(parse("bogus=1").unwrap_err(), Error::UnknownField("bogus".into()));
        assert_eq!(
            parse(r#"title="oops"#).unwrap_err(),
            Error::UnterminatedString(6)
        );
        assert_eq!(parse("").unwrap_err(), Error::EmptyQuery);
        assert!(matches!(parse("status<open"), Err(Error::BadOperator { .. })));
        assert!(matches!(parse("priority=9"), Err(Error::InvalidValue { .. })));
        assert!(matches!(parse("label=-5-alpha"), Err(Error::UnexpectedChar { .. })));
    }

    #[test]
    fn a_query_is_stable_once_parsed() {
        // Durations resolve at parse time, so the SQL filter and the predicate
        // cannot disagree about "now" even if minutes pass between them.
        let query = parse_at("created>7d", now()).unwrap();
        assert_eq!(query, parse_at("created>7d", now()).unwrap());
        assert_ne!(
            query,
            parse_at("created>7d", now() + chrono::TimeDelta::hours(1)).unwrap()
        );
    }
}
