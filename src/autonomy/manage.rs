// Session side of the autonomy state machine: load the organization's autonomy policy, apply
// triggers to a session (journaling every transition and every rejected attempt), carry a
// quarantine over to the same agent's next session on a task, and describe the state and its
// history.

use std::fs;
use std::io::ErrorKind;

use serde_json::{json, Value};

use super::{
    describe_sets, missing, step, Actor, AutonomyPolicy, Condition, State, Trigger, NOT_VIOLATIONS,
    POLICY_FILE, SELF_ESCALATION,
};
use crate::proposals::store::agent_environment;
use crate::repository::root;
use crate::session::{session_ids, ContractSession};
use crate::util::io_error;
use crate::verify::Report;
use crate::zones::model::{Autonomy, SafetyState};

/** Authority checks in the order they apply; an earlier one is never relaxed by a later one */
const PRECEDENCE: &[&str] = &[
    "Crane metadata (.crane) is never writable by agents",
    "contract: preserve denies changes, target requires them",
    "zones: criticality and zone safety cap autonomy; restricted zones are closed unless an organizational grant opens them",
    "session safety: degraded needs approval, quarantined denies",
    "session autonomy mode: observe denies, assisted needs approval",
    "task scope (delegated mode) and autonomy budget",
];

/** Load the organization's autonomy policy from .crane/autonomy.json, or the default without one
 * Input
    - None
 * Output
    - Result<AutonomyPolicy, String>
    - Error if the file is unreadable, not JSON, or invalid
*/
pub(crate) fn load_policy() -> Result<AutonomyPolicy, String> {
    match fs::read_to_string(root()?.join(POLICY_FILE)) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map_err(|error| error.to_string())
            .and_then(|value| AutonomyPolicy::from_json(&value))
            .map_err(|error| format!(".crane/{POLICY_FILE}: {error}")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(AutonomyPolicy::default()),
        Err(error) => Err(io_error(error)),
    }
}

/** Return the autonomy policy a session is bound to
 * Input
    - session: &ContractSession - session
 * Output
    - AutonomyPolicy
*/
pub(crate) fn policy_of(session: &ContractSession) -> AutonomyPolicy {
    session
        .governance()
        .autonomy_policy
        .clone()
        .unwrap_or_default()
}

/** Return who is running this process: an agent when an agent environment marker is set
 * Input
    - None
 * Output
    - Actor (Human or Agent)
*/
pub(crate) fn actor() -> Actor {
    if agent_environment().is_some() {
        Actor::Agent
    } else {
        Actor::Human
    }
}

/** Apply a trigger to a session: journal an "autonomy" event when the state changes, or an
 * "autonomy_rejected" event when the transition is illegal; an attempt by an agent is then also
 * recorded as a critical self_escalation violation
 * Input
    - session: &ContractSession - session
    - trigger: Trigger - what happened
    - actor: Actor - who caused it
    - note: &str - context for people reading the history
 * Output
    - Result<State, String> the state after the trigger
    - Error explaining why the transition is illegal
*/
pub(crate) fn apply(
    session: &ContractSession,
    trigger: Trigger,
    actor: Actor,
    note: &str,
) -> Result<State, String> {
    let policy = policy_of(session);
    let state = session.activity().state;
    // Raising autonomy is never allowed on an exhausted risk budget
    let exhausted = matches!(trigger, Trigger::Promote(_)) && actor != Actor::Agent && {
        let budget = crate::budget::manage::state(session);
        budget.refill_requested || (budget.max > 0 && budget.available() == 0)
    };
    let outcome = if exhausted {
        Err(
            "autonomy cannot be promoted while the risk budget is exhausted; refill it first"
                .to_string(),
        )
    } else {
        step(&policy, &state, &trigger, actor)
    };
    match outcome {
        Ok((next, changes)) => {
            if next != state {
                let mut event = trigger.to_json();
                event["event"] = json!("autonomy");
                event["actor"] = json!(actor.name());
                event["note"] = json!(note);
                event["changes"] = json!(changes
                    .iter()
                    .map(|change| change.to_json())
                    .collect::<Vec<_>>());
                event["state"] = next.to_json();
                event["policy_version"] = json!(policy.version());
                if let Trigger::Violation(kind) = &trigger {
                    if let Some(penalty) = crate::budget::manage::penalty(
                        session,
                        kind,
                        policy.critical.contains(kind),
                    ) {
                        event["budget_penalty"] = penalty;
                    }
                }
                session.record(event)?;
            }
            Ok(next)
        }
        Err(error) => {
            let mut event = trigger.to_json();
            event["event"] = json!("autonomy_rejected");
            event["actor"] = json!(actor.name());
            event["note"] = json!(note);
            event["error"] = json!(error);
            session.record(event)?;
            if error.starts_with(SELF_ESCALATION) {
                apply(
                    session,
                    Trigger::Violation(SELF_ESCALATION.into()),
                    Actor::Crane,
                    &format!(
                        "an agent tried to {}",
                        trigger.to_json()["trigger"].as_str().unwrap_or_default()
                    ),
                )?;
            }
            Err(error)
        }
    }
}

/** Carry a quarantine over to a new session: when the same agent's latest earlier session on the
 * same task (or without a task) is quarantined, the new session starts quarantined, with
 * new_session recorded as recovery evidence (which only recovers together with a human condition)
 * Input
    - session: &ContractSession - the session just created
 * Output
    - Result<(), String>
*/
pub(crate) fn inherit(session: &ContractSession) -> Result<(), String> {
    let (agent, task, created) = session.identity();
    let mut latest: Option<(u64, String, State)> = None;
    for id in session_ids()? {
        if id == session.id() {
            continue;
        }
        let Ok(Some(other)) = ContractSession::load(&id) else {
            continue;
        };
        let (other_agent, other_task, other_created) = other.identity();
        if other_agent != agent || other_task != task || other_created > created {
            continue;
        }
        if latest
            .as_ref()
            .is_none_or(|(at, _, _)| other_created >= *at)
        {
            latest = Some((other_created, id.clone(), other.activity().state));
        }
    }
    let Some((_, previous, state)) = latest else {
        return Ok(());
    };
    if state.safety != SafetyState::Quarantined {
        return Ok(());
    }
    apply(
        session,
        Trigger::Quarantine(format!(
            "inherited from quarantined session {previous} ({})",
            state.reason.unwrap_or_default()
        )),
        Actor::Crane,
        "a new session of the same agent and task",
    )?;
    apply(
        session,
        Trigger::Evidence(Condition::NewSession),
        Actor::Crane,
        "a new session of the same agent and task",
    )?;
    Ok(())
}

/** Record a verified repair: a full validation by Crane found no behaviour violation (unmet
 * targets are work still to do) while the session was degraded or quarantined
 * Input
    - session: &ContractSession - session
    - report: &Report - the full validation
 * Output
    - Result<(), String>
*/
pub(crate) fn note_validation(session: &ContractSession, report: &Report) -> Result<(), String> {
    let clean = report
        .violations
        .iter()
        .all(|violation| NOT_VIOLATIONS.contains(&violation.violation_type.as_str()));
    if clean && session.activity().safety != SafetyState::Active {
        apply(
            session,
            Trigger::Evidence(Condition::VerifiedRepair),
            Actor::Crane,
            "full validation found no violation",
        )?;
    }
    Ok(())
}

/** Describe a session's autonomy: both dimensions, what recovery still needs, the legal
 * promotion, the effective level, the grants in force, and the precedence of authority checks
 * Input
    - session: &ContractSession - session
 * Output
    - Value
*/
pub(crate) fn status(session: &ContractSession) -> Value {
    let policy = policy_of(session);
    let state = session.activity().state;
    let next = Autonomy::ALL
        .iter()
        .copied()
        .find(|level| *level > state.autonomy)
        .filter(|level| *level <= policy.max_autonomy);
    let promotion = match (state.safety, next) {
        (SafetyState::Active, Some(level)) => json!({"to": level.name(), "by": "human"}),
        (SafetyState::Active, None) => json!(null),
        (safety, _) => json!({"blocked": format!("safety is {}", safety.name())}),
    };
    let recovery = policy.recovery(state.safety).map(|recovery| {
        json!({
            "to": recovery.to.name(),
            "requires": describe_sets(&recovery.any_of),
            "missing": missing(&policy, &state).iter().map(|set| set.iter().map(|condition| condition.name()).collect::<Vec<_>>()).collect::<Vec<_>>(),
        })
    });
    let governance = session.governance();
    let budget = crate::budget::manage::state(session).to_json();
    let grants = governance
        .zones
        .iter()
        .filter(|zone| !zone["granted"].is_null())
        .map(|zone| json!({"zone": zone["zone_id"], "grant": zone["granted"]}))
        .collect::<Vec<_>>();
    json!({
        "session_id": session.id(),
        "lifecycle": session.lifecycle().name(),
        "autonomy": state.autonomy.name(),
        "initial_autonomy": governance.autonomy.name(),
        "safety": state.safety.name(),
        "safety_reason": state.reason,
        "effective_autonomy": state.autonomy.min(state.safety.autonomy_cap()).name(),
        "violations_in_incident": state.violations,
        "evidence": state.evidence.iter().map(|condition| condition.name()).collect::<Vec<_>>(),
        "recovery": recovery,
        "promotion": promotion,
        "restricted_grants": grants,
        "budget": budget,
        "policy": {
            "version": policy.version(),
            "bound": governance.autonomy_policy.is_some(),
            "max_autonomy": policy.max_autonomy.name(),
        },
        "precedence": PRECEDENCE,
    })
}

/** List a session's autonomy history: the initial state, then every transition, recorded
 * evidence, and rejected attempt, in journal order
 * Input
    - session: &ContractSession - session
 * Output
    - Vec<Value>
*/
pub(crate) fn history(session: &ContractSession) -> Vec<Value> {
    let initial = State::initial(session.governance().autonomy);
    let mut entries = vec![json!({
        "seq": 0,
        "kind": "initial",
        "state": initial.to_json(),
    })];
    for event in session.events() {
        let kind = match event["event"].as_str() {
            Some("autonomy")
                if event["changes"]
                    .as_array()
                    .is_some_and(|changes| !changes.is_empty()) =>
            {
                "transition"
            }
            Some("autonomy") => "recorded",
            Some("autonomy_rejected") => "rejected",
            Some("safety") => "legacy",
            _ => continue,
        };
        let mut entry = json!({
            "seq": event["seq"],
            "at": event["at"],
            "kind": kind,
            "trigger": event["trigger"],
            "actor": event["actor"],
            "note": event["note"],
        });
        for key in [
            "to",
            "kind",
            "condition",
            "reason",
            "changes",
            "state",
            "error",
        ] {
            if !event[key].is_null() {
                let name = if key == "kind" { "violation" } else { key };
                entry[name] = event[key].clone();
            }
        }
        if kind == "legacy" {
            entry["changes"] = json!([{"dimension": "safety", "to": event["state"]}]);
        }
        entries.push(entry);
    }
    entries
}
