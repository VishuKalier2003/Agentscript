use std::collections::BTreeSet;

use serde_json::json;

use super::*;
use crate::authority::{changes_own_autonomy, runs_mutating_crane};

/** Every trigger kind, with representative values, for exhaustive checks
 * Input
    - None
 * Output
    - Vec<Trigger>
*/
fn every_trigger() -> Vec<Trigger> {
    let mut triggers = Vec::new();
    for level in Autonomy::ALL {
        triggers.push(Trigger::Promote(*level));
        triggers.push(Trigger::Demote(*level));
    }
    triggers.push(Trigger::Violation("source_changed".into()));
    triggers.push(Trigger::Violation("unauthorized_effect".into()));
    triggers.push(Trigger::Quarantine("manual".into()));
    for condition in Condition::ALL {
        triggers.push(Trigger::Evidence(*condition));
    }
    triggers
}

/** Every combination of autonomy, safety, incident counter, and evidence set worth checking
 * Input
    - None
 * Output
    - Vec<State>
*/
fn every_state() -> Vec<State> {
    let mut states = Vec::new();
    for autonomy in Autonomy::ALL {
        for safety in [
            SafetyState::Active,
            SafetyState::Degraded,
            SafetyState::Quarantined,
        ] {
            for violations in [0, 1, 2] {
                for evidence in [
                    BTreeSet::new(),
                    BTreeSet::from([Condition::HumanApproval]),
                    BTreeSet::from([Condition::VerifiedRepair]),
                ] {
                    states.push(State {
                        autonomy: *autonomy,
                        safety,
                        reason: None,
                        violations,
                        evidence,
                    });
                }
            }
        }
    }
    states
}

/** Apply a sequence of triggers, requiring each to be legal
 * Input
    - policy: &AutonomyPolicy - policy
    - state: State - start
    - triggers: &[(Trigger, Actor)] - triggers
 * Output
    - State
*/
fn run(policy: &AutonomyPolicy, mut state: State, triggers: &[(Trigger, Actor)]) -> State {
    for (trigger, actor) in triggers {
        state = step(policy, &state, trigger, *actor)
            .unwrap_or_else(|error| panic!("{trigger:?} by {actor:?}: {error}"))
            .0;
    }
    state
}

/** Autonomy rises one step at a time, by a human, exactly as the task example lists:
 * observe -> assisted -> delegated -> autonomous
 */
#[test]
fn promotions_follow_the_ladder() {
    let policy = AutonomyPolicy::default();
    let mut state = State::initial(Autonomy::Observe);
    for to in [
        Autonomy::Assisted,
        Autonomy::Delegated,
        Autonomy::Autonomous,
    ] {
        let (next, changes) = step(&policy, &state, &Trigger::Promote(to), Actor::Human).unwrap();
        assert_eq!(next.autonomy, to);
        assert_eq!(
            changes,
            vec![Change {
                dimension: "autonomy",
                from: state.autonomy.name(),
                to: to.name()
            }]
        );
        state = next;
    }
    let error = step(
        &policy,
        &state,
        &Trigger::Promote(Autonomy::Autonomous),
        Actor::Human,
    )
    .unwrap_err();
    assert!(error.contains("must raise autonomy"));
}

/** Skipping a step, promoting past the policy maximum, promoting while degraded or quarantined,
 * and promotions by Crane are all illegal; without single-step promotion skipping is allowed
 */
#[test]
fn illegal_promotions_are_rejected() {
    let policy = AutonomyPolicy::default();
    let observe = State::initial(Autonomy::Observe);
    let error = step(
        &policy,
        &observe,
        &Trigger::Promote(Autonomy::Delegated),
        Actor::Human,
    )
    .unwrap_err();
    assert!(
        error.contains("one step at a time: observe can only become assisted"),
        "{error}"
    );
    let error = step(
        &policy,
        &observe,
        &Trigger::Promote(Autonomy::Assisted),
        Actor::Crane,
    )
    .unwrap_err();
    assert!(error.contains("only a human"));

    let capped = AutonomyPolicy {
        max_autonomy: Autonomy::Delegated,
        ..AutonomyPolicy::default()
    };
    let delegated = State::initial(Autonomy::Delegated);
    let error = step(
        &capped,
        &delegated,
        &Trigger::Promote(Autonomy::Autonomous),
        Actor::Human,
    )
    .unwrap_err();
    assert!(error.contains("at most delegated"));

    for safety in [SafetyState::Degraded, SafetyState::Quarantined] {
        let state = State {
            safety,
            ..State::initial(Autonomy::Assisted)
        };
        let error = step(
            &policy,
            &state,
            &Trigger::Promote(Autonomy::Delegated),
            Actor::Human,
        )
        .unwrap_err();
        assert!(
            error.contains("only be promoted while safety is active"),
            "{error}"
        );
    }

    let free = AutonomyPolicy {
        single_step: false,
        ..AutonomyPolicy::default()
    };
    let (next, _) = step(
        &free,
        &observe,
        &Trigger::Promote(Autonomy::Autonomous),
        Actor::Human,
    )
    .unwrap();
    assert_eq!(next.autonomy, Autonomy::Autonomous);
}

/** A human may demote to any lower level at any safety state; demoting upwards or by Crane is not
 * legal
 */
#[test]
fn demotions() {
    let policy = AutonomyPolicy::default();
    for safety in [
        SafetyState::Active,
        SafetyState::Degraded,
        SafetyState::Quarantined,
    ] {
        let state = State {
            safety,
            ..State::initial(Autonomy::Autonomous)
        };
        let (next, changes) = step(
            &policy,
            &state,
            &Trigger::Demote(Autonomy::Observe),
            Actor::Human,
        )
        .unwrap();
        assert_eq!(next.autonomy, Autonomy::Observe);
        assert_eq!(next.safety, safety, "demotion never touches safety");
        assert_eq!(changes.len(), 1);
    }
    let assisted = State::initial(Autonomy::Assisted);
    assert!(step(
        &policy,
        &assisted,
        &Trigger::Demote(Autonomy::Delegated),
        Actor::Human
    )
    .is_err());
    assert!(step(
        &policy,
        &assisted,
        &Trigger::Demote(Autonomy::Assisted),
        Actor::Human
    )
    .is_err());
    assert!(step(
        &policy,
        &assisted,
        &Trigger::Demote(Autonomy::Observe),
        Actor::Crane
    )
    .is_err());
}

/** The two dimensions never mix: over every state and trigger, promotions and demotions change only
 * autonomy, and violations, quarantines, and evidence change only safety
 */
#[test]
fn dimensions_are_independent() {
    let policy = AutonomyPolicy::default();
    for state in every_state() {
        for trigger in every_trigger() {
            for actor in [Actor::Human, Actor::Crane] {
                let Ok((next, changes)) = step(&policy, &state, &trigger, actor) else {
                    continue;
                };
                match trigger {
                    Trigger::Promote(_) | Trigger::Demote(_) => {
                        assert_eq!(next.safety, state.safety, "{trigger:?} changed safety");
                        assert_eq!(next.evidence, state.evidence);
                        assert!(changes.iter().all(|change| change.dimension == "autonomy"));
                    }
                    _ => {
                        assert_eq!(
                            next.autonomy, state.autonomy,
                            "{trigger:?} changed autonomy"
                        );
                        assert!(changes.iter().all(|change| change.dimension == "safety"));
                    }
                }
            }
        }
    }
}

/** No trigger caused by an agent is ever legal, from any state; each is a self-escalation */
#[test]
fn agents_can_never_cause_a_transition() {
    let policy = AutonomyPolicy::default();
    for state in every_state() {
        for trigger in every_trigger() {
            let error = step(&policy, &state, &trigger, Actor::Agent).unwrap_err();
            assert!(error.starts_with(SELF_ESCALATION), "{error}");
        }
    }
}

/** A violation degrades an active session; further violations in the same incident quarantine it
 * at the configured count (default 3); 0 never escalates; quarantine absorbs more violations
 */
#[test]
fn violations_degrade_then_quarantine() {
    let policy = AutonomyPolicy::default();
    let violation = || (Trigger::Violation("source_changed".into()), Actor::Crane);
    let active = State::initial(Autonomy::Delegated);
    let (degraded, changes) = step(&policy, &active, &violation().0, Actor::Crane).unwrap();
    assert_eq!(degraded.safety, SafetyState::Degraded);
    assert_eq!(degraded.violations, 1);
    assert_eq!(
        changes,
        vec![Change {
            dimension: "safety",
            from: "active",
            to: "degraded"
        }]
    );
    let second = run(&policy, degraded.clone(), &[violation()]);
    assert_eq!(second.safety, SafetyState::Degraded);
    let third = run(&policy, second, &[violation()]);
    assert_eq!(third.safety, SafetyState::Quarantined);
    assert!(third
        .reason
        .unwrap()
        .contains("3 violations in one incident"));

    let strict = AutonomyPolicy {
        violations_to_quarantine: 2,
        ..AutonomyPolicy::default()
    };
    assert_eq!(
        run(&strict, degraded.clone(), &[violation()]).safety,
        SafetyState::Quarantined
    );
    let lenient = AutonomyPolicy {
        violations_to_quarantine: 0,
        ..AutonomyPolicy::default()
    };
    let many = run(
        &lenient,
        degraded,
        &[violation(), violation(), violation(), violation()],
    );
    assert_eq!(many.safety, SafetyState::Degraded);
    assert_eq!(many.violations, 5);
}

/** A critical violation, or an explicit quarantine, quarantines from every state; what is critical
 * is configurable
 */
#[test]
fn critical_violations_quarantine_from_any_state() {
    let policy = AutonomyPolicy::default();
    for state in every_state() {
        for (trigger, actor) in [
            (
                Trigger::Violation("unauthorized_effect".into()),
                Actor::Crane,
            ),
            (Trigger::Violation(SELF_ESCALATION.into()), Actor::Crane),
            (Trigger::Violation("budget_exhausted".into()), Actor::Human),
            (Trigger::Quarantine("by hand".into()), Actor::Human),
        ] {
            let (next, _) = step(&policy, &state, &trigger, actor).unwrap();
            assert_eq!(
                next.safety,
                SafetyState::Quarantined,
                "{trigger:?} from {state:?}"
            );
            assert!(
                next.evidence.is_empty(),
                "a violation discards earlier evidence"
            );
        }
    }
    let custom = AutonomyPolicy {
        critical: [SELF_ESCALATION, "tests_failed"]
            .iter()
            .map(|kind| kind.to_string())
            .collect(),
        ..AutonomyPolicy::default()
    };
    let active = State::initial(Autonomy::Delegated);
    let tests = Trigger::Violation("tests_failed".into());
    assert_eq!(
        step(&custom, &active, &tests, Actor::Crane)
            .unwrap()
            .0
            .safety,
        SafetyState::Quarantined
    );
    let effect = Trigger::Violation("unauthorized_effect".into());
    assert_eq!(
        step(&custom, &active, &effect, Actor::Crane)
            .unwrap()
            .0
            .safety,
        SafetyState::Degraded
    );
}

/** Recovery needs a complete configured condition set: a degraded session recovers by a verified
 * repair, a human approval, or a human budget refill; a quarantined one needs a human approval plus
 * a verified repair, a new risk budget, or a new session, never one condition alone; a degraded
 * session without the refill set stays degraded on a refill
 */
#[test]
fn recovery_requires_explicit_conditions() {
    let policy = AutonomyPolicy::default();
    let degraded = State {
        safety: SafetyState::Degraded,
        violations: 1,
        reason: Some("violation".into()),
        ..State::initial(Autonomy::Delegated)
    };
    for (condition, actor) in [
        (Condition::VerifiedRepair, Actor::Crane),
        (Condition::HumanApproval, Actor::Human),
        (Condition::NewRiskBudget, Actor::Human),
    ] {
        let (next, changes) =
            step(&policy, &degraded, &Trigger::Evidence(condition), actor).unwrap();
        assert_eq!(next.safety, SafetyState::Active);
        assert_eq!(next.violations, 0);
        assert!(next.reason.is_none() && next.evidence.is_empty());
        assert_eq!(changes[0].to, "active");
    }
    let mut strict = AutonomyPolicy::default();
    strict
        .degraded
        .any_of
        .retain(|set| !set.contains(&Condition::NewRiskBudget));
    let budget = step(
        &strict,
        &degraded,
        &Trigger::Evidence(Condition::NewRiskBudget),
        Actor::Human,
    )
    .unwrap()
    .0;
    assert_eq!(
        budget.safety,
        SafetyState::Degraded,
        "without the refill set, a budget alone does not repair"
    );

    let quarantined = State {
        safety: SafetyState::Quarantined,
        ..State::initial(Autonomy::Delegated)
    };
    for (single, actor) in [
        (Condition::HumanApproval, Actor::Human),
        (Condition::VerifiedRepair, Actor::Crane),
        (Condition::NewRiskBudget, Actor::Human),
        (Condition::NewSession, Actor::Crane),
    ] {
        let next = step(&policy, &quarantined, &Trigger::Evidence(single), actor)
            .unwrap()
            .0;
        assert_eq!(
            next.safety,
            SafetyState::Quarantined,
            "{single:?} alone lifted a quarantine"
        );
        assert_eq!(missing(&policy, &next).len(), 3);
    }
    for second in [
        (Condition::VerifiedRepair, Actor::Crane),
        (Condition::NewRiskBudget, Actor::Human),
        (Condition::NewSession, Actor::Crane),
    ] {
        let next = run(
            &policy,
            quarantined.clone(),
            &[
                (Trigger::Evidence(Condition::HumanApproval), Actor::Human),
                (Trigger::Evidence(second.0), second.1),
            ],
        );
        assert_eq!(
            next.safety,
            SafetyState::Active,
            "human approval + {:?}",
            second.0
        );
    }
}

/** Evidence only counts from its own source, only during an incident, and only after the latest
 * violation
 */
#[test]
fn evidence_rules() {
    let policy = AutonomyPolicy::default();
    let quarantined = State {
        safety: SafetyState::Quarantined,
        ..State::initial(Autonomy::Delegated)
    };
    assert!(step(
        &policy,
        &quarantined,
        &Trigger::Evidence(Condition::VerifiedRepair),
        Actor::Human
    )
    .unwrap_err()
    .contains("can only come from crane"));
    assert!(step(
        &policy,
        &quarantined,
        &Trigger::Evidence(Condition::HumanApproval),
        Actor::Crane
    )
    .unwrap_err()
    .contains("can only come from human"));

    let active = State::initial(Autonomy::Delegated);
    let (next, changes) = step(
        &policy,
        &active,
        &Trigger::Evidence(Condition::HumanApproval),
        Actor::Human,
    )
    .unwrap();
    assert_eq!(next, active, "evidence is not banked for a future incident");
    assert!(changes.is_empty());

    let approved = run(
        &policy,
        quarantined,
        &[
            (Trigger::Evidence(Condition::HumanApproval), Actor::Human),
            (Trigger::Violation("source_changed".into()), Actor::Crane),
            (Trigger::Evidence(Condition::VerifiedRepair), Actor::Crane),
        ],
    );
    assert_eq!(
        approved.safety,
        SafetyState::Quarantined,
        "the approval predates the latest violation"
    );
}

/** Recovery targets are configurable: a quarantine can recover to degraded first */
#[test]
fn quarantine_can_recover_to_degraded() {
    let mut policy = AutonomyPolicy::default();
    policy.quarantined.to = SafetyState::Degraded;
    let quarantined = State {
        safety: SafetyState::Quarantined,
        ..State::initial(Autonomy::Delegated)
    };
    let partial = run(
        &policy,
        quarantined,
        &[
            (Trigger::Evidence(Condition::HumanApproval), Actor::Human),
            (Trigger::Evidence(Condition::NewRiskBudget), Actor::Human),
        ],
    );
    assert_eq!(partial.safety, SafetyState::Degraded);
    assert!(partial.evidence.is_empty());
    let active = run(
        &policy,
        partial,
        &[(Trigger::Evidence(Condition::VerifiedRepair), Actor::Crane)],
    );
    assert_eq!(active.safety, SafetyState::Active);
}

/** Transitions are deterministic: the same trigger sequence always yields the same states, and
 * across a long pseudo-random sequence the invariants hold at every step (agents change nothing,
 * a quarantine only ends through a set containing a human condition, autonomy rises only by one
 * human step while active)
 */
#[test]
fn transitions_are_deterministic_and_keep_invariants() {
    let policy = AutonomyPolicy::default();
    let triggers = every_trigger();
    let actors = [Actor::Human, Actor::Crane, Actor::Agent];
    let replay = || {
        let mut seed: u64 = 0x5eed;
        let mut state = State::initial(Autonomy::Delegated);
        let mut trace = Vec::new();
        for _ in 0..5000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let trigger = &triggers[(seed >> 33) as usize % triggers.len()];
            let actor = actors[(seed >> 13) as usize % actors.len()];
            let before = state.clone();
            match step(&policy, &state, trigger, actor) {
                Ok((next, _)) => {
                    assert_ne!(actor, Actor::Agent);
                    if before.safety == SafetyState::Quarantined
                        && next.safety != SafetyState::Quarantined
                    {
                        let mut evidence = before.evidence.clone();
                        if let Trigger::Evidence(condition) = trigger {
                            evidence.insert(*condition);
                        }
                        assert!(evidence
                            .iter()
                            .any(|condition| condition.source() == Actor::Human));
                    }
                    if next.autonomy > before.autonomy {
                        assert_eq!(actor, Actor::Human);
                        assert_eq!(before.safety, SafetyState::Active);
                        assert_eq!(
                            Autonomy::ALL
                                .iter()
                                .position(|level| *level == next.autonomy),
                            Autonomy::ALL
                                .iter()
                                .position(|level| *level == before.autonomy)
                                .map(|index| index + 1)
                        );
                    }
                    state = next;
                }
                Err(_) => assert_eq!(state, before),
            }
            trace.push(state.clone());
        }
        trace
    };
    let first = replay();
    assert_eq!(first, replay());
    let reached = first
        .iter()
        .map(|state| (state.autonomy, state.safety))
        .collect::<BTreeSet<_>>();
    assert!(
        reached.len() >= 8,
        "the walk should visit many states: {reached:?}"
    );
}

/** The default policy round-trips through its JSON form, and its version is stable */
#[test]
fn default_policy_round_trips() {
    let policy = AutonomyPolicy::default();
    let parsed = AutonomyPolicy::from_json(&policy.to_json()).unwrap();
    assert_eq!(parsed, policy);
    assert_eq!(parsed.version(), policy.version());
    assert_eq!(AutonomyPolicy::from_json(&json!({})).unwrap(), policy);
    for trigger in every_trigger() {
        assert_eq!(Trigger::from_json(&trigger.to_json()).unwrap(), trigger);
    }
}

/** Invalid policies are rejected with a reason: unknown settings, a first violation that would
 * quarantine, recovery without explicit conditions, quarantine recovery without a human, a
 * critical list without self-escalation, and malformed grants
 */
#[test]
fn invalid_policies_are_rejected() {
    for (policy, expected) in [
        (
            json!({"max_autonmy": "observe"}),
            "unknown setting 'max_autonmy'",
        ),
        (json!({"max_autonomy": "boss"}), "invalid autonomy 'boss'"),
        (json!({"violations_to_quarantine": 1}), "at least 2"),
        (
            json!({"recovery": {"degraded": {"any_of": []}}}),
            "explicit conditions",
        ),
        (
            json!({"recovery": {"degraded": {"any_of": [[]]}}}),
            "explicit conditions",
        ),
        (
            json!({"recovery": {"degraded": {"to": "quarantined"}}}),
            "cannot be quarantined",
        ),
        (
            json!({"recovery": {"quarantined": {"any_of": [["verified_repair"]]}}}),
            "never lifted without a human",
        ),
        (
            json!({"recovery": {"quarantined": {"any_of": [["new_session"]]}}}),
            "never lifted without a human",
        ),
        (
            json!({"recovery": {"quarantined": {"any_of": [["prayer"]]}}}),
            "invalid recovery condition",
        ),
        (
            json!({"critical_violations": ["unauthorized_effect"]}),
            "must include self_escalation",
        ),
        (
            json!({"grants": [{"zone": "auth", "autonomy": "observe", "approved_by": "a", "reason": "b"}]}),
            "above observe",
        ),
        (
            json!({"grants": [{"zone": "auth", "autonomy": "assisted", "reason": "b"}]}),
            "approved_by is required",
        ),
        (
            json!({"grants": [{"zone": "auth", "autonomy": "assisted", "approved_by": "a", "reason": "b", "scope": "all"}]}),
            "unknown setting 'scope'",
        ),
    ] {
        let error = AutonomyPolicy::from_json(&policy).unwrap_err();
        assert!(error.contains(expected), "{policy}: {error}");
    }
}

/** A task-specific grant wins over a general one for its task; other tasks get the general one */
#[test]
fn grants_are_chosen_by_task() {
    let policy = AutonomyPolicy::from_json(&json!({"grants": [
        {"zone": "auth", "autonomy": "assisted", "approved_by": "ciso", "reason": "reviews"},
        {"zone": "auth", "autonomy": "delegated", "task": "SEC-1", "approved_by": "ciso", "reason": "rotation"},
        {"zone": "vault", "autonomy": "delegated", "task": "SEC-2", "approved_by": "ciso", "reason": "migration"},
    ]}))
    .unwrap();
    assert_eq!(
        policy.grant_for("auth", Some("SEC-1")).unwrap().autonomy,
        Autonomy::Delegated
    );
    assert_eq!(
        policy.grant_for("auth", Some("OTHER")).unwrap().autonomy,
        Autonomy::Assisted
    );
    assert_eq!(
        policy.grant_for("auth", None).unwrap().autonomy,
        Autonomy::Assisted
    );
    assert!(policy.grant_for("vault", None).is_none());
    assert!(policy.grant_for("billing", Some("SEC-1")).is_none());
}

/** The transition table lists the task's examples: the promotion ladder, active -> degraded on a
 * violation, and any state -> quarantined on a critical violation
 */
#[test]
fn transition_table_lists_legal_transitions() {
    let rows = AutonomyPolicy::default().transitions();
    let has = |dimension: &str, from: &str, to: &str| {
        rows.iter()
            .any(|row| row["dimension"] == dimension && row["from"] == from && row["to"] == to)
    };
    assert!(has("autonomy", "observe", "assisted"));
    assert!(has("autonomy", "assisted", "delegated"));
    assert!(has("autonomy", "delegated", "autonomous"));
    assert!(has("safety", "active", "degraded"));
    assert!(has("safety", "any", "quarantined"));
    assert!(has("safety", "quarantined", "active"));
    let capped = AutonomyPolicy {
        max_autonomy: Autonomy::Assisted,
        ..AutonomyPolicy::default()
    };
    assert!(!capped
        .transitions()
        .iter()
        .any(|row| row["to"] == "delegated"));
}

/** Shell commands by which an agent would raise its own autonomy are recognized, and denied as
 * mutating crane commands
 */
#[test]
fn self_escalation_commands_are_recognized() {
    for command in [
        "crane autonomy promote claude-s1 --to autonomous",
        "./target/debug/crane.exe autonomy approve claude-s1",
        "echo hi && crane autonomy demote claude-s1 --to observe",
        "crane agent session resume claude-s1",
        "crane agent session extend claude-s1 --actions 9",
    ] {
        assert!(changes_own_autonomy(command), "{command}");
        assert!(runs_mutating_crane(command), "{command}");
    }
    for command in [
        "crane autonomy status claude-s1",
        "crane autonomy history claude-s1 --json",
        "echo crane autonomy",
        "crane agent session show claude-s1",
    ] {
        assert!(!changes_own_autonomy(command), "{command}");
    }
    assert!(!runs_mutating_crane("crane autonomy status"));
}
