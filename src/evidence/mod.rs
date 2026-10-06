// Evidence and attestation: the session journal is the append-only, hash-chained record of what
// happened; this module projects it, together with the session's bound document, into evidence
// records (one per meaningful event, each carrying who, what, under which contract and state, with
// what budget, decision, violation, repair, test result, and human intervention) and into a final
// attestation. Both are pure functions of the evidence, so the same journal always yields the same
// bytes; OpenTelemetry output is derived from them and never the other way round. No raw tool
// arguments are ever stored: actions are identified by digests and normalized summaries.

#[cfg(test)] // Compile the module only when running tests, not in production builds
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;

use serde_json::{json, Value};

use crate::agent_session::Governance;
use crate::autonomy::{step, Actor, State, Trigger};
use crate::budget::manage::operations;
use crate::budget::BudgetState;
use crate::repository::root;
use crate::util::{io_error, sha256};
use crate::zones::model::SafetyState;

/** Version of the evidence record and attestation layout */
pub(crate) const EVIDENCE_FORMAT: u64 = 1;

/** Version of the session export layout */
pub(crate) const EXPORT_FORMAT: u64 = 1;

/** The link a journal's first event chains from */
pub(crate) const GENESIS: &str = "genesis";

/** File in .crane naming the organization and team sessions belong to */
pub(crate) const ORGANIZATION_FILE: &str = "organization.json";

/** Compute an event's chain link: the digest of the previous link and the event's canonical JSON
 * (without its own link; object keys are sorted, so the form is canonical)
 * Input
    - previous: &str - previous link, GENESIS for the first event
    - event: &Value - event
 * Output
    - String
*/
pub(crate) fn link(previous: &str, event: &Value) -> String {
    let mut body = event.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("chain");
    }
    sha256(format!("{previous}\n{body}").as_bytes())
}

/** Verify that a journal is append-only: sequence numbers run 1, 2, 3, ... without gaps, time
 * never goes backwards, and every link chains from the one before (a journal written before
 * chaining is reported as legacy; chained events after unchained ones are fine, the reverse is
 * not)
 * Input
    - events: &[Value] - journal events in order
 * Output
    - Value {status: verified|legacy|partial|broken|empty, length, head, problem}
*/
pub(crate) fn verify_chain(events: &[Value]) -> Value {
    let mut previous = GENESIS.to_string();
    let mut chained = 0;
    let mut last_at = 0;
    for (index, event) in events.iter().enumerate() {
        let broken = |problem: String| json!({"status": "broken", "length": events.len(), "head": Value::Null, "problem": problem});
        if event["seq"].as_u64() != Some(index as u64 + 1) {
            return broken(format!(
                "event {} has sequence {} (expected {})",
                index + 1,
                event["seq"],
                index + 1
            ));
        }
        let at = event["at"].as_u64().unwrap_or(0);
        if at < last_at {
            return broken(format!(
                "event {} is dated before the event preceding it",
                index + 1
            ));
        }
        last_at = at;
        match event["chain"].as_str() {
            Some(chain) => {
                if chain != link(&previous, event) {
                    return broken(format!("event {} does not chain from the event before it (edited, inserted, or removed)", index + 1));
                }
                previous = chain.to_string();
                chained += 1;
            }
            None if chained > 0 => {
                return broken(format!(
                    "event {} has no chain link after chained events",
                    index + 1
                ))
            }
            None => {}
        }
    }
    let status = match (events.len(), chained) {
        (0, _) => "empty",
        (length, chained) if chained == length => "verified",
        (_, 0) => "legacy",
        _ => "partial",
    };
    json!({"status": status, "length": events.len(), "head": (chained > 0).then_some(previous), "problem": Value::Null})
}

/** Load the organization and team from .crane/organization.json (its policies list, the
 * organization's own policies, is read by the policy activation state); without it, the
 * organization is the owner in the origin remote's URL and the team is unspecified
 * Input
    - None
 * Output
    - Result<Value, String> {organization, team}
    - Error for an invalid file
*/
pub(crate) fn load_organization() -> Result<Value, String> {
    match fs::read_to_string(root()?.join(ORGANIZATION_FILE)) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text)
                .map_err(|error| format!(".crane/{ORGANIZATION_FILE}: {error}"))?;
            let map = value
                .as_object()
                .ok_or_else(|| format!(".crane/{ORGANIZATION_FILE} must be a JSON object"))?;
            if let Some(unknown) = map
                .keys()
                .find(|key| !["organization", "team", "policies"].contains(&key.as_str()))
            {
                return Err(format!(".crane/{ORGANIZATION_FILE} has unknown setting '{unknown}'; expected organization, team, policies"));
            }
            let text = |key: &str| {
                map.get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            };
            Ok(json!({"organization": text("organization"), "team": text("team")}))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            // The connected repository's owner (or the origin remote's) names the organization
            Ok(json!({"organization": crate::repo::owner_and_name().0, "team": Value::Null}))
        }
        Err(error) => Err(io_error(error)),
    }
}

/** Summarize a shell command without keeping it: the programs it runs (first word of each
 * pipeline or list segment, without paths) and how many arguments it has; arguments, values, and
 * secrets are never kept
 * Input
    - command: &str - command text
 * Output
    - Value {programs, arguments}
*/
pub(crate) fn summarize_command(command: &str) -> Value {
    let mut programs = Vec::new();
    let mut arguments = 0;
    for segment in command.split(['|', ';', '&', '\n']) {
        let words = segment
            .split_whitespace()
            .filter(|word| !word.contains('='))
            .collect::<Vec<_>>();
        if let Some(first) = words.first() {
            let program = first
                .trim_matches(['(', ')', '`', '"', '\''])
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or_default();
            if !program.is_empty()
                && program.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "._-+".contains(character)
                })
            {
                programs.push(program.to_ascii_lowercase());
            }
            arguments += words.len() - 1;
        }
    }
    json!({"programs": programs, "arguments": arguments})
}

/** Snapshot a budget for an evidence record
 * Input
    - budget: &BudgetState - budget
 * Output
    - Value {current, available, reserved, max}
*/
fn snapshot(budget: &BudgetState) -> Value {
    json!({"current": budget.current(), "available": budget.available(), "reserved": budget.reserved_total(), "max": budget.max})
}

/** Classify a journal event
 * Input
    - name: &str - event name
 * Output
    - &'static str category
*/
fn category(name: &str) -> &'static str {
    match name {
        "pre_tool_use" | "permission_request" => "authorization",
        "post_tool_use" => "execution",
        "stop" | "verification" | "user_prompt_submit" => "verification",
        "autonomy" | "autonomy_rejected" | "safety" => "autonomy",
        "budget" | "budget_extended" | "budget_credit_reported" | "budget_rejected" => "budget",
        "session_resumed" => "human",
        "delivery_submitted"
        | "delivery_approval"
        | "delivery_rejection"
        | "delivery_changes_requested"
        | "delivery_exception"
        | "delivery_merged" => "delivery",
        _ => "lifecycle",
    }
}

/** Return a string field or null
 * Input
    - value: &Value - field
 * Output
    - Value
*/
fn or_null(value: &Value) -> Value {
    if value.is_null() {
        Value::Null
    } else {
        value.clone()
    }
}

/** Project a session's journal into evidence records, replaying autonomy, safety, and budget so
 * every record carries the state before and after it
 * Input
    - binding: &Value - the session's bound document (session.json)
    - events: &[Value] - its journal, in order
 * Output
    - Result<Vec<Value>, String>
    - Error if the binding's governance cannot be read
*/
pub(crate) fn records(binding: &Value, events: &[Value]) -> Result<Vec<Value>, String> {
    let governance = Governance::from_json(&binding["governance"])?;
    let policy = governance.autonomy_policy.clone().unwrap_or_default();
    let model = governance.budget_model.clone().unwrap_or_default();
    let organization = governance.organization.clone().unwrap_or(json!({}));
    let mut state = State::initial(governance.autonomy);
    let mut budget = BudgetState::initial(model.max_for(governance.autonomy));
    let mut verification: Option<String> = None;
    let mut decisions: BTreeMap<String, String> = BTreeMap::new();
    let mut out = Vec::new();
    for event in events {
        let at = event["at"].as_u64().unwrap_or(0);
        let name = event["event"].as_str().unwrap_or_default();
        budget.expire(at);
        let before = snapshot(&budget);
        match name {
            "autonomy" => {
                let replayed = Trigger::from_json(event).and_then(|trigger| {
                    step(
                        &policy,
                        &state,
                        &trigger,
                        Actor::parse(event["actor"].as_str().unwrap_or_default())?,
                    )
                });
                state = match replayed {
                    Ok((next, _)) => next,
                    Err(error) => State {
                        safety: SafetyState::Quarantined,
                        reason: Some(format!(
                            "autonomy event {} does not replay: {error}",
                            event["seq"]
                        )),
                        ..state.clone()
                    },
                };
            }
            "safety" => {
                state.safety = SafetyState::parse(event["state"].as_str().unwrap_or("active"))
                    .unwrap_or(SafetyState::Quarantined);
            }
            _ => {}
        }
        for operation in operations(&model, event) {
            budget.apply(&operation, at);
        }
        let digest = event["arguments_digest"].as_str().map(String::from);
        let mut violations = Vec::new();
        let mut repair = Value::Null;
        let mut tests = Value::Null;
        let mut human = Value::Null;
        match name {
            "pre_tool_use" | "permission_request" => {
                if let (Some(digest), Some(decision)) = (&digest, event["decision"].as_str()) {
                    decisions.insert(digest.clone(), decision.to_string());
                }
            }
            "post_tool_use" => {
                violations.extend(
                    event["effect"]["violations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
                let now = event["verification"].as_str().map(String::from);
                if now.as_deref() == Some("pass") && verification.as_deref() == Some("fail") {
                    repair = json!({"kind": "compliance_restored", "by": "agent"});
                }
                verification = now.or(verification);
                if digest
                    .as_ref()
                    .and_then(|digest| decisions.get(digest))
                    .map(String::as_str)
                    == Some("approval_required")
                {
                    human = json!({"action": "approved_tool_call", "by": "human"});
                }
            }
            "autonomy" => {
                if event["trigger"] == "violation" {
                    violations.push(event["kind"].clone());
                }
                if event["condition"] == "verified_repair" {
                    repair = json!({"kind": "verified_repair", "by": "crane"});
                }
                if event["actor"] == "human" {
                    human = json!({"action": event["trigger"], "detail": or_null(&event["to"]).as_str().map(String::from).or_else(|| event["condition"].as_str().map(String::from)).or_else(|| event["reason"].as_str().map(String::from)), "by": "human", "note": event["note"]});
                }
            }
            "autonomy_rejected" | "budget_rejected" => {
                human = json!({"action": "rejected_attempt", "by": event["actor"], "detail": event["error"]});
            }
            "budget" if event["kind"] == "refill" => {
                human = json!({"action": "refill", "by": event["approver"], "detail": event["reason"], "amount": event["amount"], "expires_at": event["expires_at"]});
            }
            "budget_credit_reported" => {
                human = json!({"action": "credit", "by": event["approver"], "detail": event["source"], "reference": event["reference"]});
            }
            "budget_extended" => {
                human = json!({"action": "extend_limits", "by": "human", "detail": {"mutating_actions": event["mutating_actions"], "files": event["files"]}})
            }
            "session_resumed" => human = json!({"action": "resume", "by": event["by"]}),
            "session_start" if event["source"] == "manager" => {
                human = json!({"action": "start", "by": "human"})
            }
            "session_cancelled" => {
                human = json!({"action": "cancel", "by": "human or orchestrator", "detail": event["reason"]})
            }
            "user_prompt_submit" => human = json!({"action": "prompt", "by": "human"}),
            "delivery_approval" | "delivery_rejection" | "delivery_changes_requested" => {
                human = json!({"action": name.trim_start_matches("delivery_"), "by": event["by"], "detail": event["reason"], "via": event["via"], "head": event["head"]});
            }
            "delivery_exception" => {
                human = json!({"action": "exception", "by": event["by"], "detail": event["reason"], "via": event["via"], "check": event["check"], "head": event["head"], "expires_at": event["expires_at"]});
            }
            "delivery_submitted" => {
                tests = json!({"contract": event["contract_tests"], "checks": event["checks"]})
            }
            "stop" => {
                tests = json!({"ordinary": event["tests"], "final_status": event["final_status"]})
            }
            "session_finalized" => {
                tests = json!({"contract": event["contract_tests"], "ordinary": event["ordinary_tests"], "final_status": event["final_status"]});
                violations.extend(event["findings"].as_array().into_iter().flatten().cloned());
            }
            _ => {}
        }
        out.push(json!({
            "evidence_format": EVIDENCE_FORMAT,
            "seq": event["seq"],
            "at": event["at"],
            "event": name,
            "category": category(name),
            "organization": or_null(&organization["organization"]),
            "team": or_null(&organization["team"]),
            "agent": binding["agent"],
            "session": binding["session_id"],
            "task": binding["task_id"],
            "contract": {"version": event["contract_version"]},
            "checkpoints": event["checkpoints"],
            "autonomy": state.autonomy.name(),
            "safety": state.safety.name(),
            "budget": {"before": before, "after": snapshot(&budget)},
            "tool": or_null(&event["tool"]),
            "operation": or_null(&event["operation"]),
            "resources": or_null(&event["resources"]),
            "action_digest": or_null(&event["arguments_digest"]),
            "action_summary": or_null(&event["summary"]),
            "decision": or_null(&event["decision"]),
            "reasons": or_null(&event["reasons"]),
            "violations": violations,
            "repair": repair,
            "tests": tests,
            "human": human,
            "chain": or_null(&event["chain"]),
        }));
    }
    Ok(out)
}

/** Count values into a sorted map
 * Input
    - values: impl Iterator<Item = String> - values
 * Output
    - Value {value: count}
*/
fn tally(values: impl Iterator<Item = String>) -> Value {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for value in values {
        *counts.entry(value).or_insert(0) += 1;
    }
    json!(counts)
}

/** Build a session's attestation from its evidence alone: task, contract, checkpoints, agent,
 * policy versions, action summary, denied actions, approvals, violations, repairs, human
 * interventions, contract and ordinary tests, final state, final decision, and the evidence it
 * rests on; the same evidence always gives the same attestation, and its digest covers all of it
 * Input
    - binding: &Value - the session's bound document (session.json)
    - events: &[Value] - its journal, in order
 * Output
    - Result<Value, String>
*/
pub(crate) fn attest(binding: &Value, events: &[Value]) -> Result<Value, String> {
    let governance = Governance::from_json(&binding["governance"])?;
    let records = records(binding, events)?;
    let policy = governance.autonomy_policy.clone().unwrap_or_default();
    let model = governance.budget_model.clone().unwrap_or_default();
    let organization = governance.organization.clone().unwrap_or(json!({}));
    let authorizations = records
        .iter()
        .filter(|record| record["category"] == "authorization")
        .collect::<Vec<_>>();
    let executions = records
        .iter()
        .filter(|record| record["category"] == "execution")
        .collect::<Vec<_>>();
    let action = |record: &Value| json!({"seq": record["seq"], "tool": record["tool"], "operation": record["operation"], "resources": record["resources"], "action_digest": record["action_digest"], "action_summary": record["action_summary"], "reasons": record["reasons"]});
    let executed = executions
        .iter()
        .filter_map(|record| record["action_digest"].as_str())
        .collect::<BTreeSet<_>>();
    let mut files = BTreeSet::new();
    for event in events
        .iter()
        .filter(|event| event["event"] == "post_tool_use")
    {
        let effect = &event["effect"]["files"];
        for key in ["added", "modified", "deleted"] {
            files.extend(
                effect[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(String::from),
            );
        }
        files.extend(
            effect["renamed"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|pair| pair["to"].as_str())
                .map(String::from),
        );
    }
    let finalized = events
        .iter()
        .rev()
        .find(|event| event["event"] == "session_finalized");
    let cancelled = events
        .iter()
        .any(|event| event["event"] == "session_cancelled");
    let last_stop = events.iter().rev().find(|event| event["event"] == "stop");
    let last = records.last();
    let violations = records
        .iter()
        .flat_map(|record| {
            record["violations"].as_array().into_iter().flatten().map(
                move |kind| json!({"seq": record["seq"], "kind": kind, "event": record["event"]}),
            )
        })
        .collect::<Vec<_>>();
    let decision = match (finalized, cancelled) {
        (Some(event), _) => event["final_status"]
            .as_str()
            .unwrap_or("UNKNOWN")
            .to_string(),
        (None, true) => "CANCELLED".into(),
        _ => "INCOMPLETE".into(),
    };
    let mut reasons = Vec::new();
    if let Some(event) = finalized {
        for test in event["contract_tests"]["failed_tests"]
            .as_array()
            .into_iter()
            .flatten()
        {
            reasons.push(format!(
                "contract test failed: {}",
                test.as_str().unwrap_or_default()
            ));
        }
        for finding in event["findings"].as_array().into_iter().flatten() {
            reasons.push(format!("finding: {}", finding.as_str().unwrap_or_default()));
        }
    } else {
        reasons.push(if cancelled {
            "the session was cancelled before finalization".to_string()
        } else {
            "the session has not been finalized".to_string()
        });
    }
    let mut attestation = json!({
        "attestation_format": EVIDENCE_FORMAT,
        "task": {"task_id": binding["task_id"], "title": governance.task_title},
        "organization": or_null(&organization["organization"]),
        "team": or_null(&organization["team"]),
        "agent": {
            "profile": binding["agent"],
            "provider_session": binding["provider_session"],
            "session": binding["session_id"],
            "models": events.iter().filter_map(|event| event["model"].as_str()).map(String::from).collect::<BTreeSet<_>>(),
        },
        "contract": {
            "version": binding["contracts"]["version"],
            "policies": binding["contracts"]["contracts"].as_array().into_iter().flatten().map(|contract| json!({
                "policy_id": contract["policy_id"],
                "contract_hash": contract["contract_hash"],
                "version": contract["version"],
            })).collect::<Vec<_>>(),
        },
        "checkpoints": binding["contracts"]["contracts"].as_array().into_iter().flatten().map(|contract| json!({
            "policy_id": contract["policy_id"],
            "checkpoint": contract["checkpoint"],
            "checkpoint_sha": contract["checkpoint_sha"],
        })).collect::<Vec<_>>(),
        "policy_versions": {
            "contract": binding["contracts"]["version"],
            "autonomy_policy": policy.version(),
            "budget_model": model.version(),
            "zone_set": governance.zone_set_version,
            "binding_digest": binding["binding_digest"],
        },
        "action_summary": {
            "authorizations": authorizations.len(),
            "by_decision": tally(authorizations.iter().filter_map(|record| record["decision"].as_str().map(String::from))),
            "by_operation": tally(authorizations.iter().filter_map(|record| record["operation"].as_str().map(String::from))),
            "by_tool": tally(authorizations.iter().filter_map(|record| record["tool"].as_str().map(String::from))),
            "executed": executions.len(),
            "files_changed": files,
            "budget_consumed": last.map_or(json!(0), |_| {
                let mut budget = BudgetState::initial(model.max_for(governance.autonomy));
                for event in events {
                    for operation in operations(&model, event) {
                        budget.apply(&operation, event["at"].as_u64().unwrap_or(0));
                    }
                }
                json!(budget.consumed)
            }),
        },
        "denied_actions": authorizations.iter().filter(|record| record["decision"] == "deny").map(|record| action(record)).collect::<Vec<_>>(),
        "approvals": authorizations.iter().filter(|record| record["decision"] == "approval_required").map(|record| {
            let mut entry = action(record);
            entry["outcome"] = json!(if record["action_digest"].as_str().is_some_and(|digest| executed.contains(digest)) { "approved_and_executed" } else { "not_executed" });
            entry
        }).collect::<Vec<_>>(),
        "violations": violations,
        "repairs": records.iter().filter(|record| !record["repair"].is_null()).map(|record| json!({"seq": record["seq"], "repair": record["repair"]})).collect::<Vec<_>>(),
        "human_interventions": records.iter().filter(|record| !record["human"].is_null() && record["human"]["action"] != "prompt").map(|record| json!({"seq": record["seq"], "event": record["event"], "intervention": record["human"]})).collect::<Vec<_>>(),
        "contract_tests": finalized.map_or(Value::Null, |event| event["contract_tests"].clone()),
        "ordinary_tests": finalized.map(|event| event["ordinary_tests"].clone()).or_else(|| last_stop.map(|event| event["tests"].clone())).unwrap_or(Value::Null),
        "final_state": {
            "lifecycle": if finalized.is_some() { "finalized" } else if cancelled { "cancelled" } else { "open" },
            "autonomy": last.map_or(json!(governance.autonomy.name()), |record| record["autonomy"].clone()),
            "safety": last.map_or(json!("active"), |record| record["safety"].clone()),
            "budget": last.map_or(Value::Null, |record| record["budget"]["after"].clone()),
        },
        "final_decision": {"decision": decision, "reasons": reasons},
        "delivery": {
            "submitted": events.iter().rev().find(|event| event["event"] == "delivery_submitted").map(|event| json!({"branch": event["branch"], "head": event["head"], "pull_request": event["pull_request"], "checks": event["checks"], "contract_tests": event["contract_tests"], "criticality": event["criticality"], "rule": event["rule"]})),
            "decisions": events.iter().filter(|event| matches!(event["event"].as_str(), Some("delivery_approval" | "delivery_rejection" | "delivery_changes_requested" | "delivery_exception"))).map(|event| json!({"seq": event["seq"], "event": event["event"], "by": event["by"], "head": event["head"], "via": event["via"], "check": event["check"], "expires_at": event["expires_at"]})).collect::<Vec<_>>(),
            "merged": events.iter().rev().find(|event| event["event"] == "delivery_merged").map(|event| json!({"merge_sha": event["merge_sha"], "head": event["head"], "trusted_checkpoint": event["trusted_checkpoint"], "pull_request": event["pull_request"], "rule": event["rule"], "approvals": event["approvals"], "excepted": event["excepted"], "task_completion": event["task_completion"]})),
        },
        "evidence": {
            "events": events.len(),
            "chain": verify_chain(events),
        },
    });
    attestation["attestation_digest"] = json!(sha256(attestation.to_string().as_bytes()));
    Ok(attestation)
}

/** Convert evidence records into OpenTelemetry's OTLP/JSON trace format (one trace per session,
 * a root span for the session and one span per evidence record, attributes under "crane."), for
 * an OpenTelemetry collector's file receiver; the evidence records stay authoritative
 * Input
    - binding: &Value - the session's bound document
    - records: &[Value] - evidence records
 * Output
    - Value {"resourceSpans": [...]}
*/
pub(crate) fn otlp(binding: &Value, records: &[Value]) -> Value {
    let hex = |text: &str, length: usize| {
        sha256(text.as_bytes())
            .trim_start_matches("sha256:")
            .chars()
            .take(length)
            .collect::<String>()
    };
    let trace = hex(
        &format!("{}{}", binding["session_id"], binding["binding_digest"]),
        32,
    );
    let root = hex(&format!("{trace}:root"), 16);
    let nanos = |record: &Value| format!("{}", record["at"].as_u64().unwrap_or(0) * 1_000_000_000);
    let attribute = |key: &str, value: &Value| -> Option<Value> {
        let value = match value {
            Value::Null => return None,
            Value::String(text) => json!({"stringValue": text}),
            Value::Bool(flag) => json!({"boolValue": flag}),
            Value::Number(number) => number.as_i64().map_or(
                json!({"doubleValue": number.as_f64()}),
                |integer| json!({"intValue": integer.to_string()}),
            ),
            other => json!({"stringValue": other.to_string()}),
        };
        Some(json!({"key": key, "value": value}))
    };
    let mut spans = Vec::new();
    for record in records {
        let fields = [
            ("crane.seq", &record["seq"]),
            ("crane.category", &record["category"]),
            ("crane.tool", &record["tool"]),
            ("crane.operation", &record["operation"]),
            ("crane.decision", &record["decision"]),
            ("crane.action_digest", &record["action_digest"]),
            ("crane.resources", &record["resources"]),
            ("crane.autonomy", &record["autonomy"]),
            ("crane.safety", &record["safety"]),
            (
                "crane.budget.before",
                &record["budget"]["before"]["current"],
            ),
            ("crane.budget.after", &record["budget"]["after"]["current"]),
            ("crane.violations", &record["violations"]),
            ("crane.repair", &record["repair"]),
            ("crane.tests", &record["tests"]),
            ("crane.human", &record["human"]),
            ("crane.contract.version", &record["contract"]["version"]),
            ("crane.chain", &record["chain"]),
        ];
        let failed = record["violations"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
            || record["decision"] == "deny";
        spans.push(json!({
            "traceId": trace,
            "spanId": hex(&format!("{trace}:{}", record["seq"]), 16),
            "parentSpanId": root,
            "name": format!("crane.{}.{}", record["category"].as_str().unwrap_or_default(), record["event"].as_str().unwrap_or_default()),
            "kind": 1,
            "startTimeUnixNano": nanos(record),
            "endTimeUnixNano": nanos(record),
            "attributes": fields.iter().filter_map(|(key, value)| attribute(key, value)).collect::<Vec<_>>(),
            "status": {"code": if failed { 2 } else { 1 }},
        }));
    }
    let first = records.first().map_or("0".into(), nanos);
    let last = records.last().map_or("0".into(), nanos);
    let root_attributes = [
        ("crane.session.id", &binding["session_id"]),
        ("crane.task.id", &binding["task_id"]),
        ("crane.contract.version", &binding["contracts"]["version"]),
    ]
    .iter()
    .filter_map(|(key, value)| attribute(key, value))
    .collect::<Vec<_>>();
    spans.insert(
        0,
        json!({
            "traceId": trace,
            "spanId": root,
            "name": "crane.session",
            "kind": 1,
            "startTimeUnixNano": first,
            "endTimeUnixNano": last,
            "attributes": root_attributes,
            "status": {"code": 1},
        }),
    );
    let organization = records
        .first()
        .map(|record| (record["organization"].clone(), record["team"].clone()))
        .unwrap_or((Value::Null, Value::Null));
    let service = json!("crane");
    let resource = [
        ("service.name", &service),
        ("crane.agent", &binding["agent"]),
        ("crane.organization", &organization.0),
        ("crane.team", &organization.1),
    ]
    .iter()
    .filter_map(|(key, value)| attribute(key, value))
    .collect::<Vec<_>>();
    json!({"resourceSpans": [{
        "resource": {"attributes": resource},
        "scopeSpans": [{"scope": {"name": "crane", "version": env!("CARGO_PKG_VERSION")}, "spans": spans}],
    }]})
}

/** Build the session export: the bound document, the lifecycle, the raw journal (the evidence),
 * the evidence records, the chain verification, and the attestation derived from them
 * Input
    - binding: &Value - bound document
    - lifecycle: &str - lifecycle state
    - events: &[Value] - journal
 * Output
    - Result<Value, String>
*/
pub(crate) fn export(binding: &Value, lifecycle: &str, events: &[Value]) -> Result<Value, String> {
    Ok(json!({
        "export_format": EXPORT_FORMAT,
        "session": binding,
        "lifecycle": lifecycle,
        "journal": events,
        "chain": verify_chain(events),
        "evidence": records(binding, events)?,
        "attestation": attest(binding, events)?,
    }))
}

/** Check an export against itself: re-derive the evidence and attestation from its bound document
 * and journal alone and compare them with what it carries
 * Input
    - export: &Value - a session export
 * Output
    - Result<Value, String> {chain, evidence_consistent, attestation_consistent, attestation}
*/
pub(crate) fn reconstruct(export: &Value) -> Result<Value, String> {
    let binding = &export["session"];
    let events = export["journal"]
        .as_array()
        .ok_or("the export has no journal")?;
    let records = records(binding, events)?;
    let attestation = attest(binding, events)?;
    Ok(json!({
        "chain": verify_chain(events),
        "evidence_consistent": export["evidence"] == json!(records),
        "attestation_consistent": export["attestation"]["attestation_digest"] == attestation["attestation_digest"],
        "evidence": records,
        "attestation": attestation,
    }))
}

/** Render a session for crane session inspect: identity, chain, a timeline of evidence records,
 * and the attestation's decision and counts
 * Input
    - binding: &Value - bound document
    - lifecycle: &str - lifecycle state
    - records: &[Value] - evidence records
    - attestation: &Value - attestation
    - chain: &Value - chain verification
 * Output
    - String
*/
pub(crate) fn render(
    binding: &Value,
    lifecycle: &str,
    records: &[Value],
    attestation: &Value,
    chain: &Value,
) -> String {
    let text = |value: &Value| {
        value.as_str().map_or_else(
            || {
                if value.is_null() {
                    "-".to_string()
                } else {
                    value.to_string()
                }
            },
            String::from,
        )
    };
    let mut out = format!(
        "Session {} ({lifecycle})\n  agent: {} ({})  task: {}\n  organization: {}  team: {}\n  contract: {}  checkpoints: {}\n  journal: {} events, chain {}{}\n",
        text(&binding["session_id"]),
        text(&binding["agent"]),
        text(&binding["provider_session"]),
        text(&binding["task_id"]),
        text(&attestation["organization"]),
        text(&attestation["team"]),
        text(&binding["contracts"]["version"]),
        attestation["checkpoints"].as_array().into_iter().flatten().map(|checkpoint| format!("{}@{}", text(&checkpoint["checkpoint"]), text(&checkpoint["checkpoint_sha"]).chars().take(12).collect::<String>())).collect::<Vec<_>>().join(", "),
        chain["length"],
        text(&chain["status"]),
        chain["problem"].as_str().map_or(String::new(), |problem| format!(" ({problem})")),
    );
    out.push_str("\nTIMELINE\n");
    for record in records {
        let mut parts = vec![format!(
            "#{:<3} {:<13} {:<20}",
            record["seq"],
            text(&record["category"]),
            text(&record["event"])
        )];
        if !record["tool"].is_null() {
            parts.push(format!(
                "{} {}",
                text(&record["tool"]),
                text(&record["operation"])
            ));
        }
        if !record["decision"].is_null() {
            parts.push(format!("-> {}", text(&record["decision"])));
        }
        let before = &record["budget"]["before"]["current"];
        let after = &record["budget"]["after"]["current"];
        if before != after {
            parts.push(format!("budget {before}->{after}"));
        }
        parts.push(format!(
            "[{} {}]",
            text(&record["autonomy"]),
            text(&record["safety"])
        ));
        if let Some(violations) = record["violations"]
            .as_array()
            .filter(|items| !items.is_empty())
        {
            parts.push(format!(
                "violations: {}",
                violations.iter().map(text).collect::<Vec<_>>().join(", ")
            ));
        }
        if !record["repair"].is_null() {
            parts.push(format!("repair: {}", text(&record["repair"]["kind"])));
        }
        if !record["human"].is_null() {
            parts.push(format!("human: {}", text(&record["human"]["action"])));
        }
        out.push_str(&format!("  {}\n", parts.join("  ")));
    }
    let summary = &attestation["action_summary"];
    out.push_str(&format!(
        "\nATTESTATION {}\n  final decision: {}\n  actions: {} authorized ({}), {} executed, {} files changed, budget consumed {}\n  denied: {}  approvals: {}  violations: {}  repairs: {}  human interventions: {}\n  final state: {} / autonomy {} / safety {}\n",
        text(&attestation["attestation_digest"]),
        text(&attestation["final_decision"]["decision"]),
        summary["authorizations"],
        summary["by_decision"].as_object().map_or(String::new(), |counts| counts.iter().map(|(key, value)| format!("{key} {value}")).collect::<Vec<_>>().join(", ")),
        summary["executed"],
        summary["files_changed"].as_array().map_or(0, Vec::len),
        summary["budget_consumed"],
        attestation["denied_actions"].as_array().map_or(0, Vec::len),
        attestation["approvals"].as_array().map_or(0, Vec::len),
        attestation["violations"].as_array().map_or(0, Vec::len),
        attestation["repairs"].as_array().map_or(0, Vec::len),
        attestation["human_interventions"].as_array().map_or(0, Vec::len),
        text(&attestation["final_state"]["lifecycle"]),
        text(&attestation["final_state"]["autonomy"]),
        text(&attestation["final_state"]["safety"]),
    ));
    for reason in attestation["final_decision"]["reasons"]
        .as_array()
        .into_iter()
        .flatten()
    {
        out.push_str(&format!("  - {}\n", text(reason)));
    }
    out
}
