//! Cook the formulas upstream actually ships (vendored into `tests/fixtures/`,
//! so the test survives a checkout without the reference clone). If the parser
//! or compiler chokes on a real file, it shows here — not in a fixture I tailored
//! to pass.

use std::collections::BTreeMap;

use bd_formula::{Bindings, Plan, cook, parse};

fn cook_fixture(src: &str, vars: &[(&str, &str)]) -> Plan {
    let f = parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
    let provided: BTreeMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let b = Bindings::bind(&f, &provided).unwrap_or_else(|e| panic!("bind failed: {e}"));
    let plan = cook(&f, &b).unwrap_or_else(|e| panic!("cook failed: {e}"));
    assert!(!plan.issues.is_empty(), "a real formula cooked to nothing");
    for d in &plan.deps {
        assert!(plan.issue(&d.prerequisite).is_some(), "dangling: {d:?}");
        assert!(plan.issue(&d.dependent).is_some(), "dangling: {d:?}");
    }
    plan
}

#[test]
fn quick_check_cooks() {
    let plan = cook_fixture(include_str!("fixtures/quick-check.formula.toml"), &[]);
    // lint, test, build, report — and report waits on the other three.
    assert_eq!(plan.issues.len(), 4);
    let waits = plan
        .deps
        .iter()
        .filter(|d| d.dependent == "report")
        .count();
    assert_eq!(waits, 3, "report should block on lint, test, build");
}

#[test]
fn feature_workflow_substitutes_its_required_var() {
    let plan = cook_fixture(
        include_str!("fixtures/feature-workflow.formula.toml"),
        &[("feature_name", "auth")],
    );
    assert!(
        plan.issues.iter().any(|i| i.title == "Design auth"),
        "the feature_name var should reach the title"
    );
}

#[test]
fn the_loop_primitive_cooks_to_three_moves() {
    let plan = cook_fixture(include_str!("fixtures/loop-range.formula.toml"), &[]);
    for n in 1..=3 {
        assert!(
            plan.issues.iter().any(|i| i.title == format!("Move {n}")),
            "Move {n} missing"
        );
    }
}

#[test]
fn the_gate_primitive_cooks_to_a_wait() {
    let plan = cook_fixture(include_str!("fixtures/gate-timer.formula.toml"), &[]);
    assert!(
        plan.issues.iter().any(|i| i.gate.is_some()),
        "the gate primitive should produce a gate issue"
    );
}

#[test]
fn the_condition_primitive_gates_a_step_on_a_var() {
    // Default deploy=false → the deploy step is dropped (2 issues).
    let off = cook_fixture(include_str!("fixtures/condition.formula.toml"), &[]);
    assert_eq!(off.issues.len(), 2, "deploy should be excluded by default");

    // deploy=true → it is included (3 issues).
    let on = cook_fixture(
        include_str!("fixtures/condition.formula.toml"),
        &[("deploy", "true")],
    );
    assert_eq!(on.issues.len(), 3, "deploy should be included when true");
}
