// The Agents section: which agents operate across the organization and how they behave. An agent
// is an adapter profile running a model (Claude Code on claude-opus-5-5, Codex on gpt-5, ...),
// identified as "PROFILE.MODEL"; it is observed only through the sessions it ran, so every figure
// here is a projection of those sessions' bindings, journals, deliveries, and tasks for the time
// window and filters. Nothing here can restart an agent or change its autonomy, budget, or
// configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{json, Value};

use super::dashboard::{actor, newest, Dataset, Keys, Session, Window};
use super::detail::{pr_status, session_violations};
use super::{Query, Run, PROVIDERS};
use crate::util::now_unix;

/** Parameters the Agents endpoints accept: the time window and the global filters */
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

/** Rank a safety state from best to worst
 * Input
    - state: &str - active, degraded, or quarantined
 * Output
    - u8
*/
fn worse(state: &str) -> u8 {
    match state {
        "quarantined" => 2,
        "degraded" => 1,
        _ => 0,
    }
}

/** Share of a count, as a percentage with one decimal, null when there is nothing to divide by
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

/** Behaviour figures of a group of sessions (one agent, or one provider)
 * Input
    - sessions: &[(usize, &Session)] - the sessions with their indices in the dataset
    - data: &Dataset - every session and task
    - now: u64 - current time
 * Output
    - Value
*/
fn behaviour(sessions: &[(usize, &Session)], data: &Dataset, now: u64) -> Value {
    let indices = sessions
        .iter()
        .map(|(index, _)| *index)
        .collect::<BTreeSet<_>>();
    let tasks = data
        .tasks
        .iter()
        .filter(|task| task.sessions.iter().any(|index| indices.contains(index)))
        .collect::<Vec<_>>();
    let task_durations = tasks
        .iter()
        .map(|task| Keys::new(task, data, now))
        .filter(|keys| keys.terminal)
        .map(|keys| (keys.end - keys.start) as f64)
        .collect::<Vec<_>>();
    let passed = sessions
        .iter()
        .filter(|(_, session)| session.verification == "passed")
        .count();
    let failed = sessions
        .iter()
        .filter(|(_, session)| session.verification == "failed")
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
    let autonomous = sessions
        .iter()
        .filter(|(_, session)| {
            let autonomy = &session.summary["autonomy"];
            autonomy["initial"] == "autonomous"
                || autonomy["current"] == "autonomous"
                || autonomy["allowed_mutating_actions_by_level"]["autonomous"].is_number()
        })
        .count();
    let active = sessions
        .iter()
        .filter(|(_, session)| session.active())
        .collect::<Vec<_>>();
    let latest = sessions.iter().max_by_key(|(_, session)| session.last);
    let safety = active
        .iter()
        .map(|(_, session)| {
            session.summary["safety"]["state"]
                .as_str()
                .unwrap_or("active")
        })
        .max_by_key(|state| worse(state))
        .or_else(|| latest.and_then(|(_, session)| session.summary["safety"]["state"].as_str()));
    let durations = sessions
        .iter()
        .filter(|(_, session)| !session.active())
        .map(|(_, session)| {
            session
                .ended
                .unwrap_or(session.last)
                .saturating_sub(session.started) as f64
        })
        .collect::<Vec<_>>();
    json!({
        "status": if active.is_empty() { "idle" } else { "active" },
        "repositories": sessions.iter().flat_map(|(_, session)| session.attributes.get("repository").cloned().unwrap_or_default()).collect::<BTreeSet<_>>().len(),
        "tasks": tasks.len(),
        "sessions": sessions.len(),
        "active_sessions": active.len(),
        "autonomous_sessions": autonomous,
        "success_rate": rate(passed, passed + failed),
        "verification_rate": rate(passed + failed, sessions.len()),
        "violations": violations,
        "critical_violations": critical,
        "violation_rate": if sessions.is_empty() { Value::Null } else { json!(((violations as f64 / sessions.len() as f64) * 100.0).round() / 100.0) },
        "average_autonomy_score": average(&sessions.iter().filter_map(|(_, session)| session.score).collect::<Vec<_>>()),
        "average_budget_consumption": average(&sessions.iter().filter_map(|(_, session)| session.budget).collect::<Vec<_>>()),
        "human_intervention_rate": rate(sessions.iter().filter(|(_, session)| session.human).count(), sessions.len()),
        "average_task_duration": average(&task_durations),
        "average_session_duration": average(&durations),
        "last_active": sessions.iter().map(|(_, session)| session.last).max(),
        "safety": safety,
    })
}

/** Select the sessions the window and filters keep
 * Input
    - data: &'a Dataset - every session
    - query: &Query - filters
    - window: &Window - time window
 * Output
    - Vec<(usize, &'a Session<'a>)>
*/
fn chosen<'a>(data: &'a Dataset, query: &Query, window: &Window) -> Vec<(usize, &'a Session<'a>)> {
    let (matching, _) = data.select(query);
    data.sessions
        .iter()
        .enumerate()
        .filter(|(index, session)| matching.contains(index) && session.during(window))
        .collect()
}

/** The values each filter can take, from every session
 * Input
    - data: &Dataset - every session and task
 * Output
    - Value
*/
fn facets(data: &Dataset) -> Value {
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
        .entry("task")
        .or_default()
        .extend(data.tasks.iter().take(500).map(|task| task.id.clone()));
    facets
        .entry("task_status")
        .or_default()
        .extend(data.states.values().cloned());
    facets
        .entry("repository")
        .or_default()
        .insert(data.repository.clone());
    super::dashboard::bounded(facets).0
}

/** Answer the Agents page: one row per agent, and one aggregate per provider, for the window and
 * filters
 * Input
    - query: &Query - window and filters
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn list(query: &Query) -> Result<Value, (u16, String)> {
    let now = now_unix();
    let window = Window::parse(query, now).map_err(|error| (400, error))?;
    let failed = |error: String| (500, error);
    let runs = Run::all().map_err(failed)?;
    let data = Dataset::new(&runs).map_err(failed)?;
    let sessions = chosen(&data, query, &window);
    let mut agents: BTreeMap<String, Vec<(usize, &Session)>> = BTreeMap::new();
    for (index, session) in &sessions {
        agents
            .entry(session.run.agent_id())
            .or_default()
            .push((*index, session));
    }
    let rows = agents
        .iter()
        .map(|(id, sessions)| {
            let latest = sessions
                .iter()
                .max_by_key(|(_, session)| session.last)
                .map(|(_, session)| session);
            let mut row = behaviour(sessions, &data, now);
            row["agent_id"] = json!(id);
            row["name"] = json!(latest.map(|session| actor(&session.run.agent())));
            row["profile"] = json!(latest.map(|session| session.run.agent()));
            row["provider"] = json!(latest.map(|session| session.run.provider()));
            row["model"] = json!(latest.map(|session| session.run.model()));
            row["adapter_version"] =
                json!(latest.and_then(|session| session.run.adapter_version()));
            row["link"] = json!(format!("#/agents/{id}"));
            row
        })
        .collect::<Vec<_>>();
    let providers = PROVIDERS
        .iter()
        .map(|name| {
            let members = sessions
                .iter()
                .filter(|(_, session)| session.run.provider() == *name)
                .copied()
                .collect::<Vec<_>>();
            let mut value = behaviour(&members, &data, now);
            value["provider"] = json!(name);
            value["agents"] = json!(rows.iter().filter(|row| row["provider"] == *name).count());
            value
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "now": now,
        "window": {"range": window.name, "from": window.from, "to": window.to},
        "agents": rows,
        "providers": providers,
        "facets": facets(&data),
    }))
}

/** Answer Agent Details: identity, provider, models, repositories, tasks, sessions, the policies and
 * zones its actions met, its violations, and its autonomy, budget, verification, and delivery
 * history, for the window and filters
 * Input
    - id: &str - agent id (validated)
    - query: &Query - window and filters
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn detail(id: &str, query: &Query) -> Result<Value, (u16, String)> {
    let now = now_unix();
    let window = Window::parse(query, now).map_err(|error| (400, error))?;
    let failed = |error: String| (500, error);
    let runs: Vec<Arc<Run>> = Run::all().map_err(failed)?;
    let data = Dataset::new(&runs).map_err(failed)?;
    let ever = data
        .sessions
        .iter()
        .filter(|session| session.run.agent_id() == id)
        .collect::<Vec<_>>();
    if ever.is_empty() {
        return Err((404, format!("no agent '{id}'")));
    }
    let mut sessions = chosen(&data, query, &window)
        .into_iter()
        .filter(|(_, session)| session.run.agent_id() == id)
        .collect::<Vec<_>>();
    sessions.sort_by_key(|(_, session)| session.started);
    let latest = ever.iter().max_by_key(|session| session.last).copied();
    let identity = json!({
        "agent_id": id,
        "name": latest.map(|session| actor(&session.run.agent())),
        "profile": latest.map(|session| session.run.agent()),
        "provider": latest.map(|session| session.run.provider()),
        "model": latest.map(|session| session.run.model()),
        "models": ever.iter().flat_map(|session| session.run.models()).collect::<BTreeSet<_>>(),
        "adapter_versions": ever.iter().filter_map(|session| session.run.adapter_version()).collect::<BTreeSet<_>>(),
        "provider_session_ids": ever.iter().filter_map(|session| session.run.document["provider_session"].as_str().map(String::from)).collect::<BTreeSet<_>>(),
        "first_seen": ever.iter().map(|session| session.started).min(),
        "last_seen": ever.iter().map(|session| session.last).max(),
        "sessions_ever": ever.len(),
    });
    let indices = sessions
        .iter()
        .map(|(index, _)| *index)
        .collect::<BTreeSet<_>>();

    let mut repositories: BTreeMap<String, usize> = BTreeMap::new();
    for (_, session) in &sessions {
        for repository in session.attributes.get("repository").into_iter().flatten() {
            *repositories.entry(repository.clone()).or_default() += 1;
        }
    }
    let tasks = data
        .tasks
        .iter()
        .filter(|task| task.sessions.iter().any(|index| indices.contains(index)))
        .map(|task| {
            let keys = Keys::new(task, &data, now);
            let row = keys.row(&data.repository);
            json!({"task_id": task.id, "title": row["title"], "status": row["status"], "final": row["final"], "start": row["start"], "duration": row["duration"], "violations": row["violations"], "merge_status": row["merge_status"], "completion_status": row["completion_status"], "link": row["link"]})
        })
        .collect::<Vec<_>>();
    let session_rows = sessions
        .iter()
        .map(|(_, session)| {
            let summary = &session.summary;
            json!({
                "session_id": session.run.id,
                "task_id": session.run.task(),
                "model": session.run.model(),
                "lifecycle": summary["lifecycle"],
                "started_at": session.started,
                "last_activity_at": session.last,
                "autonomy": summary["autonomy"]["current"],
                "safety": summary["safety"]["state"],
                "decisions": summary["actions"],
                "violations": summary["violations"]["total"],
                "verification": session.verification,
                "link": format!("#/runs/{}", session.run.id),
            })
        })
        .collect::<Vec<_>>();

    // Policies and zones its actions met, with how they were decided
    let mut policies: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut zones: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    for (_, session) in &sessions {
        for contract in session.run.contracts().as_array().into_iter().flatten() {
            if let Some(policy) = contract["policy_id"].as_str() {
                policies
                    .entry(policy.to_string())
                    .or_default()
                    .entry("bound_sessions".into())
                    .and_modify(|count| *count += 1)
                    .or_insert(1);
            }
        }
        for act in session
            .run
            .acts
            .iter()
            .filter(|act| act.category == Some("authorization"))
        {
            let decision = act.decision.unwrap_or("unknown").to_string();
            for (map, names) in [(&mut policies, act.policies), (&mut zones, act.zones)] {
                for name in names {
                    *map.entry(name.to_string())
                        .or_default()
                        .entry(decision.clone())
                        .or_default() += 1;
                }
            }
        }
    }
    let tally = |map: BTreeMap<String, BTreeMap<String, u64>>, key: &str| {
        map.into_iter()
            .map(|(name, counts)| {
                let mut value = json!({key: name, "allow": 0, "deny": 0, "approval_required": 0, "bound_sessions": 0});
                for (decision, count) in counts {
                    value[decision] = json!(count);
                }
                value
            })
            .collect::<Vec<_>>()
    };

    // Violations, and the autonomy, budget, verification, and delivery history
    let violations = sessions
        .iter()
        .flat_map(|(_, session)| session_violations(session))
        .filter(|violation| window.holds(violation["at"].as_u64().unwrap_or(0)))
        .collect::<Vec<_>>();
    let mut autonomy = Vec::new();
    let mut budget_changes = Vec::new();
    for (_, session) in &sessions {
        let mut previous: Option<(Option<&str>, Option<&str>)> = None;
        for act in &session.run.acts {
            let state = (act.autonomy, act.safety);
            if let Some(from) = previous.as_ref().filter(|from| **from != state) {
                let extra = act.extra();
                autonomy.push(json!({
                    "event_id": session.run.event_id(act.seq),
                    "session_id": session.run.id,
                    "seq": act.seq,
                    "at": act.at,
                    "from": {"autonomy": from.0, "safety": from.1},
                    "to": {"autonomy": state.0, "safety": state.1},
                    "trigger": extra.trigger,
                    "kind": extra.kind,
                    "actor": extra.actor,
                    "link": format!("#/runs/{}?seq={}", session.run.id, act.seq),
                }));
            }
            previous = Some(state);
            let before = act.before.map_or(0, |budget| budget.held());
            let after = act.after.map_or(0, |budget| budget.held());
            if before != after {
                budget_changes.push(json!({"event_id": session.run.event_id(act.seq), "session_id": session.run.id, "seq": act.seq, "at": act.at, "event": act.event, "change": after - before, "after": act.after.map(|budget| budget.to_json())}));
            }
        }
    }
    let budget = sessions
        .iter()
        .map(|(_, session)| {
            json!({
                "session_id": session.run.id,
                "start": session.run.acts.first().and_then(|act| act.before).map(|budget| budget.to_json()),
                "end": session.run.acts.last().and_then(|act| act.after).map(|budget| budget.to_json()),
                "consumed": session.summary["budget"]["consumed"],
                "consumption_percent": session.budget,
            })
        })
        .collect::<Vec<_>>();
    let verification = sessions
        .iter()
        .map(|(_, session)| {
            let summary = &session.summary["verification"];
            json!({
                "session_id": session.run.id,
                "at": session.verified_at,
                "status": session.verification,
                "last": summary["last"],
                "final_decision": summary["final_decision"],
                "contract_tests_passed": summary["contract_tests_passed"],
                "repository_tests_passed": summary["repository_tests_passed"],
                "attestation_digest": session.summary["evidence"]["attestation_digest"],
                "link": format!("#/runs/{}", session.run.id),
            })
        })
        .collect::<Vec<_>>();
    let delivery = sessions
        .iter()
        .filter(|(_, session)| !session.run.delivery.is_null())
        .map(|(_, session)| {
            let state = &session.run.delivery;
            json!({
                "session_id": session.run.id,
                "task_id": session.run.task(),
                "branch": state["branch"],
                "commit": state["head"],
                "pull_request": if state["pull_request"].is_null() { Value::Null } else { json!({"number": state["pull_request"]["number"], "url": state["pull_request"]["url"]}) },
                "status": pr_status(state),
                "approvals": state["approvals"].as_array().map_or(0, Vec::len),
                "merge_sha": state["merged"]["sha"],
                "merged_at": state["merged"]["at"],
            })
        })
        .collect::<Vec<_>>();
    let mut stats = behaviour(&sessions, &data, now);
    if let Some(object) = stats.as_object_mut() {
        object.remove("tasks");
        object.remove("repositories");
    }
    let (tasks, tasks_total) = newest(tasks, "start");
    let (session_rows, sessions_total) = newest(session_rows, "started_at");
    let (violations, violations_total) = newest(violations, "at");
    let (autonomy, autonomy_total) = newest(autonomy, "at");
    let (budget, _) = newest(budget, "at");
    let (budget_changes, budget_changes_total) = newest(budget_changes, "at");
    let (verification, _) = newest(verification, "at");
    let (delivery, _) = newest(delivery, "at");
    Ok(json!({
        "now": now,
        "window": {"range": window.name, "from": window.from, "to": window.to},
        "identity": identity,
        "behaviour": stats,
        "repositories": repositories.into_iter().map(|(name, count)| json!({"repository": name, "sessions": count})).collect::<Vec<_>>(),
        "tasks": tasks,
        "tasks_total": tasks_total,
        "sessions": session_rows,
        "sessions_total": sessions_total,
        "policies": tally(policies, "policy"),
        "zones": tally(zones, "zone"),
        "violations": violations,
        "violations_total": violations_total,
        "autonomy_history": autonomy,
        "autonomy_history_total": autonomy_total,
        "budget_history": {"sessions": budget, "changes": budget_changes, "changes_total": budget_changes_total},
        "verification_history": verification,
        "delivery_history": delivery,
        "facets": facets(&data),
    }))
}
