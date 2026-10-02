// Session side of the autonomy budget: load the risk-cost model, price and reserve actions at
// authorization, settle them after they run, penalize violations, regenerate on controlled events,
// and refill on a human's word. Every change is a "budget" journal event; the budget itself is
// always replayed from the session's own journal, so sessions never share or leak budget.

use std::fs;
use std::io::ErrorKind;

use serde_json::{json, Value};

use super::{
    BudgetModel, BudgetState, Factors, Facts, PathFacts, MODEL_FILE, REGENERATION_SOURCES,
};
use crate::authority::{AgentAction, Decision, Operation, Proposed, Verdict};
use crate::autonomy::manage::{actor, apply};
use crate::autonomy::{Actor, Condition, Trigger, SELF_ESCALATION};
use crate::repository::root;
use crate::session::ContractSession;
use crate::util::{io_error, now_unix};
use crate::zones::model::{Autonomy, Criticality};

/** Prefix of the reason given when an action costs more than the budget has left */
pub(crate) const RISK_BUDGET_EXHAUSTED: &str = "risk budget exhausted";

/** Violation kind recorded when the budget runs out (it degrades the session to supervision) */
pub(crate) const EXHAUSTION: &str = "risk_budget_exhausted";

/** Load the organization's risk-cost model from .crane/budget.json, or the default without one
 * Input
    - None
 * Output
    - Result<BudgetModel, String>
    - Error if the file is unreadable, not JSON, or invalid
*/
pub(crate) fn load_model() -> Result<BudgetModel, String> {
    match fs::read_to_string(root()?.join(MODEL_FILE)) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map_err(|error| error.to_string())
            .and_then(|value| BudgetModel::from_json(&value))
            .map_err(|error| format!(".crane/{MODEL_FILE}: {error}")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(BudgetModel::default()),
        Err(error) => Err(io_error(error)),
    }
}

/** Return the risk-cost model a session is bound to
 * Input
    - session: &ContractSession - session
 * Output
    - BudgetModel
*/
pub(crate) fn model_of(session: &ContractSession) -> BudgetModel {
    session
        .governance()
        .budget_model
        .clone()
        .unwrap_or_default()
}

/** Replay a session's budget from its own journal, as of now
 * Input
    - session: &ContractSession - session
 * Output
    - BudgetState
*/
pub(crate) fn state(session: &ContractSession) -> BudgetState {
    state_at(session, now_unix())
}

/** Replay a session's budget from its own journal, as of a time (refills expired by then are gone)
 * Input
    - session: &ContractSession - session
    - now: u64 - Unix seconds
 * Output
    - BudgetState
*/
pub(crate) fn state_at(session: &ContractSession, now: u64) -> BudgetState {
    let model = model_of(session);
    let mut state = BudgetState::initial(model.max_for(session.governance().autonomy));
    for event in session.events() {
        let at = event["at"].as_u64().unwrap_or(now);
        for operation in operations(&model, &event) {
            state.apply(&operation, at);
        }
    }
    state.expire(now);
    state
}

/** Translate one journal event into the budget operations it carries: its own budget event, the
 * reservation on an authorized pre-tool event, the consumption on a post-tool event, the penalty
 * on an autonomy violation, the new ceiling on an autonomy promotion or demotion, and the release
 * of pending reservations at a turn boundary
 * Input
    - model: &BudgetModel - the session's model
    - event: &Value - journal event
 * Output
    - Vec<Value> budget operations for BudgetState::apply
*/
pub(crate) fn operations(model: &BudgetModel, event: &Value) -> Vec<Value> {
    let digest = event["arguments_digest"].clone();
    let mut out = Vec::new();
    if event["budget_released"] == true {
        out.push(json!({"kind": "release_all"}));
    }
    match event["event"].as_str() {
        Some("budget") => out.push(event.clone()),
        Some("pre_tool_use" | "permission_request") if !event["budget_reserve"].is_null() => {
            out.push(json!({"kind": "reserve", "amount": event["budget_reserve"]["amount"], "digest": digest, "factors": event["budget_reserve"]["factors"]}));
        }
        Some("post_tool_use") if !event["budget_consume"].is_null() => {
            out.push(json!({"kind": "consume", "amount": event["budget_consume"]["amount"], "compliant": event["budget_consume"]["compliant"], "digest": digest}));
        }
        Some("autonomy") => {
            if !event["budget_penalty"].is_null() {
                let mut penalty = event["budget_penalty"].clone();
                penalty["kind"] = json!("penalty");
                out.push(penalty);
            }
            for change in event["changes"].as_array().into_iter().flatten() {
                if change["dimension"] != "autonomy" {
                    continue;
                }
                let level =
                    |key: &str| Autonomy::parse(change[key].as_str().unwrap_or_default()).ok();
                if let (Some(from), Some(to)) = (level("from"), level("to")) {
                    out.push(json!({"kind": "autonomy", "max": model.max_for(to), "raise": to > from, "note": format!("autonomy {} -> {}", from.name(), to.name())}));
                }
            }
        }
        _ => {}
    }
    out
}

/** List a session's budget events (budget_events): every budget operation its journal carries
 * Input
    - session: &ContractSession - session
 * Output
    - Vec<Value>
*/
pub(crate) fn events(session: &ContractSession) -> Vec<Value> {
    let model = model_of(session);
    session
        .events()
        .iter()
        .flat_map(|event| {
            operations(&model, event).into_iter().map(|mut operation| {
                operation["seq"] = event["seq"].clone();
                operation["at"] = event["at"].clone();
                if let Some(object) = operation.as_object_mut() {
                    for key in [
                        "event",
                        "session_id",
                        "agent",
                        "contract_version",
                        "checkpoints",
                        "binding",
                        "key",
                    ] {
                        object.remove(key);
                    }
                }
                operation
            })
        })
        .collect()
}

/** Journal one budget event
 * Input
    - session: &ContractSession - session
    - kind: &str - event kind
    - fields: Value - more fields
 * Output
    - Result<(), String>
*/
fn record(session: &ContractSession, kind: &str, mut fields: Value) -> Result<(), String> {
    fields["event"] = json!("budget");
    fields["kind"] = json!(kind);
    session.record(fields)
}

/** Describe what an action does, for pricing: its operation, the zones, task scope, and contract
 * coverage of each file it writes, its command, and whether the session is isolated
 * Input
    - session: &ContractSession - session
    - action: &AgentAction - action
 * Output
    - Facts
*/
pub(crate) fn facts(session: &ContractSession, action: &AgentAction) -> Facts {
    let governance = session.governance();
    let runtime = session.runtime(&[]);
    let covered = matches!(action.operation, Operation::Write)
        && runtime.relevant(action).iter().any(|relevant| *relevant);
    let operation = match action.operation {
        Operation::Read => "read",
        Operation::Write
            if !action.files.is_empty()
                && action
                    .files
                    .iter()
                    .all(|change| matches!(change.proposed, Proposed::Delete)) =>
        {
            "delete"
        }
        Operation::Write => "write",
        Operation::Execute => "execute",
        Operation::Other => "other",
    };
    let paths = action
        .files
        .iter()
        .filter_map(|change| runtime.relative(&change.path))
        .map(|path| {
            let exists = session.root_path().join(&path).exists();
            PathFacts {
                criticality: governance
                    .constraint(&path)
                    .map_or(Criticality::Routine, |constraint| constraint.criticality),
                in_task: governance
                    .scope
                    .as_ref()
                    .map(|_| governance.in_scope(&path, exists)),
                covered,
                path,
            }
        })
        .collect();
    Facts {
        operation,
        paths,
        command: action.command.clone(),
        isolated: crate::effects::workspace_of(session).is_some(),
    }
}

/** Price an action under the session's model
 * Input
    - session: &ContractSession - session
    - action: &AgentAction - action
 * Output
    - (u64, Factors) cost and the factors it came from
*/
pub(crate) fn price(session: &ContractSession, action: &AgentAction) -> (u64, Factors) {
    let model = model_of(session);
    let factors = model.factors(&facts(session, action));
    (model.cost(&factors), factors)
}

/** Charge an action the runtime allows: its cost is reserved on the pre-tool event until it runs;
 * when the budget cannot cover it, the action is not autonomous any more: it needs human
 * approval (which costs no budget), the session is degraded once for the exhaustion, and a human
 * refill is requested
 * Input
    - session: &ContractSession - session
    - action: &AgentAction - proposed action
    - verdict: &mut Verdict - the runtime's decision, downgraded when the budget is exhausted
 * Output
    - Result<Option<Value>, String> the budget_reserve field for the pre-tool event, if any
*/
pub(crate) fn authorize(
    session: &ContractSession,
    action: &AgentAction,
    verdict: &mut Verdict,
) -> Result<Option<Value>, String> {
    if verdict.decision != Decision::Allow || action.operation == Operation::Read {
        return Ok(None);
    }
    let (cost, factors) = price(session, action);
    if cost == 0 {
        return Ok(None);
    }
    let state = state(session);
    if cost <= state.available() {
        return Ok(Some(json!({"amount": cost, "factors": factors.to_json()})));
    }
    verdict.decision = Decision::ApprovalRequired;
    verdict.reasons.push(format!(
        "{RISK_BUDGET_EXHAUSTED}: this action costs {cost} and {} of {} is available; a human must approve it, or refill the budget with 'crane autonomy refill {} --amount N --reason TEXT --approver NAME --expires DURATION'",
        state.available(),
        state.max,
        session.id()
    ));
    if !state.refill_requested {
        record(
            session,
            "refill_requested",
            json!({"amount": cost, "note": format!("{} available", state.available())}),
        )?;
        let _ = apply(
            session,
            Trigger::Violation(EXHAUSTION.into()),
            Actor::Crane,
            "the risk budget cannot cover an autonomous action",
        );
    }
    Ok(None)
}

/** Work out what an executed action consumes: an autonomously authorized action its reservation
 * (or its price, when it was never reserved), an approved one nothing
 * Input
    - session: &ContractSession - session
    - action: &AgentAction - action that ran
 * Output
    - Option<u64> points to consume, None when nothing is charged
*/
pub(crate) fn consumption(session: &ContractSession, action: &AgentAction) -> Option<u64> {
    if action.operation == Operation::Read {
        return None;
    }
    let decision = session
        .events()
        .into_iter()
        .rev()
        .find(|event| {
            matches!(
                event["event"].as_str(),
                Some("pre_tool_use" | "permission_request")
            ) && event["arguments_digest"] == action.digest.as_str()
        })
        .and_then(|event| event["decision"].as_str().map(String::from));
    let amount = match decision.as_deref() {
        Some("approval_required" | "deny") => return None,
        Some(_) => state(session)
            .reserved
            .get(&action.digest)
            .copied()
            .unwrap_or_else(|| price(session, action).0),
        None => price(session, action).0,
    };
    (amount > 0).then_some(amount)
}

/** Regenerate budget for sustained compliance: every configured number of compliant actions in a
 * row earns points, up to the per-session cap, never above the maximum
 * Input
    - session: &ContractSession - session
 * Output
    - Result<(), String>
*/
pub(crate) fn reward_compliance(session: &ContractSession) -> Result<(), String> {
    let (every, points, cap) = model_of(session).compliance;
    let state = state(session);
    let key = format!("sustained_compliance:{}", state.actions);
    if state.streak == 0
        || !state.streak.is_multiple_of(every)
        || state.compliance_credited >= cap
        || state.current() >= state.max
        || state.credited.contains(&key)
    {
        return Ok(());
    }
    record(
        session,
        "regenerate",
        json!({"source": "sustained_compliance", "amount": points.min(cap - state.compliance_credited), "reference": format!("{} compliant actions in a row", state.streak), "key": key}),
    )
}

/** Check whether a turn boundary has reservations to release (actions authorized but never run)
 * Input
    - session: &ContractSession - session
 * Output
    - bool, true when the boundary event should carry budget_released
*/
pub(crate) fn pending(session: &ContractSession) -> bool {
    !state(session).reserved.is_empty()
}

/** Price a violation: a critical one zeroes the budget (unless the model sets a fixed penalty),
 * any other costs the model's violation penalty; running out of budget is not penalized again
 * Input
    - session: &ContractSession - session
    - kind: &str - violation kind
    - critical: bool - whether the autonomy policy counts it as critical
 * Output
    - Option<Value> the budget_penalty field for the autonomy event
*/
pub(crate) fn penalty(session: &ContractSession, kind: &str, critical: bool) -> Option<Value> {
    if kind == EXHAUSTION {
        return None;
    }
    let model = model_of(session);
    Some(match (critical, model.critical_zeroes) {
        (true, true) => json!({"violation": kind, "zero": true}),
        (true, false) => json!({"violation": kind, "amount": model.critical_penalty}),
        (false, _) => json!({"violation": kind, "amount": model.penalty}),
    })
}

/** Regenerate budget for a controlled event, at most once per event and reference, never above
 * budget_max
 * Input
    - session: &ContractSession - session
    - source: &str - controlled event (see REGENERATION_SOURCES)
    - reference: &str - what it refers to (task state, review, merge commit), "" for the session
 * Output
    - Result<Option<u64>, String> points requested, None when already credited, not configured, or
      the budget is full
*/
pub(crate) fn regenerate(
    session: &ContractSession,
    source: &str,
    reference: &str,
) -> Result<Option<u64>, String> {
    if !REGENERATION_SOURCES.iter().any(|(name, _)| *name == source) {
        return Err(format!(
            "unknown regeneration event '{source}'; expected {}",
            REGENERATION_SOURCES
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let key = format!("{source}:{reference}");
    let amount = model_of(session)
        .regeneration
        .get(source)
        .copied()
        .unwrap_or(0);
    let state = state(session);
    // A full budget has nothing to regenerate; the event can still count once there is room
    if amount == 0 || state.credited.contains(&key) || state.current() >= state.max {
        return Ok(None);
    }
    record(
        session,
        "regenerate",
        json!({"source": source, "reference": reference, "key": key, "amount": amount}),
    )?;
    Ok(Some(amount))
}

/** Credit a controlled event a human or trusted process reports (task milestone, human review,
 * merge); refused, journaled, and quarantining when an agent tries it
 * Input
    - session: &ContractSession - session
    - source: &str - task_milestone, human_review, or merge
    - reference: &str - what it refers to
    - approver: &str - who reports it
 * Output
    - Result<Option<u64>, String>
*/
pub(crate) fn credit(
    session: &ContractSession,
    source: &str,
    reference: &str,
    approver: &str,
) -> Result<Option<u64>, String> {
    refuse_agents(session, "credit")?;
    if !REGENERATION_SOURCES
        .iter()
        .any(|(name, reported)| *name == source && *reported)
    {
        return Err(format!(
            "'{source}' cannot be credited by hand; use task_milestone, human_review, or merge"
        ));
    }
    if reference.trim().is_empty() || approver.trim().is_empty() {
        return Err("crediting needs --reference and --approver".into());
    }
    let credited = regenerate(session, source, reference)?;
    if credited.is_some() {
        session.record(json!({"event": "budget_credit_reported", "source": source, "reference": reference, "approver": approver}))?;
    }
    Ok(credited)
}

/** Refuse a budget change in an agent environment, recording it as self-escalation
 * Input
    - session: &ContractSession - session
    - what: &str - the attempted change
 * Output
    - Result<(), String>
*/
fn refuse_agents(session: &ContractSession, what: &str) -> Result<(), String> {
    if actor() != Actor::Agent {
        return Ok(());
    }
    session.record(json!({"event": "budget_rejected", "attempt": what, "actor": "agent"}))?;
    let _ = apply(
        session,
        Trigger::Violation(SELF_ESCALATION.into()),
        Actor::Crane,
        &format!("an agent tried to {what} its own budget"),
    );
    Err(format!(
        "crane autonomy {what} refuses to run in an agent environment; the session is quarantined"
    ))
}

/** Refill a session's budget on a human's word: the points are temporary, expire unused at the
 * expiry, never raise the budget above budget_max, and count as new_risk_budget recovery evidence;
 * only the session journal changes, never a policy
 * Input
    - session: &ContractSession - session
    - amount: u64 - points requested
    - reason: &str - why
    - approver: &str - who approved it
    - expires_in: u64 - seconds until it expires
 * Output
    - Result<BudgetState, String> the budget afterwards
*/
pub(crate) fn refill(
    session: &ContractSession,
    amount: u64,
    reason: &str,
    approver: &str,
    expires_in: u64,
) -> Result<BudgetState, String> {
    refuse_agents(session, "refill")?;
    if !session.resumable() {
        return Err(format!(
            "contract session {} is {}; it cannot be refilled",
            session.id(),
            session.lifecycle().name()
        ));
    }
    if amount == 0 {
        return Err("--amount must be at least 1".into());
    }
    if reason.trim().is_empty() || approver.trim().is_empty() {
        return Err("a refill needs --reason and --approver".into());
    }
    let model = model_of(session);
    if expires_in == 0 || expires_in > model.max_expiry {
        return Err(format!(
            "--expires must be between 1 second and {} seconds (max_refill_expiry_seconds)",
            model.max_expiry
        ));
    }
    let before = state(session);
    record(
        session,
        "refill",
        json!({
            "amount": amount,
            "granted": amount.min(before.max.saturating_sub(before.current())),
            "reason": reason,
            "approver": approver,
            "expires_at": now_unix() + expires_in,
        }),
    )?;
    apply(
        session,
        Trigger::Evidence(Condition::NewRiskBudget),
        Actor::Human,
        &format!("budget refilled by {approver}"),
    )?;
    Ok(state(session))
}

/** Parse a refill duration: seconds, or a number with s, m, h, or d
 * Input
    - text: &str - such as "3600", "90m", "4h", "2d"
 * Output
    - Result<u64, String> seconds
*/
pub(crate) fn duration(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let (digits, unit) = match text.char_indices().last() {
        Some((index, unit)) if unit.is_ascii_alphabetic() => (&text[..index], unit),
        _ => (text, 's'),
    };
    let value = digits.parse::<u64>().map_err(|_| {
        format!("invalid duration '{text}'; use seconds or a number with s, m, h, or d")
    })?;
    let unit = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        other => {
            return Err(format!(
                "invalid duration unit '{other}'; use s, m, h, or d"
            ))
        }
    };
    Ok(value * unit)
}

/** Describe a session's budget for status output: the state, the model version, and its events
 * Input
    - session: &ContractSession - session
 * Output
    - Value
*/
pub(crate) fn status(session: &ContractSession) -> Value {
    let mut value = state(session).to_json();
    let model = model_of(session);
    value["session_id"] = json!(session.id());
    value["model"] =
        json!({"version": model.version(), "bound": session.governance().budget_model.is_some()});
    value["budget_events"] = json!(events(session));
    value
}
