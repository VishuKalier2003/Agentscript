// Runs: one governed execution instance each (a contract session). The Runs page lists them with
// server-side filtering and pagination; a run's page shows its timeline (start, contract and
// checkpoint binding, agent connection, decisions, violations, budget, autonomy, verification,
// attestation, delivery, approval, merge, task completion) and an inspection of every action. Tool
// arguments are never shown: the journal keeps only each call's digest and a normalized summary,
// and that is all that is displayed. Everything is a read-only projection.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use super::dashboard::{actor, Dataset, Session, Window};
use super::detail::{policy_labels, pr_status, resources, session_violations, strings};
use super::{slug, Full, Query, Run};
use crate::util::now_unix;

/** Parameters the Runs page accepts */
pub(crate) const PARAMETERS: &[&str] = &[
    "range",
    "from",
    "to",
    "repository",
    "task",
    "agent",
    "provider",
    "session",
    "policy",
    "policy_version",
    "zone",
    "autonomy",
    "safety",
    "task_status",
    "session_status",
    "severity",
    "verification",
    "delivery",
    "environment",
    "branch",
    "team",
    "q",
    "sort",
    "page",
    "page_size",
];

/** Sort orders of the Runs page */
const SORTS: &[&str] = &[
    "newest",
    "oldest",
    "duration",
    "actions",
    "violations",
    "budget",
];

/** Read a page number or size
 * Input
    - query: &Query - parameters
    - key: &str - name
    - default: usize - value when absent
    - max: usize - largest value
 * Output
    - Result<usize, (u16, String)>
*/
fn number(query: &Query, key: &str, default: usize, max: usize) -> Result<usize, (u16, String)> {
    match query.get(key) {
        None => Ok(default),
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|value| (1..=max).contains(value))
            .ok_or_else(|| {
                (
                    400,
                    format!("{key} must be a number from 1 to {max}, not '{text}'"),
                )
            }),
    }
}

/** The id of a repository in links: its name with every other character turned into "-"
 * Input
    - name: &str - repository name ("acme/shop")
 * Output
    - String ("acme-shop")
*/
pub(crate) fn repository_id(name: &str) -> String {
    slug(&name.replace('/', "-"))
}

/** Build one row of the Runs page
 * Input
    - session: &Session - the run
    - data: &Dataset - every session and task
 * Output
    - Value
*/
pub(super) fn run_row(session: &Session, data: &Dataset) -> Value {
    let summary = &session.summary;
    let delivery = &session.run.delivery;
    let tasks = session.attributes.get("task").cloned().unwrap_or_default();
    json!({
        "run_id": session.run.id,
        "task_id": tasks.iter().next(),
        "tasks": tasks,
        "repository": data.repository,
        "repository_id": repository_id(&data.repository),
        "agent": actor(&session.run.agent()),
        "agent_id": session.run.agent_id(),
        "provider": session.run.provider(),
        "status": summary["lifecycle"],
        "phase": summary["phase"],
        "autonomy": summary["autonomy"]["current"],
        "safety": summary["safety"]["state"],
        "start": session.started,
        "duration": session.ended.unwrap_or(if session.active() { now_unix() } else { session.last }).saturating_sub(session.started),
        "actions": summary["actions"]["total"],
        "allowed": summary["actions"]["allow"],
        "denied": summary["actions"]["deny"],
        "approvals": summary["actions"]["approval_required"],
        "violations": summary["violations"]["total"],
        "critical_violations": summary["violations"]["critical"],
        "budget_consumed": summary["budget"]["consumed"],
        "verification": session.verification,
        "pull_request": if delivery["pull_request"].is_null() { Value::Null } else { json!({"number": delivery["pull_request"]["number"], "url": delivery["pull_request"]["url"]}) },
        "merge": if delivery.is_null() { "none" } else { pr_status(delivery) },
        "final_status": summary["verification"]["final_decision"],
        "link": format!("#/runs/{}", session.run.id),
    })
}

/** Answer the Runs page: one row per run matching the window, filters, and search, sorted and
 * paginated on the server
 * Input
    - query: &Query - window, filters, q, sort, page, page_size
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn list(query: &Query) -> Result<Value, (u16, String)> {
    let now = now_unix();
    let window = Window::parse(query, now).map_err(|error| (400, error))?;
    query.choice("sort", SORTS).map_err(|error| (400, error))?;
    let page_size = number(query, "page_size", 50, 200)?;
    let page = number(query, "page", 1, 1_000_000)?;
    let sort = query.get("sort").unwrap_or("newest");
    let failed = |error: String| (500, error);
    let runs = Run::all().map_err(failed)?;
    let data = Dataset::new(&runs).map_err(failed)?;
    let (matching, _) = data.select(query);
    let search = query.get("q").map(str::to_lowercase);
    let mut chosen = data
        .sessions
        .iter()
        .enumerate()
        .filter(|(index, session)| matching.contains(index) && session.during(&window))
        .map(|(_, session)| session)
        .filter(|session| {
            search.as_ref().is_none_or(|search| {
                let haystack = format!(
                    "{} {} {} {} {}",
                    session.run.id,
                    session
                        .attributes
                        .get("task")
                        .into_iter()
                        .flatten()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" "),
                    session.run.agent_id(),
                    session.run.document["provider_session"]
                        .as_str()
                        .unwrap_or_default(),
                    session.run.delivery["head"].as_str().unwrap_or_default()
                )
                .to_lowercase();
                search
                    .split_whitespace()
                    .all(|word| haystack.contains(word))
            })
        })
        .collect::<Vec<_>>();
    let count = |session: &Session, path: &[&str]| {
        path.iter()
            .fold(session.summary, |value, key| &value[*key])
            .as_f64()
            .unwrap_or(0.0)
    };
    chosen.sort_by(|left, right| {
        let order = match sort {
            "oldest" => left.started.cmp(&right.started),
            "duration" => (right.last - right.started).cmp(&(left.last - left.started)),
            "actions" => {
                count(right, &["actions", "total"]).total_cmp(&count(left, &["actions", "total"]))
            }
            "violations" => count(right, &["violations", "total"])
                .total_cmp(&count(left, &["violations", "total"])),
            "budget" => count(right, &["budget", "consumed"])
                .total_cmp(&count(left, &["budget", "consumed"])),
            _ => right.started.cmp(&left.started),
        };
        order
            .then_with(|| right.started.cmp(&left.started))
            .then_with(|| left.run.id.cmp(&right.run.id))
    });
    let total = chosen.len();
    let items = chosen
        .iter()
        .skip((page - 1).saturating_mul(page_size))
        .take(page_size)
        .map(|session| run_row(session, &data))
        .collect::<Vec<_>>();
    let mut facets: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for session in &data.sessions {
        for (key, values) in &session.attributes {
            facets
                .entry(key)
                .or_default()
                .extend(values.iter().cloned());
        }
    }
    facets
        .entry("repository")
        .or_default()
        .insert(data.repository.clone());
    Ok(json!({
        "now": now,
        "window": {"range": window.name, "from": window.from, "to": window.to},
        "total": total,
        "page": page,
        "pages": total.div_ceil(page_size).max(1),
        "page_size": page_size,
        "sort": sort,
        "sorts": SORTS,
        "items": items,
        "facets": super::dashboard::bounded(facets).0,
    }))
}

/** Describe what an action was, without its arguments: the journal's normalized summary
 * Input
    - record: &Value - evidence record of an authorization
 * Output
    - String such as "edit 1 file", "run git | grep (4 arguments)", or "read payments/service.py"
*/
fn normalized(record: &Value) -> String {
    let summary = &record["action_summary"];
    let programs = strings(&summary["programs"]);
    if !programs.is_empty() {
        return format!(
            "run {} ({} argument{})",
            programs.join(" | "),
            summary["arguments"],
            if summary["arguments"] == 1 { "" } else { "s" }
        );
    }
    let read = strings(&summary["read"]);
    if !read.is_empty() {
        return format!("read {}", read.join(", "));
    }
    let changes = strings(&summary["changes"]);
    if !changes.is_empty() {
        return format!(
            "{} {} file{}",
            changes.join(", "),
            summary["files"],
            if summary["files"] == 1 { "" } else { "s" }
        );
    }
    format!("{} call", record["operation"].as_str().unwrap_or("tool"))
}

/** Build a run's timeline and action inspection
 * Input
    - session: &Session - the run
    - full: &Full - the run read in full from its journal
    - completions: &[Value] - completion events
 * Output
    - (Vec<Value>, Vec<Value>) timeline events, and one inspection per action
*/
fn timeline(session: &Session, full: &Full, completions: &[Value]) -> (Vec<Value>, Vec<Value>) {
    let run = session.run;
    let document = &full.document;
    let link = |seq: &Value| format!("#/runs/{}?seq={seq}", run.id);
    let mut events = Vec::new();
    let mut push = |at: &Value,
                    kind: &str,
                    title: String,
                    outcome: Value,
                    detail: Value,
                    seq: &Value| {
        // A journal event is named by its session and sequence number, which never change; an
        // event from another record by what it is and when
        let event_id = if seq.is_null() {
            format!("{}:{kind}@{}", run.id, at.as_u64().unwrap_or(0))
        } else {
            format!("{}:{seq}", run.id)
        };
        events.push(json!({"event_id": event_id, "at": at, "kind": kind, "title": title, "outcome": outcome, "detail": detail, "seq": seq, "link": if seq.is_null() { Value::Null } else { json!(link(seq)) }}));
    };
    let created = &document["created_at"];
    push(
        created,
        "session",
        "Session started".into(),
        json!("started"),
        json!(format!("{} on {}", actor(&run.agent()), run.model())),
        &Value::Null,
    );
    let contracts = document["contracts"]["contracts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    push(
        created,
        "contract",
        "Contract bound".into(),
        json!(document["contracts"]["version"]),
        json!(contracts
            .iter()
            .map(|contract| format!(
                "{} ({})",
                contract["policy_id"].as_str().unwrap_or_default(),
                contract["contract_hash"]
                    .as_str()
                    .unwrap_or_default()
                    .trim_start_matches("sha256:")
                    .chars()
                    .take(12)
                    .collect::<String>()
            ))
            .collect::<Vec<_>>()
            .join(", ")),
        &Value::Null,
    );
    let checkpoints = contracts
        .iter()
        .map(|contract| {
            format!(
                "{}@{}",
                contract["checkpoint"].as_str().unwrap_or_default(),
                contract["checkpoint_sha"]
                    .as_str()
                    .unwrap_or("unavailable")
                    .chars()
                    .take(12)
                    .collect::<String>()
            )
        })
        .collect::<BTreeSet<_>>();
    push(
        created,
        "checkpoint",
        "Checkpoint bound".into(),
        json!(checkpoints.iter().next()),
        json!(checkpoints.into_iter().collect::<Vec<_>>().join(", ")),
        &Value::Null,
    );
    if let Ok(Some(record)) = crate::session_orchestrator::load(&run.id) {
        for entry in record["history"].as_array().into_iter().flatten() {
            let kind = if entry["to"] == "AGENT_CONNECTED" {
                "agent"
            } else {
                "lifecycle"
            };
            let title = if kind == "agent" {
                "Agent connected".to_string()
            } else {
                format!("Phase {}", entry["to"].as_str().unwrap_or_default())
            };
            push(
                &entry["at"],
                kind,
                title,
                entry["to"].clone(),
                entry["reason"].clone(),
                &Value::Null,
            );
        }
    }
    let mut executions: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for item in &full.events {
        if item["event"] == "post_tool_use" {
            if let Some(digest) = item["arguments_digest"].as_str() {
                executions.entry(digest.to_string()).or_default().push(item);
            }
        }
    }
    let mut actions = Vec::new();
    let mut previous: Option<(Value, Value)> = None;
    for (record, item) in full.records.iter().zip(&full.events) {
        let seq = &record["seq"];
        let at = &record["at"];
        let name = record["event"].as_str().unwrap_or_default();
        match (record["category"].as_str(), name) {
            (Some("authorization"), _) => {
                let decision = record["decision"].as_str().unwrap_or("unknown");
                let executed = record["action_digest"]
                    .as_str()
                    .and_then(|digest| executions.get(digest))
                    .and_then(|runs| runs.iter().find(|run| run["at"].as_u64() >= at.as_u64()))
                    .copied();
                let before = record["budget"]["before"]["current"].as_i64().unwrap_or(0);
                let after = record["budget"]["after"]["current"].as_i64().unwrap_or(0);
                let effect = executed.map(|item| {
                    let files = &item["effect"]["files"];
                    json!({
                        "added": files["added"], "modified": files["modified"], "deleted": files["deleted"], "renamed": files["renamed"],
                        "symbols": item["effect"]["symbols"].as_array().map_or(0, Vec::len),
                        "violations": item["effect"]["violations"].as_array().map_or(0, Vec::len),
                    })
                });
                let risk = match decision {
                    "deny" => "high",
                    "approval_required" => "medium",
                    _ if executed.is_some_and(|item| item["verification"] == "fail") => "high",
                    _ => "low",
                };
                let inspection = json!({
                    "seq": seq,
                    "at": at,
                    "tool": record["tool"],
                    "action_type": record["operation"],
                    "event": name,
                    "resource": resources(record),
                    "normalized_action": normalized(record),
                    "action_digest": record["action_digest"],
                    "arguments": "not stored: the journal keeps only the digest and the normalized summary",
                    "decision": decision,
                    "reasons": record["reasons"],
                    "policy": policy_labels(record),
                    "zone": record["zones"].as_array().cloned().unwrap_or_default(),
                    "risk": risk,
                    "budget_impact": {"change": after - before, "before": record["budget"]["before"], "after": record["budget"]["after"], "reserved": item["budget_reserve"]},
                    "autonomy": record["autonomy"],
                    "safety": record["safety"],
                    "executed": executed.is_some(),
                    "execution_duration": executed.and_then(|item| item["at"].as_u64()).map(|end| end.saturating_sub(at.as_u64().unwrap_or(0))),
                    "effect": effect,
                    "verification": executed.map(|item| item["verification"].clone()),
                    "chain": record["chain"],
                    "link": link(seq),
                });
                push(
                    at,
                    "decision",
                    format!(
                        "{} {}",
                        record["operation"]
                            .as_str()
                            .unwrap_or("tool")
                            .to_uppercase(),
                        resources(record).join(", ")
                    ),
                    json!(match decision {
                        "allow" => "ALLOW",
                        "deny" => "DENY",
                        "approval_required" => "ASK",
                        other => other,
                    }),
                    json!(policy_labels(record).join(", ")),
                    seq,
                );
                actions.push(inspection);
            }
            (_, "post_tool_use") => {
                let changed = ["added", "modified", "deleted"]
                    .iter()
                    .flat_map(|key| strings(&item["effect"]["files"][*key]))
                    .collect::<Vec<_>>();
                push(
                    at,
                    "action",
                    format!("{} executed", record["tool"].as_str().unwrap_or("tool")),
                    json!(if changed.is_empty() {
                        "no change"
                    } else {
                        "changed"
                    }),
                    json!(changed.join(", ")),
                    seq,
                );
                if let Some(verdict) = item["verification"].as_str() {
                    push(
                        at,
                        "verification",
                        "Contract verification".into(),
                        json!(verdict.to_uppercase()),
                        Value::Null,
                        seq,
                    );
                }
            }
            (_, "stop") => push(
                at,
                "verification",
                "Verification at stop".into(),
                item["final_status"].clone(),
                Value::Null,
                seq,
            ),
            (_, "session_finalized") => {
                push(
                    at,
                    "verification",
                    "Final reconciliation".into(),
                    item["final_status"].clone(),
                    json!(strings(&item["findings"]).join("; ")),
                    seq,
                );
                push(
                    at,
                    "attestation",
                    "Attestation".into(),
                    json!(full.attestation["final_decision"]["decision"]),
                    json!(full.attestation["attestation_digest"]),
                    seq,
                );
            }
            (_, "session_start") => {
                if item["source"] != "manager" && item["source"] != "task orchestration" {
                    push(
                        at,
                        "agent",
                        "Agent connected".into(),
                        json!(item["source"].as_str().unwrap_or("startup")),
                        json!(item["model"]),
                        seq,
                    );
                }
            }
            (
                _,
                "session_resumed" | "session_cancelled" | "session_end" | "session_timed_out"
                | "authority_lapsed",
            ) => push(
                at,
                "lifecycle",
                name.replace('_', " "),
                json!(name.trim_start_matches("session_")),
                item["reason"].clone(),
                seq,
            ),
            _ => {}
        }
        let state = (record["autonomy"].clone(), record["safety"].clone());
        if let Some(from) = previous.as_ref().filter(|from| **from != state) {
            push(
                at,
                "autonomy",
                format!(
                    "Autonomy {} / {}",
                    state.0.as_str().unwrap_or_default(),
                    state.1.as_str().unwrap_or_default()
                ),
                state.1.clone(),
                json!(format!(
                    "from {} / {}{}",
                    from.0.as_str().unwrap_or_default(),
                    from.1.as_str().unwrap_or_default(),
                    item["kind"]
                        .as_str()
                        .map_or(String::new(), |kind| format!(" ({kind})"))
                )),
                seq,
            );
        }
        previous = Some(state);
        let before = record["budget"]["before"]["current"].as_i64().unwrap_or(0);
        let after = record["budget"]["after"]["current"].as_i64().unwrap_or(0);
        if before != after {
            push(
                at,
                "budget",
                format!(
                    "Budget {}",
                    if after < before {
                        "reduced"
                    } else {
                        "restored"
                    }
                ),
                json!(format!("{:+}", after - before)),
                json!(format!(
                    "{} of {} left",
                    after, record["budget"]["after"]["max"]
                )),
                seq,
            );
        }
    }
    for violation in session_violations(session) {
        push(
            &violation["at"],
            "violation",
            format!(
                "Violation: {}",
                violation["kind"].as_str().unwrap_or_default()
            ),
            violation["severity"].clone(),
            violation["consequence"].clone(),
            &violation["seq"],
        );
    }
    if let Ok(journal) = crate::delivery::journal(&run.id) {
        for event in &journal {
            let kind = event["kind"].as_str().unwrap_or_default();
            let (category, title) = match kind {
                "submitted" => ("delivery", "Delivery submitted".to_string()),
                "pull_request" => ("delivery", format!("Pull request #{}", event["number"])),
                "approval" => ("approval", "Approved".to_string()),
                "rejection" => ("approval", "Rejected".to_string()),
                "changes_requested" => ("approval", "Changes requested".to_string()),
                "exception" => ("approval", "Exception granted".to_string()),
                "merged" => ("merge", "Merged".to_string()),
                "merge_failed" => ("merge", "Merge failed".to_string()),
                "task_completion_queued" | "task_completed" => {
                    ("completion", kind.replace('_', " "))
                }
                "attestation_finalized" => ("attestation", "Attestation finalized".to_string()),
                _ => ("delivery", kind.replace('_', " ")),
            };
            let detail = event["by"]
                .as_str()
                .map(|by| format!("by {by}"))
                .or_else(|| {
                    event["sha"]
                        .as_str()
                        .map(|sha| format!("merge commit {sha}"))
                })
                .or_else(|| event["head"].as_str().map(|head| format!("commit {head}")));
            push(
                &event["at"],
                category,
                title,
                json!(kind),
                json!(detail),
                &Value::Null,
            );
        }
    }
    if let Some(task) = run.task().as_str() {
        for event in completions.iter().filter(|event| event["task_id"] == task) {
            let state = crate::task_completion::lifecycle_state(event);
            push(
                &event["created_at"],
                "completion",
                "Task completion queued".into(),
                json!(event["status"]),
                json!(format!(
                    "{} {}",
                    event["tracker"].as_str().unwrap_or("tracker"),
                    event["external_id"].as_str().unwrap_or_default()
                )),
                &Value::Null,
            );
            if state == "COMPLETED" {
                push(
                    &event["completed_at"],
                    "completion",
                    "Task completed".into(),
                    json!(state),
                    Value::Null,
                    &Value::Null,
                );
            }
        }
    }
    events.sort_by_key(|event| {
        (
            event["at"].as_u64().unwrap_or(0),
            event["seq"].as_u64().unwrap_or(0),
        )
    });
    (events, actions)
}

/** Answer a run's page: its row, bindings, timeline, and action inspection
 * Input
    - id: &str - run (session) id (validated)
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn detail(id: &str) -> Result<Value, (u16, String)> {
    let failed = |error: String| (500, error);
    let runs = Run::all().map_err(failed)?;
    let data = Dataset::new(&runs).map_err(failed)?;
    let session = data
        .sessions
        .iter()
        .find(|session| session.run.id == id)
        .ok_or_else(|| (404, format!("no run '{id}'")))?;
    let completions = super::completions().map_err(failed)?;
    let full = Run::full(id)
        .map_err(failed)?
        .ok_or_else(|| (404, format!("no run '{id}'")))?;
    let (mut events, mut actions) = timeline(session, &full, &completions);
    // A long run is paged: the first events here, the rest from /runs/{run}/events?after=SEQ
    let timeline_total = events.len();
    let actions_total = actions.len();
    events.truncate(PAGE);
    actions.truncate(PAGE);
    let next = (timeline_total > PAGE)
        .then(|| {
            events
                .iter()
                .filter_map(|event| event["seq"].as_u64())
                .max()
        })
        .flatten();
    let document = &full.document;
    let governance = &document["governance"];
    Ok(json!({
        "run": run_row(session, &data),
        "binding": {
            "binding_digest": document["binding_digest"],
            "created_at": document["created_at"],
            "expires_at": document["expires_at"],
            "provider_session": document["provider_session"],
            "contract_version": document["contracts"]["version"],
            "policies": document["contracts"]["contracts"].as_array().into_iter().flatten().map(|contract| json!({"policy_id": contract["policy_id"], "contract_hash": contract["contract_hash"], "checkpoint": contract["checkpoint"], "checkpoint_sha": contract["checkpoint_sha"], "link": format!("#/policies/{}", contract["policy_id"].as_str().unwrap_or_default())})).collect::<Vec<_>>(),
            "autonomy": governance["autonomy"],
            "budget": governance["budget"],
            "zone_set_version": governance["zone_set_version"],
            "zones": governance["zones"],
            "scope_modules": governance["scope_modules"],
            "task_contract": governance["task_contract"],
            "organization": governance["organization"],
            "environment": session.run.environment,
        },
        "summary": session.summary,
        "timeline": events,
        "timeline_total": timeline_total,
        "timeline_next": next,
        "actions": actions,
        "actions_total": actions_total,
        "violations": session_violations(session),
        "attestation": {"digest": session.run.attestation["attestation_digest"], "final_decision": session.run.attestation["final_decision"], "chain": session.run.attestation["evidence"]["chain"], "stored_digest": session.summary["evidence"]["stored_attestation_digest"]},
        "links": {
            "agent": format!("#/agents/{}", session.run.agent_id()),
            "tasks": session.attributes.get("task").into_iter().flatten().map(|task| format!("#/tasks/{task}")).collect::<Vec<_>>(),
            "repository": format!("#/repositories/{}", repository_id(&data.repository)),
        },
    }))
}

/** Most timeline events or actions one answer about a run carries */
const PAGE: usize = 2_000;

/** Page through a run's journal events after a sequence number (keyset paging on the immutable
 * sequence: a page never shifts when the journal grows)
 * Input
    - id: &str - run id (validated)
    - query: &Query - after (sequence number, default 0), limit (default 100, at most 1000),
      kind (timeline event kinds, comma-separated)
 * Output
    - Result<Value, (u16, String)> {run_id, items, next, remaining}
*/
pub(crate) fn events(id: &str, query: &Query) -> Result<Value, (u16, String)> {
    let failed = |error: String| (500, error);
    let after = match query.get("after") {
        None => 0,
        Some(text) => text.parse::<u64>().map_err(|_| {
            (
                400,
                format!("after must be a sequence number, not '{text}'"),
            )
        })?,
    };
    let limit = query.limit().map_err(|error| (400, error))?;
    let runs = Run::all().map_err(failed)?;
    let data = Dataset::new(&runs).map_err(failed)?;
    let session = data
        .sessions
        .iter()
        .find(|session| session.run.id == id)
        .ok_or_else(|| (404, format!("no run '{id}'")))?;
    let full = Run::full(id)
        .map_err(failed)?
        .ok_or_else(|| (404, format!("no run '{id}'")))?;
    let completions = super::completions().map_err(failed)?;
    let (events, _) = timeline(session, &full, &completions);
    let mut following = events
        .into_iter()
        .filter(|event| event["seq"].as_u64().is_some_and(|seq| seq > after))
        .filter(|event| query.admits("kind", &event["kind"]))
        .collect::<Vec<_>>();
    following.sort_by_key(|event| event["seq"].as_u64().unwrap_or(0));
    let remaining = following.len();
    following.truncate(limit);
    let next = (remaining > following.len())
        .then(|| following.last().and_then(|event| event["seq"].as_u64()))
        .flatten();
    Ok(
        json!({"run_id": id, "after": after, "items": following, "next": next, "remaining": remaining}),
    )
}
