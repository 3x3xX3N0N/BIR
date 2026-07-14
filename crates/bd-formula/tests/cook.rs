//! Cooking the real primitives, end to end: parse → bind → cook → assert the
//! graph. These are the formulas upstream ships as its smoke-test primitives, so
//! passing them is evidence the compiler agrees with the format's authors about
//! what each construct means.

use std::collections::BTreeMap;

use bd_core::IssueType;
use bd_formula::{Bindings, Error, Plan, cook, parse};

fn cook_str(src: &str, vars: &[(&str, &str)]) -> Result<Plan, Error> {
    let f = parse(src).unwrap();
    let provided: BTreeMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let b = Bindings::bind(&f, &provided).unwrap();
    cook(&f, &b)
}

fn titles(plan: &Plan) -> Vec<&str> {
    plan.issues.iter().map(|i| i.title.as_str()).collect()
}

/// The dependents (blocked issues) of a prerequisite, over `blocks` edges.
fn blocked_by<'a>(plan: &'a Plan, prereq: &str) -> Vec<&'a str> {
    let mut v: Vec<&str> = plan
        .deps
        .iter()
        .filter(|d| d.prerequisite == prereq && d.kind == "blocks")
        .map(|d| d.dependent.as_str())
        .collect();
    v.sort();
    v
}

/// No edge may name an issue the plan never created. The single invariant that,
/// if it ever breaks, means a cooked formula produces a workspace `bd ready`
/// cannot reason about.
fn no_dangling_edges(plan: &Plan) {
    for d in &plan.deps {
        assert!(plan.issue(&d.prerequisite).is_some(), "dangling prereq: {d:?}");
        assert!(plan.issue(&d.dependent).is_some(), "dangling dependent: {d:?}");
    }
}

#[test]
fn a_linear_workflow_becomes_a_chain() {
    let plan = cook_str(
        r#"
        formula = "chain"
        version = 1
        [[steps]]
        id = "a"
        title = "A"
        [[steps]]
        id = "b"
        title = "B"
        needs = ["a"]
        [[steps]]
        id = "c"
        title = "C"
        needs = ["b"]
    "#,
        &[],
    )
    .unwrap();
    assert_eq!(titles(&plan), ["A", "B", "C"]);
    assert_eq!(blocked_by(&plan, "a"), ["b"]);
    assert_eq!(blocked_by(&plan, "b"), ["c"]);
    no_dangling_edges(&plan);
}

#[test]
fn variables_are_substituted_into_titles() {
    let plan = cook_str(
        r#"
        formula = "f"
        version = 1
        [vars.feature]
        required = true
        [[steps]]
        id = "d"
        title = "Design {{feature}}"
    "#,
        &[("feature", "auth")],
    )
    .unwrap();
    assert_eq!(titles(&plan), ["Design auth"]);
}

#[test]
fn a_false_condition_drops_the_step_and_true_keeps_it() {
    let src = r#"
        formula = "cond"
        version = 1
        [vars.deploy]
        default = "false"
        [[steps]]
        id = "build"
        title = "Build"
        [[steps]]
        id = "test"
        title = "Test"
        needs = ["build"]
        [[steps]]
        id = "deploy"
        title = "Deploy"
        needs = ["test"]
        condition = "{{deploy}} == true"
    "#;

    let off = cook_str(src, &[]).unwrap();
    assert_eq!(titles(&off), ["Build", "Test"]);

    let on = cook_str(src, &[("deploy", "true")]).unwrap();
    assert_eq!(titles(&on), ["Build", "Test", "Deploy"]);
    assert_eq!(blocked_by(&on, "test"), ["deploy"]);
}

/// The subtle one: an excluded step in the *middle* of a chain must have the
/// edge through it rewired, not dropped. `report needs deploy needs build`, with
/// deploy excluded, becomes `report needs build`.
#[test]
fn an_excluded_middle_step_is_bypassed_not_left_dangling() {
    let plan = cook_str(
        r#"
        formula = "bypass"
        version = 1
        [vars.deploy]
        default = "false"
        [[steps]]
        id = "build"
        title = "Build"
        [[steps]]
        id = "deploy"
        title = "Deploy"
        needs = ["build"]
        condition = "{{deploy}} == true"
        [[steps]]
        id = "report"
        title = "Report"
        needs = ["deploy"]
    "#,
        &[],
    )
    .unwrap();

    assert_eq!(titles(&plan), ["Build", "Report"]);
    assert_eq!(blocked_by(&plan, "build"), ["report"]);
    assert!(blocked_by(&plan, "deploy").is_empty());
    assert!(plan.issue("deploy").is_none());
    no_dangling_edges(&plan);
}

#[test]
fn a_range_loop_fans_out_with_its_variable_bound() {
    let plan = cook_str(
        r#"
        formula = "loop"
        version = 1
        [[steps]]
        id = "moves"
        title = "Tower moves"
        [steps.loop]
        range = "1..3"
        var = "n"
        [[steps.loop.body]]
        id = "move"
        title = "Move {n}"
    "#,
        &[],
    )
    .unwrap();

    let t = titles(&plan);
    assert!(t.contains(&"Tower moves"), "container missing: {t:?}");
    assert!(t.contains(&"Move 1"));
    assert!(t.contains(&"Move 2"));
    assert!(t.contains(&"Move 3"));

    let kids = plan
        .deps
        .iter()
        .filter(|d| d.kind == "parent-child" && d.prerequisite == "moves")
        .count();
    assert_eq!(kids, 3, "each iteration should be a child of the container");
    no_dangling_edges(&plan);
}

#[test]
fn a_dependent_on_a_loop_blocks_on_every_iteration() {
    let plan = cook_str(
        r#"
        formula = "loopdep"
        version = 1
        [[steps]]
        id = "moves"
        title = "Moves"
        [steps.loop]
        count = 2
        var = "n"
        [[steps.loop.body]]
        id = "move"
        title = "Move {n}"
        [[steps]]
        id = "report"
        title = "Report"
        needs = ["moves"]
    "#,
        &[],
    )
    .unwrap();

    let mut waits: Vec<&str> = plan
        .deps
        .iter()
        .filter(|d| d.dependent == "report" && d.kind == "blocks")
        .map(|d| d.prerequisite.as_str())
        .collect();
    waits.sort();
    assert_eq!(waits, ["moves.move#1", "moves.move#2"]);
    no_dangling_edges(&plan);
}

#[test]
fn a_gate_splits_into_a_wait_and_the_work_that_follows() {
    let plan = cook_str(
        r#"
        formula = "gate"
        version = 1
        [[steps]]
        id = "verify"
        title = "Verify after soak"
        [steps.gate]
        type = "timer"
        await_id = "soak-30m"
        timeout = "30m"
    "#,
        &[],
    )
    .unwrap();

    let gate = plan.issue("verify.gate").expect("a gate issue");
    assert_eq!(gate.issue_type, IssueType::Gate);
    let g = gate.gate.as_ref().unwrap();
    assert_eq!(g.await_type, "timer");
    assert_eq!(g.timeout.as_deref(), Some("30m"));

    assert_eq!(blocked_by(&plan, "verify.gate"), ["verify"]);
    let gate_pos = plan.issues.iter().position(|i| i.local_id == "verify.gate");
    let work_pos = plan.issues.iter().position(|i| i.local_id == "verify");
    assert!(gate_pos < work_pos, "the wait must be created before the work");
    no_dangling_edges(&plan);
}

#[test]
fn extends_is_refused_as_unsupported_not_as_broken() {
    let err = cook_str(
        r#"
        formula = "child"
        version = 1
        extends = ["parent"]
        [[steps]]
        id = "a"
        title = "A"
    "#,
        &[],
    )
    .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)));
    assert!(err.to_string().contains("extends"));
}

#[test]
fn a_non_workflow_type_is_refused_with_its_name() {
    let err = cook_str(
        r#"
        formula = "asp"
        version = 1
        type = "aspect"
        [[steps]]
        id = "a"
        title = "A"
    "#,
        &[],
    )
    .unwrap_err();
    assert!(err.to_string().contains("aspect"));
}
