// The Task Details page: everything that happened when Society let an AI agent execute one
// engineering task, read from the records that own it (the task record, its task contract, its
// sessions' bindings and hash-chained journals through the evidence projection, its deliveries,
// and its completion). It is a projection like the rest of the observability layer: it writes
// nothing, shows the contract without any way to change it, and never carries raw tool arguments
// or secrets (actions appear as digests and summaries, as the journal stores them).

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::compact::Act;
use super::dashboard::{actor, Dataset, Keys, Session};
use super::{Full, Query, Run};
use crate::util::now_unix;

/** Parameters the Task Details endpoint accepts */
pub(crate) const PARAMETERS: &[&str] = &["limit"];

/** Default and largest number of timeline events returned */
const EVENTS: (usize, usize) = (2_000, 20_000);

/** Name a risk score's level
 * Input
    - score: f64 - risk score (see dashboard::Keys)
    - critical: u64 - critical violations
 * Output
    - &'static str low, medium, high, or critical
*/
pub(super) fn risk_level(score: f64, critical: u64) -> &'static str {
    if critical > 0 || score >= 30.0 {
        "critical"
    } else if score >= 15.0 {
        "high"
    } else if score >= 5.0 {
        "medium"
    } else {
        "low"
    }
}

/** Return the string items of a JSON array
 * Input
    - value: &Value - array
 * Output
    - Vec<String>
*/
pub(super) fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect()
}

/** Find the rule ("preserve" or "target") a reason names, for "policy/rule" labels
 * Input
    - reasons: &[String] - reasons of a decision
 * Output
    - Option<&'static str>
*/
fn rule(reasons: &[String]) -> Option<&'static str> {
    let text = reasons.join(" ");
    ["preserve", "target"]
        .into_iter()
        .find(|rule| text.contains(&format!("by {rule} ")))
}

/** Label the policies of a decision with the rule they applied
 * Input
    - record: &Value - evidence record
 * Output
    - Vec<String> such as "payments/preserve"
*/
pub(super) fn policy_labels(record: &Value) -> Vec<String> {
    let rule = rule(&strings(&record["reasons"]));
    strings(&record["policies"])
        .into_iter()
        .map(|policy| rule.map_or(policy.clone(), |rule| format!("{policy}/{rule}")))
        .collect()
}

/** Describe what a record touched: files, or the programs of a command, or its tool
 * Input
    - record: &Value - evidence record
 * Output
    - Vec<String>
*/
pub(super) fn resources(record: &Value) -> Vec<String> {
    let files = strings(&record["resources"])
        .into_iter()
        .filter_map(|resource| resource.strip_prefix("file:").map(String::from))
        .collect::<Vec<_>>();
    if !files.is_empty() {
        return files;
    }
    let read = strings(&record["action_summary"]["read"]);
    if !read.is_empty() {
        return read;
    }
    let programs = strings(&record["action_summary"]["programs"]);
    if !programs.is_empty() {
        return vec![format!("command: {}", programs.join(" | "))];
    }
    strings(&record["resources"])
}

/** Describe what a decision touched (see resources)
 * Input
    - act: &Act - the decision
 * Output
    - Vec<String>
*/
pub(super) fn act_resources(act: &Act) -> Vec<String> {
    let files = act
        .resources
        .iter()
        .filter_map(|resource| resource.strip_prefix("file:").map(String::from))
        .collect::<Vec<_>>();
    if !files.is_empty() {
        return files;
    }
    if !act.read.is_empty() {
        return act.read.iter().map(|item| item.to_string()).collect();
    }
    if !act.programs.is_empty() {
        return vec![format!("command: {}", act.programs.join(" | "))];
    }
    act.resources.iter().map(|item| item.to_string()).collect()
}

/** Build one timeline event
 * Input
    - session: &Session - its session
    - at: &Value - timestamp
    - seq: &Value - journal sequence number (null for task events)
    - kind: &str - event type
    - fields: Value - the other fields
 * Output
    - Value
*/
fn event(session: Option<&Session>, at: &Value, seq: &Value, kind: &str, fields: Value) -> Value {
    let mut value = json!({
        "at": at,
        "seq": seq,
        "type": kind,
        "session_id": session.map(|session| session.run.id.clone()),
        "agent": session.map(|session| actor(&session.run.agent())),
        "tool": null,
        "resource": [],
        "decision": null,
        "policy": [],
        "zone": [],
        "risk": "low",
        "result": null,
        "duration": null,
        "detail": null,
        "link": session.map(|session| format!("#/runs/{}?seq={}", session.run.id, seq)),
    });
    if let (Some(object), Some(fields)) = (value.as_object_mut(), fields.as_object()) {
        for (key, field) in fields {
            object.insert(key.clone(), field.clone());
        }
    }
    value
}

/** Build the execution timeline of one session: decisions with their tool, resources, policies,
 * zones, risk, result, and how long the tool ran; budget changes; autonomy and safety changes;
 * verification; lifecycle and delivery events
 * Input
    - session: &Session - session
    - run: &Full - the session read in full
 * Output
    - Vec<Value>
*/
fn session_timeline(session: &Session, run: &Full) -> Vec<Value> {
    let mut executed: BTreeMap<String, Vec<(u64, &Value)>> = BTreeMap::new();
    for item in &run.events {
        if item["event"] == "post_tool_use" {
            if let Some(digest) = item["arguments_digest"].as_str() {
                executed
                    .entry(digest.to_string())
                    .or_default()
                    .push((item["at"].as_u64().unwrap_or(0), item));
            }
        }
    }
    let mut out = Vec::new();
    let mut previous: Option<(Value, Value)> = None;
    for (record, item) in run.records.iter().zip(&run.events) {
        let at = &record["at"];
        let seq = &record["seq"];
        let name = record["event"].as_str().unwrap_or_default();
        let violated = record["violations"]
            .as_array()
            .is_some_and(|violations| !violations.is_empty());
        match (record["category"].as_str(), name) {
            (Some("authorization"), _) => {
                let decision = match record["decision"].as_str() {
                    Some("allow") => "ALLOW",
                    Some("deny") => "DENY",
                    Some("approval_required") => "ASK",
                    _ => "UNKNOWN",
                };
                let after = record["action_digest"]
                    .as_str()
                    .and_then(|digest| executed.get(digest))
                    .and_then(|runs| runs.iter().find(|(when, _)| *when >= at.as_u64().unwrap_or(0)));
                out.push(event(
                    Some(session),
                    at,
                    seq,
                    &record["operation"].as_str().unwrap_or("other").to_uppercase(),
                    json!({
                        "tool": record["tool"],
                        "resource": resources(record),
                        "decision": decision,
                        "policy": policy_labels(record),
                        "zone": record["zones"].as_array().cloned().unwrap_or_default(),
                        "risk": match decision { "DENY" => "high", "ASK" => "medium", _ => "low" },
                        "result": item["result"].as_str().unwrap_or(match decision { "ALLOW" => "authorized", "DENY" => "blocked", _ => "approval_required" }),
                        "duration": after.map(|(when, _)| when.saturating_sub(at.as_u64().unwrap_or(0))),
                        "detail": strings(&record["reasons"]).join(" "),
                    }),
                ));
            }
            (_, "post_tool_use") => {
                let changed = ["added", "modified", "deleted"]
                    .iter()
                    .flat_map(|key| strings(&item["effect"]["files"][*key]))
                    .collect::<Vec<_>>();
                let verdict = item["verification"].as_str().map(str::to_uppercase);
                out.push(event(
                    Some(session),
                    at,
                    seq,
                    "EXECUTED",
                    json!({
                        "tool": record["tool"],
                        "resource": changed,
                        "risk": if violated { "high" } else { "low" },
                        "result": verdict.clone().unwrap_or_else(|| "executed".into()),
                        "detail": if violated { format!("{} violation(s) found after the tool ran", record["violations"].as_array().map_or(0, Vec::len)) } else { String::new() },
                    }),
                ));
                if let Some(verdict) = verdict {
                    out.push(event(Some(session), at, seq, "VERIFICATION", json!({"result": verdict, "risk": if verdict == "FAIL" { "high" } else { "low" }, "detail": "Contract verification"})));
                }
            }
            (_, "stop" | "session_finalized") => out.push(event(
                Some(session),
                at,
                seq,
                "VERIFICATION",
                json!({
                    "result": item["final_status"],
                    "risk": if item["final_status"] == "FAIL" { "high" } else { "low" },
                    "detail": if name == "stop" { "Verification when the agent stopped".to_string() } else { format!("Final reconciliation{}", if strings(&item["findings"]).is_empty() { String::new() } else { format!(": {}", strings(&item["findings"]).join("; ")) }) },
                }),
            )),
            (_, "session_start") => out.push(event(Some(session), at, seq, "SESSION", json!({"result": "started", "detail": item["model"].as_str().map(|model| format!("model {model}"))}))),
            (_, "session_resumed") => out.push(event(Some(session), at, seq, "SESSION", json!({"result": "resumed", "detail": "Agent continued after a human resumed the session"}))),
            (_, "session_end" | "session_cancelled" | "session_timed_out" | "authority_lapsed") => out.push(event(Some(session), at, seq, "SESSION", json!({"result": name.trim_start_matches("session_").replace('_', " "), "detail": item["reason"]}))),
            (Some("delivery"), _) => out.push(event(
                Some(session),
                at,
                seq,
                "DELIVERY",
                json!({
                    "result": name.trim_start_matches("delivery_"),
                    "resource": item["pull_request"]["number"].as_u64().or(item["pull_request"].as_u64()).map(|number| vec![format!("PR #{number}")]).unwrap_or_default(),
                    "detail": item["by"].as_str().map(|by| format!("by {by}")).or_else(|| item["merge_sha"].as_str().map(|sha| format!("merge {sha}"))),
                }),
            )),
            _ => {}
        }
        let state = (record["autonomy"].clone(), record["safety"].clone());
        if previous.as_ref().is_some_and(|previous| *previous != state)
            || (name == "autonomy" && violated)
        {
            let from = previous.clone().unwrap_or_default();
            out.push(event(
                Some(session),
                at,
                seq,
                "AUTONOMY",
                json!({
                    "result": format!("{} / {}", state.0.as_str().unwrap_or_default(), state.1.as_str().unwrap_or_default()),
                    "risk": match state.1.as_str() { Some("quarantined") => "critical", Some("degraded") => "high", _ => "low" },
                    "detail": format!(
                        "{} → {}{}",
                        format!("{} / {}", from.0.as_str().unwrap_or("-"), from.1.as_str().unwrap_or("-")),
                        format!("{} / {}", state.0.as_str().unwrap_or_default(), state.1.as_str().unwrap_or_default()),
                        if item["trigger"].is_string() { format!(" ({}{})", item["trigger"].as_str().unwrap_or_default(), item["kind"].as_str().map_or(String::new(), |kind| format!(": {kind}"))) } else { String::new() }
                    ),
                }),
            ));
        }
        previous = Some(state);
        let before = record["budget"]["before"]["current"].as_i64().unwrap_or(0);
        let after = record["budget"]["after"]["current"].as_i64().unwrap_or(0);
        if before != after {
            out.push(event(
                Some(session),
                at,
                seq,
                "BUDGET",
                json!({
                    "result": format!("{:+}", after - before),
                    "detail": format!("Budget {} {} (now {} of {})", if after < before { "reduced" } else { "restored" }, (after - before).abs(), after, record["budget"]["after"]["max"]),
                }),
            ));
        }
    }
    out
}

/** List one session's violations in detail (derived once, when the session is read)
 * Input
    - session: &Session - session
 * Output
    - Vec<Value>
*/
pub(super) fn session_violations(session: &Session) -> Vec<Value> {
    session.run.violation_details.clone()
}

/** Derive one session's violations in detail from its full journal: when, the policy, zone,
 * resource, action, and decision of the action that caused it, its severity, its consequence,
 * the autonomy state it left, and whether it was resolved (a later verified repair, or a human
 * resuming the session)
 * Input
    - run: &Full - the session read in full
 * Output
    - Vec<Value>
*/
pub(super) fn violation_details(run: &Full) -> Vec<Value> {
    let records = &run.records;
    let mut out = Vec::new();
    for (index, (record, item)) in records.iter().zip(&run.events).enumerate() {
        for violation in record["violations"].as_array().into_iter().flatten() {
            let kind = violation
                .as_str()
                .map(String::from)
                .or_else(|| violation["violation_type"].as_str().map(String::from))
                .unwrap_or_else(|| "unknown".into());
            let name = record["event"].as_str().unwrap_or_default();
            let severity = run.severity(&kind, name);
            // The action that caused it: the decision for this tool call, or the last one before
            let cause = records[..=index]
                .iter()
                .rev()
                .find(|candidate| {
                    candidate["category"] == "authorization"
                        && (record["action_digest"].is_null()
                            || candidate["action_digest"] == record["action_digest"])
                })
                .unwrap_or(record);
            let before = index
                .checked_sub(1)
                .map(|previous| records[previous]["safety"].clone())
                .unwrap_or(json!("active"));
            let consequence = match (before.as_str(), record["safety"].as_str()) {
                (Some(from), Some("quarantined")) if from != "quarantined" => {
                    "session quarantined: every change is denied until a human resumes it"
                }
                (Some("active"), Some("degraded")) => {
                    "session degraded: changes need human approval"
                }
                _ if name == "post_tool_use" => {
                    "verification failed: the agent must repair the change"
                }
                _ if name == "session_finalized" => "the final decision lists it as a finding",
                _ => "recorded",
            };
            let resolution = records[index + 1..]
                .iter()
                .find(|later| !later["repair"].is_null() || later["human"]["action"] == "resume");
            let mut policy = policy_labels(cause);
            if let Some(id) = violation["policy_id"].as_str().filter(|id| *id != "crane") {
                policy = vec![format!(
                    "{id}/{}",
                    violation["rule"].as_str().unwrap_or_default()
                )];
            }
            out.push(json!({
                "event_id": format!("{}:{}", run.id, record["seq"]),
                "session_id": run.id,
                "seq": record["seq"],
                "at": record["at"],
                "kind": kind,
                "message": violation["message"].as_str().or_else(|| item["reason"].as_str()),
                "policy": policy,
                "zone": cause["zones"].as_array().cloned().unwrap_or_default(),
                "resource": if violation["target"].is_string() { vec![violation["target"].as_str().unwrap_or_default().to_string()] } else { resources(cause) },
                "action": format!("{} {}", cause["tool"].as_str().unwrap_or(name), cause["operation"].as_str().unwrap_or_default()).trim().to_string(),
                "decision": cause["decision"],
                "severity": severity,
                "consequence": consequence,
                "resulting_state": {"autonomy": record["autonomy"], "safety": record["safety"]},
                "previous_state": index.checked_sub(1).map(|previous| json!({"autonomy": records[previous]["autonomy"], "safety": records[previous]["safety"]})),
                "budget": {
                    "before": record["budget"]["before"]["current"],
                    "after": record["budget"]["after"]["current"],
                    "change": record["budget"]["after"]["current"].as_i64().unwrap_or(0) - record["budget"]["before"]["current"].as_i64().unwrap_or(0),
                },
                "resolved": resolution.is_some(),
                "resolved_by": resolution.map(|later| json!({"seq": later["seq"], "at": later["at"], "how": if later["human"]["action"] == "resume" { "a human resumed the session".to_string() } else { later["repair"]["kind"].as_str().unwrap_or("repair").replace('_', " ") }})),
                "link": format!("#/runs/{}?seq={}", run.id, record["seq"]),
            }));
        }
    }
    out
}

/** Name a delivery's pull request status from its replayed state
 * Input
    - state: &Value - delivery state (delivery::state)
 * Output
    - &'static str merged, rejected, changes requested, approved, open, or submitted
*/
pub(super) fn pr_status(state: &Value) -> &'static str {
    let blocked = |kind: &str| {
        state["blocks"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|block| block["kind"] == kind))
    };
    if !state["merged"].is_null() {
        "merged"
    } else if blocked("rejection") {
        "rejected"
    } else if blocked("changes_requested") {
        "changes requested"
    } else if state["approvals"]
        .as_array()
        .is_some_and(|approvals| !approvals.is_empty())
    {
        "approved"
    } else if !state["pull_request"].is_null() {
        "open"
    } else {
        "submitted"
    }
}

/** Answer the Task Details page
 * Input
    - id: &str - task id (validated)
    - query: &Query - limit (timeline events)
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn task(id: &str, query: &Query) -> Result<Value, (u16, String)> {
    let limit = match query.get("limit") {
        None => EVENTS.0,
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=EVENTS.1).contains(limit))
            .ok_or_else(|| {
                (
                    400,
                    format!(
                        "limit must be a number from 1 to {}, not '{text}'",
                        EVENTS.1
                    ),
                )
            })?,
    };
    let now = now_unix();
    let failed = |error: String| (500, error);
    let runs = Run::all().map_err(failed)?;
    let data = Dataset::new(&runs).map_err(failed)?;
    let task = data
        .tasks
        .iter()
        .find(|task| task.id == id)
        .ok_or_else(|| (404, format!("no task '{id}'")))?;
    let keys = Keys::new(task, &data, now);
    let sessions = &keys.sessions;
    // The task's sessions read in full: its timeline, attestation, and evidence references
    let mut fulls = BTreeMap::new();
    for session in sessions {
        if let Some(full) = Run::full(&session.run.id).map_err(failed)? {
            fulls.insert(session.run.id.clone(), full);
        }
    }
    let empty = Full::default();
    let full = |session: &Session| fulls.get(&session.run.id).unwrap_or(&empty);
    let contract = super::contract(id);
    let record = &task.record;
    let branch = sessions
        .iter()
        .filter_map(|session| session.run.delivery["branch"].as_str())
        .next_back()
        .map(String::from);
    let row = keys.row(&data.repository);

    // 1. Task
    let description = contract
        .as_ref()
        .map(|contract| contract["task"].clone())
        .unwrap_or(Value::Null);
    let history = record["history"].as_array().cloned().unwrap_or_default();
    let first_session = sessions.iter().map(|session| session.started).min();
    let task_section = json!({
        "task_id": id,
        "source": record["source"],
        "external_id": record["external_id"],
        "title": keys.title(),
        "description": description["description"],
        "acceptance_criteria": description["acceptance_criteria"],
        "requester": description["requester"],
        "priority": description["priority"],
        "labels": description["labels"],
        "repository": data.repository,
        "branch": branch,
        "state": keys.status,
        "final": keys.terminal,
        "lifecycle": history,
        "created_at": task.received,
        "started_at": first_session,
        "completed_at": task.completed.or(task.failed).or(task.cancelled),
        "tracker_completed_at": task.confirmed,
    });

    // 2. Contract (as stored; nothing here can change it)
    let contract_section = contract.as_ref().map(|contract| {
        let bindings = &contract["bindings"];
        json!({
            "contract_id": contract["contract_id"],
            "status": contract["status"],
            "digest": contract["digest"],
            "version": contract["version"],
            "checkpoint": bindings["checkpoint"],
            "policy_version": bindings["policy_version"],
            "zone_set_version": bindings["zone_set_version"],
            "autonomy_policy_version": bindings["autonomy"]["policy_version"],
            "max_autonomy": bindings["autonomy"]["max_autonomy"],
            "budget_model_version": bindings["budget"]["model_version"],
            "approval": contract["approval"],
            "invalidation": contract["invalidation"],
            "must_change": contract["contract"]["MUST_CHANGE"],
            "must_not_change": contract["contract"]["MUST_NOT_CHANGE"],
            "may_change": contract["contract"]["MAY_CHANGE"],
            "requires_approval": contract["contract"]["REQUIRES_APPROVAL"],
            "task_scope": contract["contract"]["TASK_SCOPE"],
            "expected_tests": contract["contract"]["EXPECTED_TESTS"],
            "agentscript": contract["agentscript"],
            "policy_name": contract["policy_name"],
        })
    });
    let bound_contracts = sessions
        .iter()
        .map(|session| json!({"session_id": session.run.id, "contract": session.summary["evidence"], "policies": full(session).attestation["contract"]["policies"], "checkpoints": full(session).attestation["checkpoints"], "policy_versions": full(session).attestation["policy_versions"]}))
        .collect::<Vec<_>>();

    // 3. Agents
    let agents = sessions
        .iter()
        .map(|session| {
            let version = session.run.adapter_version();
            json!({
                "session_id": session.run.id,
                "agent_id": session.run.document["provider_session"],
                "agent": session.run.agent(),
                "name": actor(&session.run.agent()),
                "provider": session.run.provider(),
                "models": session.summary["models"],
                "adapter": format!("{} adapter", session.run.agent()),
                "adapter_version": version,
                "lifecycle": session.summary["lifecycle"],
                "autonomy": session.summary["autonomy"]["current"],
                "safety": session.summary["safety"]["state"],
                "started_at": session.started,
                "last_activity_at": session.last,
            })
        })
        .collect::<Vec<_>>();

    // 4. Autonomy
    let ordered = {
        let mut ordered = sessions.clone();
        ordered.sort_by_key(|session| session.started);
        ordered
    };
    let budget_start = ordered
        .first()
        .and_then(|session| full(session).records.first())
        .map(|record| record["budget"]["before"].clone());
    let budget_end = ordered
        .last()
        .and_then(|session| full(session).records.last())
        .map(|record| record["budget"]["after"].clone());
    let mut transitions = Vec::new();
    let mut degradations = Vec::new();
    let mut quarantines = Vec::new();
    for session in &ordered {
        let mut previous: Option<(Value, Value)> = None;
        for (record, item) in full(session).records.iter().zip(&full(session).events) {
            let state = (record["autonomy"].clone(), record["safety"].clone());
            if let Some(from) = previous.as_ref().filter(|from| **from != state) {
                let change = json!({
                    "session_id": session.run.id,
                    "seq": record["seq"],
                    "at": record["at"],
                    "from": {"autonomy": from.0, "safety": from.1},
                    "to": {"autonomy": state.0, "safety": state.1},
                    "trigger": item["trigger"],
                    "kind": item["kind"],
                    "actor": item["actor"],
                    "reason": item["reason"].as_str().map(String::from).or_else(|| item["kind"].as_str().map(|kind| format!("violation: {kind}"))),
                    "link": format!("#/runs/{}?seq={}", session.run.id, record["seq"]),
                });
                if state.1 == "quarantined" && from.1 != "quarantined" {
                    quarantines.push(change.clone());
                } else if state.1 == "degraded" && from.1 == "active" {
                    degradations.push(change.clone());
                }
                transitions.push(change);
            }
            previous = Some(state);
        }
    }
    let autonomy = json!({
        "initial": ordered.first().map(|session| session.summary["autonomy"]["initial"].clone()),
        "final": ordered.last().map(|session| session.summary["autonomy"]["current"].clone()),
        "final_safety": ordered.last().map(|session| session.summary["safety"]["state"].clone()),
        "score": row["autonomy_score"],
        "budget": {
            "start": budget_start,
            "consumed": keys.budget,
            "remaining": budget_end.as_ref().map(|budget| budget["available"].clone()),
            "end": budget_end,
        },
        "transitions": transitions,
        "degradations": degradations,
        "quarantines": quarantines,
    });

    // 5. Execution timeline, chronological, with the task's own lifecycle
    let mut timeline = ordered
        .iter()
        .flat_map(|session| session_timeline(session, full(session)))
        .collect::<Vec<_>>();
    for entry in &history {
        timeline.push(event(
            None,
            &entry["at"],
            &Value::Null,
            "TASK",
            json!({"result": entry["to"], "detail": entry["reason"], "link": format!("#/tasks/{id}")}),
        ));
    }
    timeline.sort_by_key(|item| {
        (
            item["at"].as_u64().unwrap_or(0),
            item["seq"].as_u64().unwrap_or(0),
        )
    });
    let events_total = timeline.len();
    timeline.truncate(limit);

    // 6. Violations
    let violations = ordered
        .iter()
        .flat_map(|session| session_violations(session))
        .collect::<Vec<_>>();
    let count = |key: &str, value: &str| {
        violations
            .iter()
            .filter(|violation| violation[key] == value)
            .count()
    };
    let violation_section = json!({
        "total": violations.len(),
        "low": count("severity", "low"),
        "medium": count("severity", "medium"),
        "high": count("severity", "high"),
        "critical": count("severity", "critical"),
        "resolved": violations.iter().filter(|violation| violation["resolved"] == true).count(),
        "unresolved": violations.iter().filter(|violation| violation["resolved"] == false).count(),
        "items": violations,
    });

    // 7. Verification, per session
    let verification = ordered
        .iter()
        .map(|session| {
            let checks = session.run.delivery["checks"].as_array().cloned().unwrap_or_default();
            let pick = |words: &[&str]| {
                checks
                    .iter()
                    .filter(|check| {
                        let text = format!("{} {}", check["kind"].as_str().unwrap_or_default(), check["name"].as_str().unwrap_or_default()).to_lowercase();
                        words.iter().any(|word| text.contains(word))
                    })
                    .map(|check| json!({"name": check["name"], "status": check["status"]}))
                    .collect::<Vec<_>>()
            };
            let finalized = full(session).events.iter().rev().find(|item| item["event"] == "session_finalized");
            let attestation = &full(session).attestation;
            let stored = &session.summary["evidence"]["stored_attestation_digest"];
            json!({
                "session_id": session.run.id,
                "contract_tests": session.summary["verification"]["contract_tests"],
                "contract_tests_passed": session.summary["verification"]["contract_tests_passed"],
                "delivered_contract_tests": session.run.delivery["contract_tests"],
                "repository_tests": session.summary["verification"]["repository_tests"],
                "repository_tests_passed": session.summary["verification"]["repository_tests_passed"],
                "lint": pick(&["lint", "format", "clippy"]),
                "security": pick(&["security", "audit", "secret", "sast", "vuln"]),
                "checks": checks.iter().map(|check| json!({"name": check["name"], "kind": check["kind"], "status": check["status"]})).collect::<Vec<_>>(),
                "last_verification": session.summary["verification"]["last"],
                "final_reconciliation": finalized.map(|item| json!({"at": item["at"], "final_status": item["final_status"], "findings": item["findings"]})),
                "journal_chain": attestation["evidence"]["chain"],
                "attestation": {
                    "digest": attestation["attestation_digest"],
                    "stored_digest": stored,
                    "matches_stored": if stored.is_null() { Value::Null } else { json!(*stored == attestation["attestation_digest"]) },
                    "final_decision": attestation["final_decision"],
                },
            })
        })
        .collect::<Vec<_>>();

    // 8. Delivery, per delivered session
    let delivery = ordered
        .iter()
        .filter(|session| !session.run.delivery.is_null())
        .map(|session| {
            let state = &session.run.delivery;
            let blocks = state["blocks"].as_array().cloned().unwrap_or_default();
            let approvals = state["approvals"].as_array().cloned().unwrap_or_default();
            let status = pr_status(state);
            let person = |entry: &Value| json!({"by": entry["by"], "at": entry["at"], "kind": entry["kind"], "reason": entry["reason"], "via": entry["via"], "head": entry["head"]});
            json!({
                "session_id": session.run.id,
                "branch": state["branch"],
                "base": state["base"],
                "commit": state["head"],
                "files": state["files"],
                "pull_request": if state["pull_request"].is_null() { Value::Null } else { json!({"number": state["pull_request"]["number"], "url": state["pull_request"]["url"], "title": state["pull_request"]["title"], "provider": state["pull_request"]["provider"]}) },
                "pr_status": status,
                "reviews": blocks.iter().map(person).collect::<Vec<_>>(),
                "approvals": approvals.iter().map(person).collect::<Vec<_>>(),
                "exceptions": state["exceptions"].as_array().into_iter().flatten().map(|entry| json!({"by": entry["by"], "check": entry["check"], "reason": entry["reason"], "expires_at": entry["expires_at"]})).collect::<Vec<_>>(),
                "merge_sha": state["merged"]["sha"],
                "merged_at": state["merged"]["at"],
                "merged_by": state["merged"]["by"],
                "trusted_checkpoint": state["merged"]["trusted_checkpoint"],
                "merge_failures": state["merge_failures"].as_array().map_or(0, Vec::len),
                "task_completion": state["task_completion"]["status"],
                "chain": state["chain"]["status"],
            })
        })
        .collect::<Vec<_>>();
    let completion = json!({"state": task.completion, "confirmed_at": task.confirmed});

    // 9. Evidence references
    let mut references = Vec::new();
    if let Some(contract) = &contract {
        references.push(json!({"kind": "contract digest", "value": contract["digest"], "where": format!(".crane/task-contracts/{id}/v{}.json", contract["version"]), "inspect": format!("crane task contract show {id}")}));
    }
    for session in &ordered {
        let session_id = &session.run.id;
        references.push(json!({"kind": "session journal", "value": full(session).attestation["evidence"]["chain"]["head"], "detail": format!("{} events, chain {}", full(session).events.len(), full(session).attestation["evidence"]["chain"]["status"].as_str().unwrap_or("unknown")), "where": format!(".crane/runtime/sessions/{session_id}/journal.jsonl"), "inspect": format!("crane session inspect {session_id}"), "link": format!("#/runs/{session_id}")}));
        references.push(json!({"kind": "attestation", "value": full(session).attestation["attestation_digest"], "where": format!(".crane/runtime/sessions/{session_id}/attestation.json"), "inspect": format!("crane session export {session_id} --json")}));
        let state = &session.run.delivery;
        if let Some(head) = state["head"].as_str() {
            references.push(json!({"kind": "commit", "value": head, "detail": state["branch"]}));
        }
        if let Some(url) = state["pull_request"]["url"].as_str() {
            references.push(json!({"kind": "pull request", "value": format!("#{}", state["pull_request"]["number"]), "url": url}));
        }
        if let Some(sha) = state["merged"]["sha"].as_str() {
            references.push(json!({"kind": "merge commit", "value": sha, "detail": state["merged"]["trusted_checkpoint"]}));
        }
        if !state.is_null() {
            references.push(json!({"kind": "delivery journal", "value": state["chain"]["head"], "where": format!(".crane/runtime/delivery/{session_id}/journal.jsonl"), "inspect": format!("crane deliver status {session_id}")}));
        }
    }
    let notable = timeline
        .iter()
        .filter(|item| matches!(item["decision"].as_str(), Some("DENY" | "ASK")) || matches!(item["risk"].as_str(), Some("high" | "critical")))
        .map(|item| json!({"at": item["at"], "type": item["type"], "decision": item["decision"], "result": item["result"], "resource": item["resource"], "link": item["link"]}))
        .collect::<Vec<_>>();

    let header = json!({
        "task_id": id,
        "title": row["title"],
        "repository": data.repository,
        "branch": branch,
        "agents": row["agents"],
        "providers": row["providers"],
        "status": keys.status,
        "final": keys.terminal,
        "start": task.received,
        "duration": keys.end - keys.start,
        "autonomy": row["autonomy"],
        "autonomy_score": row["autonomy_score"],
        "risk": {"score": keys.risk, "level": risk_level(keys.risk, keys.critical)},
    });
    Ok(json!({
        "header": header,
        "task": task_section,
        "contract": contract_section,
        "bound_contracts": bound_contracts,
        "agents": {"session_count": sessions.len(), "sessions": agents},
        "autonomy": autonomy,
        "timeline": {"total": events_total, "truncated": events_total > limit, "events": timeline},
        "violations": violation_section,
        "verification": verification,
        "delivery": {"deliveries": delivery, "completion": completion},
        "evidence": {"references": references, "notable_events": notable},
        "rows": {"summary": row},
        "risk_definition": "10 × critical violations + 3 × other violations + 5 if verification or tests failed + 5 × quarantined sessions + 1 × denied actions; level: low < 5, medium < 15, high < 30, critical at 30 or with any critical violation",
        "severity_definition": "critical: the session's autonomy policy treats the kind as critical (it quarantines); high: other violations the autonomy state machine acted on (they degrade); medium: violations found when verifying an executed tool call; low: findings of the final reconciliation",
    }))
}
