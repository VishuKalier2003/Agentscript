use serde_json::json;

use super::*;

/** Facts for a write of one file
 * Input
    - path: &str - file
    - criticality: Criticality - zone criticality
 * Output
    - Facts
*/
fn write(path: &str, criticality: Criticality) -> Facts {
    Facts {
        operation: "write",
        paths: vec![PathFacts {
            path: path.into(),
            criticality,
            in_task: None,
            covered: false,
        }],
        command: None,
        isolated: false,
    }
}

/** Facts for a shell command
 * Input
    - command: &str - command text
 * Output
    - Facts
*/
fn shell(command: &str) -> Facts {
    Facts {
        operation: "execute",
        paths: Vec::new(),
        command: Some(command.into()),
        isolated: false,
    }
}

/** Price facts under a model
 * Input
    - model: &BudgetModel - model
    - facts: &Facts - facts
 * Output
    - u64
*/
fn cost(model: &BudgetModel, facts: &Facts) -> u64 {
    model.cost(&model.factors(facts))
}

/** Apply events at a time
 * Input
    - state: &mut BudgetState - state
    - events: &[Value] - budget events
    - at: u64 - time
 * Output
    - None
*/
fn apply_all(state: &mut BudgetState, events: &[Value], at: u64) {
    for event in events {
        state.apply(event, at);
    }
}

/** The task's ordering holds under the default model: routine source write < shared library
 * write < production write < privilege escalation
 */
#[test]
fn default_costs_follow_the_risk_ordering() {
    let model = BudgetModel::default();
    let routine = cost(&model, &write("src/app/service.py", Criticality::Routine));
    let shared = cost(&model, &write("lib/money/round.py", Criticality::Routine));
    let production = cost(
        &model,
        &write("deploy/prod/values.yaml", Criticality::Routine),
    );
    let privileged = cost(
        &model,
        &write(".github/workflows/release.yml", Criticality::Routine),
    );
    assert!(routine > 0);
    assert!(routine < shared, "{routine} < {shared}");
    assert!(shared < production, "{shared} < {production}");
    assert!(production < privileged, "{production} < {privileged}");
    assert!(
        cost(&model, &shell("sudo systemctl restart api"))
            > cost(&model, &shell("kubectl rollout restart api"))
    );
}

/** Each factor moves the cost the way the model says: reads are free; criticality, production,
 * shared scope, irreversibility, and contract coverage raise it; an isolated worktree and the task
 * scope lower it
 */
#[test]
fn every_factor_contributes() {
    let model = BudgetModel::default();
    let plain = write("src/a.py", Criticality::Routine);
    let base = cost(&model, &plain);
    assert_eq!(
        cost(
            &model,
            &Facts {
                operation: "read",
                ..plain.clone()
            }
        ),
        0
    );
    let mut critical = plain.clone();
    critical.paths[0].criticality = Criticality::Critical;
    assert!(cost(&model, &critical) > base);
    let mut restricted = plain.clone();
    restricted.paths[0].criticality = Criticality::Restricted;
    assert!(cost(&model, &restricted) > cost(&model, &critical));
    let mut covered = plain.clone();
    covered.paths[0].covered = true;
    assert!(cost(&model, &covered) > base);
    assert_eq!(model.factors(&covered).sensitivity, "contract");
    let mut task = plain.clone();
    task.paths[0].in_task = Some(true);
    assert!(cost(&model, &task) < base);
    let isolated = Facts {
        isolated: true,
        ..plain.clone()
    };
    assert!(cost(&model, &isolated) < base);
    assert_eq!(model.factors(&isolated).environment, "isolated");
    assert!(cost(&model, &shell("git push origin main")) > cost(&model, &shell("ls -la")));
    assert_eq!(
        model.factors(&shell("RM -RF build")).reversibility,
        "irreversible"
    );
    let delete = Facts {
        operation: "delete",
        ..plain
    };
    assert!(cost(&model, &delete) > base);
}

/** The model is configurable: changing a table entry changes prices, and nothing else is needed */
#[test]
fn costs_come_from_the_model() {
    let cheap =
        BudgetModel::from_json(&json!({"operation": {"write": 1}, "privilege_escalation": 0}))
            .unwrap();
    let dear = BudgetModel::from_json(
        &json!({"operation": {"write": 10}, "environment": {"workspace": 300}}),
    )
    .unwrap();
    let facts = write("src/a.py", Criticality::Routine);
    assert!(cost(&cheap, &facts) < cost(&BudgetModel::default(), &facts));
    assert!(cost(&dear, &facts) > cost(&BudgetModel::default(), &facts));
    assert_eq!(
        cost(
            &cheap,
            &write(".github/workflows/ci.yml", Criticality::Routine)
        ),
        cost(&cheap, &facts)
    );
    let patterns =
        BudgetModel::from_json(&json!({"patterns": {"production_paths": ["infra/live/**"]}}))
            .unwrap();
    assert_eq!(
        patterns
            .factors(&write("infra/live/db.tf", Criticality::Routine))
            .environment,
        "production"
    );
    assert_eq!(
        patterns
            .factors(&write("deploy/prod/x.yaml", Criticality::Routine))
            .environment,
        "workspace"
    );
    assert_eq!(BudgetModel::default().max_for(Autonomy::Observe), 0);
    assert!(
        BudgetModel::default().max_for(Autonomy::Autonomous)
            > BudgetModel::default().max_for(Autonomy::Delegated)
    );
}

/** Invalid models are rejected */
#[test]
fn invalid_models_are_rejected() {
    for (model, expected) in [
        (json!({"tokens": 5}), "unknown setting 'tokens'"),
        (json!({"criticality": {"routine": 0}}), "at least 1"),
        (
            json!({"criticality": {"secret": 200}}),
            "unknown setting 'secret'",
        ),
        (json!({"operation": {"write": -1}}), "whole number"),
        (json!({"compliance": {"every": 0}}), "at least 1"),
        (
            json!({"patterns": {"shared_paths": [""]}}),
            "non-empty strings",
        ),
        (
            json!({"penalties": {"critical": "everything"}}),
            "penalties.critical",
        ),
        (json!({"max_refill_expiry_seconds": 0}), "at least 1"),
        (
            json!({"regeneration": {"lottery": 5}}),
            "unknown setting 'lottery'",
        ),
    ] {
        let error = BudgetModel::from_json(&model).unwrap_err();
        assert!(error.contains(expected), "{model}: {error}");
    }
    let model = BudgetModel::default();
    assert_eq!(BudgetModel::from_json(&model.to_json()).unwrap(), model);
    let fixed = BudgetModel::from_json(&json!({"penalties": {"critical": 30}})).unwrap();
    assert!(!fixed.critical_zeroes);
    assert_eq!(fixed.critical_penalty, 30);
}

/** Consumption: reserving holds points (available drops, current does not), consuming spends
 * them, releasing returns them; a consumption never goes below zero
 */
#[test]
fn consumption() {
    let mut state = BudgetState::initial(100);
    state.apply(&json!({"kind": "reserve", "amount": 30, "digest": "a"}), 1);
    assert_eq!(
        (state.current(), state.reserved_total(), state.available()),
        (100, 30, 70)
    );
    state.apply(
        &json!({"kind": "consume", "amount": 30, "digest": "a", "compliant": true}),
        2,
    );
    assert_eq!(
        (state.current(), state.reserved_total(), state.consumed),
        (70, 0, 30)
    );
    state.apply(&json!({"kind": "reserve", "amount": 10, "digest": "b"}), 3);
    state.apply(&json!({"kind": "release", "digest": "b"}), 4);
    assert_eq!(state.available(), 70);
    state.apply(&json!({"kind": "consume", "amount": 500, "digest": "c"}), 5);
    assert_eq!(state.current(), 0);
    assert_eq!(state.consumed, 100);
    assert_eq!(state.streak, 0, "a non-compliant action breaks the streak");
    assert_eq!(state.actions, 2);
}

/** Regeneration and the maximum: controlled events add points, each key only once, never above
 * budget_max; refills never exceed it either
 */
#[test]
fn regeneration_never_exceeds_the_maximum() {
    let mut state = BudgetState::initial(100);
    state.apply(
        &json!({"kind": "regenerate", "source": "merge", "amount": 20, "key": "merge:1"}),
        1,
    );
    assert_eq!(state.current(), 100, "a full budget cannot grow");
    assert_eq!(state.regenerated, 0);
    state.apply(&json!({"kind": "consume", "amount": 50}), 2);
    state.apply(&json!({"kind": "regenerate", "source": "human_review", "amount": 15, "key": "human_review:1"}), 3);
    assert_eq!(state.current(), 65);
    state.apply(
        &json!({"kind": "regenerate", "source": "merge", "amount": 1000, "key": "merge:2"}),
        4,
    );
    assert_eq!(state.current(), 100);
    assert_eq!(state.regenerated, 50);
    assert!(state.credited.contains("merge:2"));
    state.apply(
        &json!({"kind": "refill", "amount": 40, "approver": "lead", "expires_at": 99}),
        5,
    );
    assert_eq!(state.current(), 100);
    assert_eq!(state.refilled, 0);
}

/** Violation penalties: a violation costs its penalty, a critical one zeroes everything (base,
 * refills, and reservations) and resets the compliance streak
 */
#[test]
fn violation_penalties() {
    let mut state = BudgetState::initial(100);
    state.apply(
        &json!({"kind": "consume", "amount": 4, "compliant": true}),
        1,
    );
    state.apply(
        &json!({"kind": "penalty", "violation": "source_changed", "amount": 10}),
        2,
    );
    assert_eq!(
        (state.current(), state.penalized, state.streak),
        (86, 10, 0)
    );
    state.apply(&json!({"kind": "consume", "amount": 40}), 3);
    state.apply(
        &json!({"kind": "refill", "amount": 30, "approver": "lead", "expires_at": 100}),
        4,
    );
    state.apply(&json!({"kind": "reserve", "amount": 5, "digest": "x"}), 5);
    state.apply(
        &json!({"kind": "penalty", "violation": "unauthorized_effect", "zero": true}),
        6,
    );
    assert_eq!(
        (state.current(), state.reserved_total(), state.lots.len()),
        (0, 0, 0)
    );
    assert_eq!(state.penalized, 86);
}

/** Exhaustion and refills: refills are drawn first, cleared the refill request, and what is left
 * of them expires at their expiry
 */
#[test]
fn refills_and_expiry() {
    let mut state = BudgetState::initial(100);
    state.apply(&json!({"kind": "consume", "amount": 100}), 1);
    assert_eq!(state.available(), 0);
    state.apply(&json!({"kind": "refill_requested", "amount": 3}), 2);
    assert!(state.refill_requested);
    state.apply(
        &json!({"kind": "refill", "amount": 30, "approver": "lead", "expires_at": 100}),
        3,
    );
    state.apply(
        &json!({"kind": "refill", "amount": 20, "approver": "ops", "expires_at": 50}),
        4,
    );
    assert!(!state.refill_requested);
    assert_eq!(state.current(), 50);
    assert_eq!(
        state.lots[0].approver, "ops",
        "the earliest expiry is drawn first"
    );
    state.apply(&json!({"kind": "consume", "amount": 25}), 5);
    assert_eq!(state.lots.len(), 1);
    assert_eq!(state.lots[0].amount, 25);
    state.expire(99);
    assert_eq!(state.current(), 25);
    state.expire(100);
    assert_eq!(state.current(), 0);
    assert_eq!(state.expired, 25);
    let mut late = BudgetState::initial(10);
    late.apply(&json!({"kind": "consume", "amount": 10}), 1);
    late.apply(
        &json!({"kind": "refill", "amount": 5, "approver": "a", "expires_at": 20}),
        2,
    );
    late.apply(&json!({"kind": "reserve", "amount": 1, "digest": "d"}), 30);
    assert_eq!(
        late.current(),
        0,
        "an event after the expiry replays without the refill"
    );
}

/** Replay is deterministic: the same events at the same times give the same state */
#[test]
fn replay_is_deterministic() {
    let events = [
        json!({"kind": "reserve", "amount": 7, "digest": "a"}),
        json!({"kind": "consume", "amount": 7, "digest": "a", "compliant": true}),
        json!({"kind": "penalty", "violation": "tests_failed", "amount": 10}),
        json!({"kind": "refill", "amount": 12, "approver": "x", "expires_at": 50}),
        json!({"kind": "regenerate", "source": "merge", "amount": 20, "key": "merge:9"}),
    ];
    let mut first = BudgetState::initial(60);
    apply_all(&mut first, &events, 10);
    let mut second = BudgetState::initial(60);
    apply_all(&mut second, &events, 10);
    assert_eq!(first, second);
    assert_eq!(first.current(), 60);
}
