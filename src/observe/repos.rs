// Repositories: the list of connected repositories and, for one, its health, activity, tasks,
// agents, sessions, policies, zones, violations, deliveries, and attestations, for the time window
// and filters. One .crane governs one repository today, so the list has one row; the shape is
// the same for many. Everything is a projection of the connection record and the runs; nothing
// here can connect, disconnect, or change a repository.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use super::dashboard::{actor, newest, Dataset, Keys, Session, Window, DETAIL_LIMIT};
use super::detail::{pr_status, session_violations, strings};
use super::runs::{repository_id, run_row};
use super::{Query, Run};
use crate::util::now_unix;

/** Parameters the Repositories pages accept: the window and the global filters */
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
];

/** Share as a percentage with one decimal, null when there is nothing to divide by
 * Input
    - part: usize - count
    - whole: usize - total
 * Output
    - Value
*/
fn rate(part: usize, whole: usize) -> Value {
    if whole == 0 {
        Value::Null
    } else {
        json!(((part as f64 / whole as f64) * 1000.0).round() / 10.0)
    }
}

/** Average with one decimal, null without values
 * Input
    - values: &[f64] - values
 * Output
    - Value
*/
fn average(values: &[f64]) -> Value {
    if values.is_empty() {
        Value::Null
    } else {
        json!(((values.iter().sum::<f64>() / values.len() as f64) * 10.0).round() / 10.0)
    }
}

/** What one repository's pages read: its connection and the runs and tasks the window and
 * filters keep
 * Fields
    - data: Dataset - every session and task
    - window: Window - time window
    - now: u64 - current time
    - sessions: Vec<(usize, &Session)> - runs kept, with their dataset indices
    - tasks: Vec<&Task> - tasks those runs worked on, or that the filters keep
    - connection: Option<Value> - the connection record
*/
struct Scope<'a> {
    data: &'a Dataset<'a>,
    window: Window,
    now: u64,
    sessions: Vec<(usize, &'a Session<'a>)>,
    tasks: Vec<&'a super::dashboard::Task>,
    connection: Option<Value>,
}

impl<'a> Scope<'a> {
    /** Read a repository's scope
     * Input
        - data: &'a Dataset - every session and task
        - query: &Query - window and filters
     * Output
        - Result<Scope<'a>, (u16, String)>
    */
    fn new(data: &'a Dataset<'a>, query: &Query) -> Result<Self, (u16, String)> {
        let now = now_unix();
        let window = Window::parse(query, now).map_err(|error| (400, error))?;
        let (matching, tasks) = data.select(query);
        let sessions = data
            .sessions
            .iter()
            .enumerate()
            .filter(|(index, session)| matching.contains(index) && session.during(&window))
            .collect::<Vec<_>>();
        let tasks = tasks
            .into_iter()
            .filter(|task| {
                let keys = Keys::new(task, data, now);
                keys.start <= window.to && keys.end >= window.from
            })
            .collect();
        Ok(Self {
            data,
            window,
            now,
            sessions,
            tasks,
            connection: crate::repo::load().map_err(|error| (500, error))?,
        })
    }

    /** Whether a task's sessions needed a human (an approval or an intervention)
     * Input
        - task: &super::dashboard::Task - task
     * Output
        - bool
    */
    fn intervened(&self, task: &super::dashboard::Task) -> bool {
        task.sessions
            .iter()
            .any(|index| self.data.sessions[*index].human)
    }

    /** The connection facts and the trusted checkpoint's state
     * Input
        - None (uses self)
     * Output
        - Value
    */
    fn connection(&self) -> Value {
        match &self.connection {
            Some(record) => {
                let checkpoint = record["trusted_checkpoint"].as_str().unwrap_or("baseline");
                json!({
                    "provider": record["provider"],
                    "full_name": record["full_name"],
                    "default_branch": record["default_branch"],
                    "status": record["connection_status"],
                    "connected_at": record["connected_at"],
                    "discovery": record["discovery"],
                    "trusted_checkpoint": crate::repo::checkpoint_state(checkpoint),
                })
            }
            None => {
                json!({"provider": "local", "full_name": null, "default_branch": null, "status": "not connected", "connected_at": null, "discovery": null, "trusted_checkpoint": null})
            }
        }
    }

    /** The headline figures of the list row and the health section
     * Input
        - None (uses self)
     * Output
        - Value
    */
    fn health(&self) -> Value {
        let sessions = &self.sessions;
        let keys = self
            .tasks
            .iter()
            .map(|task| Keys::new(task, self.data, self.now))
            .collect::<Vec<_>>();
        let worked = keys
            .iter()
            .filter(|keys| !keys.sessions.is_empty())
            .collect::<Vec<_>>();
        let successful = keys
            .iter()
            .filter(|keys| match keys.task.state.as_deref() {
                Some(state) => matches!(state, "COMPLETED" | "MERGED"),
                None => {
                    !keys.sessions.is_empty()
                        && keys
                            .sessions
                            .iter()
                            .all(|session| session.verification == "passed")
                }
            })
            .count();
        let failed = keys
            .iter()
            .filter(|keys| match keys.task.state.as_deref() {
                Some(state) => state == "FAILED",
                None => keys
                    .sessions
                    .iter()
                    .any(|session| session.verification == "failed"),
            })
            .count();
        let completed = keys
            .iter()
            .filter(|keys| keys.task.completed.is_some())
            .collect::<Vec<_>>();
        let delivered = sessions
            .iter()
            .filter(|(_, session)| !session.run.delivery.is_null())
            .collect::<Vec<_>>();
        let pull_requests = delivered
            .iter()
            .filter(|(_, session)| !session.run.delivery["pull_request"].is_null())
            .count();
        let merged = delivered
            .iter()
            .filter(|(_, session)| !session.run.delivery["merged"].is_null())
            .count();
        let approved = delivered
            .iter()
            .filter(|(_, session)| {
                matches!(pr_status(&session.run.delivery), "approved" | "merged")
            })
            .count();
        let violations = sessions
            .iter()
            .map(|(_, session)| session.summary["violations"]["total"].as_u64().unwrap_or(0))
            .sum::<u64>();
        let critical = sessions
            .iter()
            .map(|(_, session)| {
                session.summary["violations"]["critical"]
                    .as_u64()
                    .unwrap_or(0)
            })
            .sum::<u64>();
        let outcomes = sessions
            .iter()
            .filter(|(_, session)| session.verification != "pending")
            .count();
        let agents = sessions
            .iter()
            .map(|(_, session)| session.run.agent_id())
            .collect::<BTreeSet<_>>();
        let active_agents = sessions
            .iter()
            .filter(|(_, session)| session.active())
            .map(|(_, session)| session.run.agent_id())
            .collect::<BTreeSet<_>>();
        json!({
            "tasks": self.tasks.len(),
            "sessions": sessions.len(),
            "agents": agents.len(),
            "active_agents": active_agents.len(),
            "autonomous_tasks": worked.iter().filter(|keys| !self.intervened(keys.task)).count(),
            "manually_intervened_tasks": worked.iter().filter(|keys| self.intervened(keys.task)).count(),
            "successful_tasks": successful,
            "failed_tasks": failed,
            "verification_failures": sessions.iter().filter(|(_, session)| session.verification == "failed").count(),
            "violations": violations,
            "critical_violations": critical,
            "autonomous_completion_rate": rate(completed.iter().filter(|keys| !self.intervened(keys.task)).count(), completed.len()),
            "verification_rate": rate(outcomes, sessions.len()),
            "average_autonomy_score": average(&sessions.iter().filter_map(|(_, session)| session.score).collect::<Vec<_>>()),
            "average_task_duration": average(&keys.iter().filter(|keys| keys.terminal).map(|keys| (keys.end - keys.start) as f64).collect::<Vec<_>>()),
            "average_budget_consumption": average(&sessions.iter().filter_map(|(_, session)| session.budget).collect::<Vec<_>>()),
            "pull_requests": pull_requests,
            "merged_pull_requests": merged,
            "pr_success_rate": rate(approved, pull_requests),
            "merge_rate": rate(merged, pull_requests),
            "last_activity": sessions.iter().map(|(_, session)| session.last).max(),
        })
    }
}

/** Answer the Repositories page: one row per connected repository
 * Input
    - query: &Query - window and filters
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn list(query: &Query) -> Result<Value, (u16, String)> {
    let runs = Run::all().map_err(|error| (500, error))?;
    let data = Dataset::new(&runs).map_err(|error| (500, error))?;
    let scope = Scope::new(&data, query)?;
    let connection = scope.connection();
    let mut row = scope.health();
    row["repository"] = json!(data.repository);
    row["repository_id"] = json!(repository_id(&data.repository));
    row["provider"] = connection["provider"].clone();
    row["default_branch"] = connection["default_branch"].clone();
    row["status"] = connection["status"].clone();
    row["trusted_checkpoint"] = connection["trusted_checkpoint"].clone();
    row["link"] = json!(format!(
        "#/repositories/{}",
        repository_id(&data.repository)
    ));
    Ok(json!({
        "now": scope.now,
        "window": {"range": scope.window.name, "from": scope.window.from, "to": scope.window.to},
        "repositories": [row],
    }))
}

/** Answer Repository Details: overview and health, activity over time, tasks, agents (and the
 * most active), sessions, policies (and the most triggered), zones (and the highest-risk),
 * violations, deliveries, and attestations
 * Input
    - id: &str - repository id (validated)
    - query: &Query - window and filters
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn detail(id: &str, query: &Query) -> Result<Value, (u16, String)> {
    let runs = Run::all().map_err(|error| (500, error))?;
    let data = Dataset::new(&runs).map_err(|error| (500, error))?;
    if repository_id(&data.repository) != id {
        return Err((404, format!("no repository '{id}'")));
    }
    let scope = Scope::new(&data, query)?;
    let sessions = &scope.sessions;
    let window = &scope.window;
    let crane = super::crane_root().map_err(|error| (500, error))?;
    let decisions = sessions
        .iter()
        .flat_map(|(_, session)| {
            session
                .run
                .acts
                .iter()
                .filter(|act| act.category == Some("authorization"))
                .map(move |act| (*session, act))
        })
        .filter(|(_, act)| window.holds(act.at))
        .collect::<Vec<_>>();
    let violations = sessions
        .iter()
        .flat_map(|(_, session)| session_violations(session))
        .filter(|violation| window.holds(violation["at"].as_u64().unwrap_or(0)))
        .collect::<Vec<_>>();

    // Activity over time
    let start = window.from - window.from % window.bucket;
    let buckets = (0..)
        .map(|index| start + index * window.bucket)
        .take_while(|at| *at <= window.to)
        .collect::<Vec<_>>();
    let mut activity = buckets.iter().map(|at| json!({"t": at, "runs": 0, "decisions": 0, "denied": 0, "violations": 0, "merged": 0})).collect::<Vec<_>>();
    let mut bump = |at: u64, key: &str| {
        if window.holds(at) {
            let index = ((at - start) / window.bucket) as usize;
            if let Some(point) = activity.get_mut(index) {
                point[key] = json!(point[key].as_u64().unwrap_or(0) + 1);
            }
        }
    };
    for (_, session) in sessions {
        bump(session.started, "runs");
        if let Some(at) = session.run.delivery["merged"]["at"].as_u64() {
            bump(at, "merged");
        }
    }
    for (_, record) in &decisions {
        let at = record.at;
        bump(at, "decisions");
        if record.decision == Some("deny") {
            bump(at, "denied");
        }
    }
    for violation in &violations {
        bump(violation["at"].as_u64().unwrap_or(0), "violations");
    }

    // Agents, and the most active
    let mut agents: BTreeMap<String, (usize, usize, u64, u64, u64)> = BTreeMap::new();
    for (_, session) in sessions {
        let entry = agents.entry(session.run.agent_id()).or_default();
        entry.0 += 1;
        entry.1 += usize::from(session.active());
        entry.2 += session.summary["actions"]["total"].as_u64().unwrap_or(0);
        entry.3 += session.summary["violations"]["total"].as_u64().unwrap_or(0);
        entry.4 = entry.4.max(session.last);
    }
    let mut agent_rows = agents
        .iter()
        .map(|(agent, (runs, active, actions, violations, last))| {
            let name = sessions.iter().find(|(_, session)| session.run.agent_id() == *agent).map(|(_, session)| actor(&session.run.agent()));
            json!({"agent_id": agent, "name": name, "runs": runs, "active_runs": active, "actions": actions, "violations": violations, "last_active": last, "link": format!("#/agents/{agent}")})
        })
        .collect::<Vec<_>>();
    agent_rows.sort_by(|left, right| right["actions"].as_u64().cmp(&left["actions"].as_u64()));

    // Policies, and the most triggered
    let mut triggered: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
    for (_, record) in &decisions {
        for policy in record.policies {
            let entry = triggered.entry(policy.to_string()).or_default();
            match record.decision {
                Some("allow") => entry.0 += 1,
                Some("deny") => entry.1 += 1,
                _ => entry.2 += 1,
            }
        }
    }
    let state = crate::task_contracts::activation::current().map_err(|error| (500, error))?;
    let mut policies = state["policies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|policy| policy["policy"].as_str().map(String::from))
        .chain(triggered.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|policy| {
            let (allow, deny, ask) = triggered.get(&policy).copied().unwrap_or_default();
            let layer = state["policies"].as_array().into_iter().flatten().find(|entry| entry["policy"] == policy.as_str()).map_or(json!("bound only"), |entry| entry["layer"].clone());
            json!({"policy_id": policy, "type": layer, "triggered": allow + deny + ask, "allow": allow, "deny": deny, "approval_required": ask, "link": format!("#/policies/{policy}")})
        })
        .collect::<Vec<_>>();
    policies.sort_by(|left, right| right["triggered"].as_u64().cmp(&left["triggered"].as_u64()));

    // Zones, and the highest-risk
    let (zones, _) = crate::zones::model::load(&crane).map_err(|error| (500, error))?;
    let mut zone_rows = zones
        .iter()
        .map(|zone| {
            let in_zone = |value: &Value| strings(value).contains(&zone.zone_id);
            let zone_decisions = decisions
                .iter()
                .filter(|(_, record)| record.zones.contains(&zone.zone_id.as_str()))
                .collect::<Vec<_>>();
            let zone_violations = violations
                .iter()
                .filter(|violation| in_zone(&violation["zone"]))
                .collect::<Vec<_>>();
            let critical = zone_violations
                .iter()
                .filter(|violation| violation["severity"] == "critical")
                .count();
            let denied = zone_decisions
                .iter()
                .filter(|(_, record)| record.decision == Some("deny"))
                .count();
            let asked = zone_decisions
                .iter()
                .filter(|(_, record)| record.decision == Some("approval_required"))
                .count();
            let criticality = match zone.criticality.name() {
                "restricted" => 4.0,
                "critical" => 3.0,
                "sensitive" => 2.0,
                _ => 1.0,
            };
            let risk = criticality
                * (1.0
                    + critical as f64 * 10.0
                    + (zone_violations.len() - critical) as f64 * 3.0
                    + denied as f64
                    + asked as f64 * 0.5);
            json!({
                "zone_id": zone.zone_id,
                "criticality": zone.criticality.name(),
                "autonomy": zone.default_autonomy.min(zone.criticality.autonomy_cap()).name(),
                "decisions": zone_decisions.len(),
                "denied": denied,
                "approval_required": asked,
                "violations": zone_violations.len(),
                "critical_violations": critical,
                "risk": risk,
                "link": format!("#/zones/{}", zone.zone_id),
            })
        })
        .collect::<Vec<_>>();
    zone_rows.sort_by(|left, right| {
        right["risk"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&left["risk"].as_f64().unwrap_or(0.0))
    });

    // Tasks, sessions, violations, deliveries, attestations
    let mut tasks = scope
        .tasks
        .iter()
        .map(|task| {
            let row = Keys::new(task, &data, scope.now).row(&data.repository);
            json!({"task_id": row["task_id"], "title": row["title"], "status": row["status"], "final": row["final"], "start": row["start"], "duration": row["duration"], "runs": row["runs"], "violations": row["violations"], "merge_status": row["merge_status"], "intervened": scope.intervened(task), "link": row["link"]})
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|left, right| right["start"].as_u64().cmp(&left["start"].as_u64()));
    let mut ordered = sessions
        .iter()
        .map(|(_, session)| *session)
        .collect::<Vec<_>>();
    ordered.sort_by_key(|session| std::cmp::Reverse(session.started));
    let deliveries = ordered
        .iter()
        .filter(|session| !session.run.delivery.is_null())
        .map(|session| {
            let state = &session.run.delivery;
            json!({"run_id": session.run.id, "task_id": session.attributes.get("task").and_then(|tasks| tasks.iter().next().cloned()), "branch": state["branch"], "commit": state["head"], "pull_request": if state["pull_request"].is_null() { Value::Null } else { json!({"number": state["pull_request"]["number"], "url": state["pull_request"]["url"]}) }, "status": pr_status(state), "approvals": state["approvals"].as_array().map_or(0, Vec::len), "merge_sha": state["merged"]["sha"], "merged_at": state["merged"]["at"], "trusted_checkpoint": state["merged"]["trusted_checkpoint"], "link": format!("#/runs/{}", session.run.id)})
        })
        .collect::<Vec<_>>();
    let attestations = ordered
        .iter()
        .map(|session| {
            let attestation = &session.run.attestation;
            let stored = &session.summary["evidence"]["stored_attestation_digest"];
            json!({"run_id": session.run.id, "digest": attestation["attestation_digest"], "stored_digest": stored, "matches_stored": if stored.is_null() { Value::Null } else { json!(*stored == attestation["attestation_digest"]) }, "final_decision": attestation["final_decision"]["decision"], "journal_chain": attestation["evidence"]["chain"]["status"], "events": attestation["evidence"]["chain"]["length"], "link": format!("#/runs/{}", session.run.id)})
        })
        .collect::<Vec<_>>();
    let mut violation_rows = violations;
    violation_rows
        .sort_by_key(|violation| std::cmp::Reverse(violation["at"].as_u64().unwrap_or(0)));
    let violation_total = violation_rows.len();
    violation_rows.truncate(100);
    let (tasks, tasks_total) = newest(tasks, "start");
    let agents_total = agent_rows.len();
    let (deliveries, deliveries_total) = newest(deliveries, "at");
    let (attestations, attestations_total) = newest(attestations, "at");
    Ok(json!({
        "now": scope.now,
        "window": {"range": window.name, "from": window.from, "to": window.to, "bucket": window.bucket},
        "repository_id": id,
        "name": data.repository,
        "overview": scope.connection(),
        "health": scope.health(),
        "activity": activity,
        "tasks": tasks,
        "tasks_total": tasks_total,
        "agents": agent_rows.iter().take(DETAIL_LIMIT).cloned().collect::<Vec<_>>(),
        "agents_total": agents_total,
        "most_active_agents": agent_rows.into_iter().take(5).collect::<Vec<_>>(),
        "sessions": ordered.iter().take(100).map(|session| run_row(session, &data)).collect::<Vec<_>>(),
        "sessions_total": ordered.len(),
        "policies": policies.clone(),
        "most_triggered_policies": policies.into_iter().filter(|policy| policy["triggered"].as_u64().unwrap_or(0) > 0).take(5).collect::<Vec<_>>(),
        "zones": zone_rows.clone(),
        "highest_risk_zones": zone_rows.into_iter().filter(|zone| zone["decisions"].as_u64().unwrap_or(0) > 0 || zone["violations"].as_u64().unwrap_or(0) > 0).take(5).collect::<Vec<_>>(),
        "violations": violation_rows,
        "violations_total": violation_total,
        "deliveries": deliveries,
        "deliveries_total": deliveries_total,
        "attestations": attestations,
        "attestations_total": attestations_total,
        "zone_risk_definition": "criticality weight (restricted 4, critical 3, sensitive 2, routine 1) × (1 + 10 × critical violations + 3 × other violations + denials + 0.5 × approval requests) in the window",
    }))
}
