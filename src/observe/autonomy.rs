// Autonomy and risk: how much autonomy agents actually exercise, and whether it is safe. Every
// figure comes from Society's own recorded state: the autonomy and safety each action was decided
// at (replayed by the evidence projection through the run's bound autonomy policy), the run's
// budget replayed through its bound budget model (the same operations the runtime applied), its
// decisions, violations, verification, and quarantines. The autonomy score is an observability
// metric with a fixed, documented formula; nothing here can raise or lower autonomy, refill a
// budget, or change a safety state.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::compact::Act;
use super::dashboard::{actor, level, newest, Dataset, Session, Window};
use super::runs::repository_id;
use super::{Query, Run};
use crate::util::now_unix;

/** Most rows a distribution (by agent, repository, or task) lists: those with the most actions */
const DISTRIBUTION: usize = 100;

/** Parameters the Autonomy page accepts: the window and the global filters */
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

/** Autonomy levels, from least to most */
const LEVELS: &[&str] = &["observe", "assisted", "delegated", "autonomous"];

/** The documented formulas, returned with the answer */
const DEFINITIONS: &[(&str, &str)] = &[
    ("autonomy_score", "the autonomy level each mutating action was decided at (observe 0, assisted 33.3, delegated 66.7, autonomous 100), averaged over the actions; the level is the run's recorded autonomy state at that moment, replayed through its bound autonomy policy"),
    ("budget_consumed", "points the runs' budgets consumed in the window, replayed from their journals through their bound budget models"),
    ("budget_regeneration", "points the budgets regenerated in the window through controlled events (refills by a human are counted apart)"),
    ("budget_remaining", "available budget of the active runs, of their maximum"),
    ("human_intervention_rate", "runs in which a human approved a tool call or intervened, of the runs"),
    ("denied_action_rate", "mutating actions denied, of the mutating actions decided"),
    ("violation_rate", "runs with at least one violation, of the runs"),
    ("critical_violation_rate", "runs with at least one critical violation, of the runs"),
    ("verification_failure_rate", "runs whose verification failed, of the runs with a verification outcome"),
    ("quarantine_rate", "runs that were quarantined at some point, of the runs"),
    ("autonomous_completion_rate", "completed tasks none of whose runs needed a human approval or intervention, of the completed tasks"),
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

/** Count mutating actions by the autonomy level they were decided at, with their score
 * Input
    - acts: impl Iterator<Item = &Act> - mutating authorization decisions
 * Output
    - Value {observe, assisted, delegated, autonomous, actions, score}
*/
fn distribution<'a>(acts: impl Iterator<Item = &'a Act>) -> Value {
    let mut counts: BTreeMap<&str, u64> = LEVELS.iter().map(|level| (*level, 0)).collect();
    let mut scores = Vec::new();
    for act in acts {
        if let Some(name) = act.autonomy {
            if let Some(count) = counts.get_mut(name) {
                *count += 1;
            }
            scores.extend(level(name));
        }
    }
    let mut value = json!(counts);
    value["actions"] = json!(scores.len());
    value["score"] = average(&scores);
    value
}

/** Answer the Autonomy page
 * Input
    - query: &Query - window and filters
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn overview(query: &Query) -> Result<Value, (u16, String)> {
    let now = now_unix();
    let window = Window::parse(query, now).map_err(|error| (400, error))?;
    let runs = Run::all().map_err(|error| (500, error))?;
    let data = Dataset::new(&runs).map_err(|error| (500, error))?;
    let (matching, tasks) = data.select(query);
    let sessions = data
        .sessions
        .iter()
        .enumerate()
        .filter(|(index, session)| matching.contains(index) && session.during(&window))
        .map(|(_, session)| session)
        .collect::<Vec<_>>();
    let holds = |act: &Act| window.holds(act.at);
    let decisions = sessions
        .iter()
        .flat_map(|session| {
            session
                .run
                .acts
                .iter()
                .filter(|act| act.mutating())
                .map(move |act| (*session, act))
        })
        .filter(|(_, act)| holds(act))
        .collect::<Vec<_>>();

    // Transitions: quarantine and degradation events, and runs ever quarantined
    let mut quarantines = Vec::new();
    let mut degradations = Vec::new();
    for session in &sessions {
        let acts = &session.run.acts;
        for (index, record) in acts.iter().enumerate() {
            let Some(previous) = index.checked_sub(1).map(|previous| &acts[previous]) else {
                continue;
            };
            if !holds(record) || previous.safety == record.safety {
                continue;
            }
            let event = record.extra();
            let entry = json!({
                "event_id": session.run.event_id(record.seq),
                "at": record.at,
                "run_id": session.run.id,
                "agent": actor(&session.run.agent()),
                "agent_id": session.run.agent_id(),
                "task_id": session.attributes.get("task").and_then(|tasks| tasks.iter().next().cloned()),
                "from": {"autonomy": previous.autonomy, "safety": previous.safety},
                "to": {"autonomy": record.autonomy, "safety": record.safety},
                "trigger": event.trigger,
                "kind": event.kind,
                "actor": event.actor,
                "reason": event.reason.map(String::from).or_else(|| event.kind.map(|kind| format!("violation: {kind}"))),
                "link": format!("#/runs/{}?seq={}", session.run.id, record.seq),
            });
            match record.safety {
                Some("quarantined") => quarantines.push(entry),
                Some("degraded") => degradations.push(entry),
                _ => {}
            }
        }
    }
    let quarantined_runs = sessions
        .iter()
        .filter(|session| {
            session
                .run
                .acts
                .iter()
                .any(|act| act.safety == Some("quarantined"))
        })
        .count();

    // Budget, replayed
    let flows = sessions
        .iter()
        .map(|session| session.run.flow.as_slice())
        .collect::<Vec<_>>();
    let in_window = |flow: &[(u64, u64, u64, u64)], pick: fn(&(u64, u64, u64, u64)) -> u64| {
        flow.iter()
            .filter(|entry| window.holds(entry.0))
            .map(pick)
            .sum::<u64>()
    };
    let consumed = flows
        .iter()
        .map(|flow| in_window(flow, |entry| entry.1))
        .sum::<u64>();
    let regenerated = flows
        .iter()
        .map(|flow| in_window(flow, |entry| entry.2))
        .sum::<u64>();
    let refilled = flows
        .iter()
        .map(|flow| in_window(flow, |entry| entry.3))
        .sum::<u64>();
    let active = sessions
        .iter()
        .filter(|session| session.active())
        .collect::<Vec<_>>();
    let remaining = active
        .iter()
        .filter_map(|session| session.run.acts.last().and_then(|act| act.after))
        .fold((0u64, 0u64), |(available, max), budget| {
            let whole = |value: f64| {
                if value.is_finite() && value > 0.0 {
                    value as u64
                } else {
                    0
                }
            };
            (available + whole(budget.available), max + whole(budget.max))
        });

    // Metrics
    let outcome = sessions
        .iter()
        .filter(|session| session.verification != "pending")
        .count();
    let completed = tasks
        .iter()
        .filter(|task| task.completed.is_some_and(|at| window.holds(at)))
        .collect::<Vec<_>>();
    let human_task = |task: &&&super::dashboard::Task| {
        task.sessions
            .iter()
            .any(|index| data.sessions[*index].human)
    };
    let modes = |key: &str, of: &[&&Session]| {
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for session in of {
            if let Some(value) = session.summary.pointer(key).and_then(Value::as_str) {
                *counts.entry(value.to_string()).or_default() += 1;
            }
        }
        json!(counts)
    };
    let scores = decisions
        .iter()
        .filter_map(|(_, act)| act.autonomy.and_then(level))
        .collect::<Vec<_>>();
    let metrics = json!({
        "autonomy_score": average(&scores),
        "autonomy_mode": {"active_runs": modes("/autonomy/current", &active), "runs_in_window": modes("/autonomy/current", &sessions.iter().collect::<Vec<_>>())},
        "safety_state": {"active_runs": modes("/safety/state", &active), "runs_in_window": modes("/safety/state", &sessions.iter().collect::<Vec<_>>())},
        "budget_remaining": {"available": remaining.0, "max": remaining.1, "percent": rate(remaining.0 as usize, remaining.1 as usize)},
        "budget_consumed": consumed,
        "budget_regeneration": regenerated,
        "budget_refilled_by_humans": refilled,
        "human_intervention_rate": rate(sessions.iter().filter(|session| session.human).count(), sessions.len()),
        "denied_action_rate": rate(decisions.iter().filter(|(_, act)| act.decision == Some("deny")).count(), decisions.len()),
        "violation_rate": rate(sessions.iter().filter(|session| !session.violations.is_empty()).count(), sessions.len()),
        "critical_violation_rate": rate(sessions.iter().filter(|session| session.violations.iter().any(|violation| violation["severity"] == "critical")).count(), sessions.len()),
        "verification_failure_rate": rate(sessions.iter().filter(|session| session.verification == "failed").count(), outcome),
        "quarantine_rate": rate(quarantined_runs, sessions.len()),
        "autonomous_completion_rate": rate(completed.iter().filter(|task| !human_task(task)).count(), completed.len()),
        "runs": sessions.len(),
        "mutating_actions": decisions.len(),
    });

    // Series over the window
    let start = window.from - window.from % window.bucket;
    let buckets = (0..)
        .map(|index| start + index * window.bucket)
        .take_while(|at| *at <= window.to)
        .collect::<Vec<_>>();
    let slot = |at: u64| {
        window
            .holds(at)
            .then(|| ((at - start) / window.bucket) as usize)
            .filter(|index| *index < buckets.len())
    };
    let mut score_points: Vec<Vec<f64>> = vec![Vec::new(); buckets.len()];
    for (_, act) in &decisions {
        if let (Some(index), Some(value)) = (slot(act.at), act.autonomy.and_then(level)) {
            score_points[index].push(value);
        }
    }
    let mut budget_points = vec![(0u64, 0u64, 0u64); buckets.len()];
    for flow in &flows {
        for (at, used, regained, refill) in flow.iter() {
            if let Some(index) = slot(*at) {
                budget_points[index].0 += used;
                budget_points[index].1 += regained;
                budget_points[index].2 += refill;
            }
        }
    }
    let series = |values: Vec<Value>| {
        buckets
            .iter()
            .zip(values)
            .map(|(at, value)| json!({"t": at, "v": value}))
            .collect::<Vec<_>>()
    };

    // Distributions by agent, repository, and task
    let group = |key: &dyn Fn(&Session) -> Vec<(String, String)>| {
        let mut groups: BTreeMap<(String, String), Vec<&Act>> = BTreeMap::new();
        // Each session's keys are computed once, not once per decision
        let mut keys: std::collections::HashMap<&str, Vec<(String, String)>> =
            std::collections::HashMap::new();
        for (session, record) in &decisions {
            let names = keys
                .entry(session.run.id.as_str())
                .or_insert_with(|| key(session));
            for name in names.iter() {
                if let Some(list) = groups.get_mut(name) {
                    list.push(record);
                } else {
                    groups.insert(name.clone(), vec![*record]);
                }
            }
        }
        let mut rows = groups
            .into_iter()
            .map(|((name, link), records)| {
                let mut value = distribution(records.into_iter());
                value["name"] = json!(name);
                value["link"] = json!(link);
                value
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| right["actions"].as_u64().cmp(&left["actions"].as_u64()));
        rows
    };
    let by_agent = group(&|session| {
        vec![(
            session.run.agent_id(),
            format!("#/agents/{}", session.run.agent_id()),
        )]
    });
    let by_repository = group(&|session| {
        session
            .attributes
            .get("repository")
            .into_iter()
            .flatten()
            .map(|name| {
                (
                    name.clone(),
                    format!("#/repositories/{}", repository_id(name)),
                )
            })
            .collect()
    });
    let by_task = group(&|session| {
        session
            .attributes
            .get("task")
            .into_iter()
            .flatten()
            .map(|task| (task.clone(), format!("#/tasks/{task}")))
            .collect()
    });

    // Violations and human intervention against the autonomy level actions were decided at
    let mut per_level: BTreeMap<&str, (u64, u64, u64, u64)> =
        LEVELS.iter().map(|level| (*level, (0, 0, 0, 0))).collect();
    for (_, act) in &decisions {
        if let Some(entry) = act.autonomy.and_then(|name| per_level.get_mut(name)) {
            entry.0 += 1;
            entry.1 += u64::from(act.decision == Some("approval_required"));
            entry.2 += u64::from(act.decision == Some("deny"));
        }
    }
    for session in &sessions {
        for violation in session.violations {
            if !window.holds(violation["at"].as_u64().unwrap_or(0)) {
                continue;
            }
            let state = session
                .run
                .acts
                .binary_search_by_key(&violation["seq"].as_u64().unwrap_or(0), |act| act.seq)
                .ok()
                .and_then(|position| session.run.acts[position].autonomy);
            if let Some(entry) = state.and_then(|name| per_level.get_mut(name)) {
                entry.3 += 1;
            }
        }
    }
    let against = LEVELS
        .iter()
        .map(|name| {
            let (actions, asked, denied, violations) = per_level[name];
            json!({
                "autonomy": name,
                "actions": actions,
                "approval_requests": asked,
                "human_intervention_rate": rate(asked as usize, actions as usize),
                "denied": denied,
                "violations": violations,
                "violations_per_100_actions": if actions == 0 { Value::Null } else { json!(((violations as f64 / actions as f64) * 1000.0).round() / 10.0) },
            })
        })
        .collect::<Vec<_>>();
    let mut by_mode: BTreeMap<&str, (usize, usize)> =
        LEVELS.iter().map(|level| (*level, (0, 0))).collect();
    for session in &sessions {
        if let Some(entry) = session.summary["autonomy"]["initial"]
            .as_str()
            .and_then(|name| by_mode.get_mut(name))
        {
            entry.0 += 1;
            entry.1 += usize::from(session.human);
        }
    }
    Ok(json!({
        "now": now,
        "window": {"range": window.name, "from": window.from, "to": window.to, "bucket": window.bucket},
        "metrics": metrics,
        "definitions": DEFINITIONS.iter().map(|(key, text)| (key.to_string(), json!(text))).collect::<serde_json::Map<_, _>>(),
        "score_over_time": series(score_points.iter().map(|values| average(values)).collect()),
        "by_agent_total": by_agent.len(),
        "by_agent": by_agent.into_iter().take(DISTRIBUTION).collect::<Vec<_>>(),
        "by_repository": by_repository.into_iter().take(DISTRIBUTION).collect::<Vec<_>>(),
        "by_task_total": by_task.len(),
        "by_task": by_task.into_iter().take(DISTRIBUTION).collect::<Vec<_>>(),
        "against_autonomy": against,
        "human_by_starting_mode": LEVELS.iter().map(|name| json!({"autonomy": name, "runs": by_mode[name].0, "human_intervention_rate": rate(by_mode[name].1, by_mode[name].0)})).collect::<Vec<_>>(),
        "budget_consumed_over_time": series(budget_points.iter().map(|point| json!(point.0)).collect()),
        "budget_regeneration_over_time": series(budget_points.iter().map(|point| json!(point.1)).collect()),
        "budget_refills_over_time": series(budget_points.iter().map(|point| json!(point.2)).collect()),
        "quarantine_events_total": quarantines.len(),
        "quarantine_events": newest(quarantines, "at").0,
        "degradation_events_total": degradations.len(),
        "degradation_events": newest(degradations, "at").0,
        "read_only": "Autonomy, budgets, and safety states change only through Crane's own commands run by a human (crane autonomy ...); this page observes them",
    }))
}
