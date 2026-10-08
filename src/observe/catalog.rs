// Policies, zones, and violations: read-only catalogs over what governed the runs. Policies come
// from the activation state (the active layers), the retired and proposed policy files, and the
// policies runs were bound to; zones from the zone files and the zones' last resolution, which
// `crane zones` keeps in .crane/runtime/zones/resolution.json (read here, never refreshed, since
// resolving zones again would write); violations from every run's evidence. Nothing here can edit
// a policy or a zone.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use serde_json::{json, Value};

use super::compact::Act;
use super::dashboard::{actor, Dataset, Keys, Session, Window};
use super::detail::{act_resources, session_violations, strings};
use super::runs::repository_id;
use super::{Query, Run, SEVERITIES};
use crate::util::{now_unix, sha256};

/** Parameters the catalog pages accept: the window and the global filters */
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

/** Parameters the violations explorer accepts: the catalog's, plus status, sort, and page */
pub(crate) const VIOLATION_PARAMETERS: &[&str] = &[
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
    "status",
    "page",
    "page_size",
];

/** The global filters that describe a run; policy, zone, and severity are applied to each
 * decision or violation instead, so a run's other actions do not count */
const RUN_FILTERS: &[&str] = &[
    "repository",
    "task",
    "agent",
    "provider",
    "session",
    "policy_version",
    "autonomy",
    "safety",
    "task_status",
    "session_status",
    "verification",
    "delivery",
    "environment",
    "branch",
    "team",
];

/** Everything a catalog page reads, once per request
 * Fields
    - data: Dataset - every session and task
    - window: Window - the time window
    - now: u64 - current time
*/
struct Context<'a> {
    data: Dataset<'a>,
    window: Window,
    now: u64,
}

impl<'a> Context<'a> {
    /** Read the dataset and the window
     * Input
        - runs: &'a [std::sync::Arc<Run>] - every session
        - query: &Query - window
     * Output
        - Result<Context<'a>, (u16, String)>
    */
    fn new(runs: &'a [std::sync::Arc<Run>], query: &Query) -> Result<Self, (u16, String)> {
        let now = now_unix();
        let window = Window::parse(query, now).map_err(|error| (400, error))?;
        let data = Dataset::new(runs).map_err(|error| (500, error))?;
        Ok(Self { data, window, now })
    }

    /** The sessions the run-level filters and the window keep
     * Input
        - query: &Query - filters
     * Output
        - Vec<(usize, &Session)>
    */
    fn sessions(&self, query: &Query) -> Vec<(usize, &Session<'a>)> {
        self.data
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| session.during(&self.window))
            .filter(|(_, session)| {
                RUN_FILTERS.iter().all(|key| {
                    query.set(key).is_none_or(|wanted| {
                        session
                            .attributes
                            .get(key)
                            .is_some_and(|values| values.iter().any(|value| wanted.contains(value)))
                    })
                })
            })
            .collect()
    }

    /** The authorization decisions of some sessions within the window
     * Input
        - sessions: &[(usize, &'b Session)] - sessions
     * Output
        - Vec<(&'b Session, &'b Act)> each decision with its session
    */
    fn decisions<'b>(
        &self,
        sessions: &[(usize, &'b Session<'a>)],
    ) -> Vec<(&'b Session<'a>, &'b Act)> {
        sessions
            .iter()
            .flat_map(|(_, session)| {
                session
                    .run
                    .acts
                    .iter()
                    .filter(|act| act.category == Some("authorization"))
                    .map(move |act| (*session, act))
            })
            .filter(|(_, act)| self.window.holds(act.at))
            .collect()
    }

    /** The tasks some sessions worked on
     * Input
        - indices: &BTreeSet<usize> - session indices
     * Output
        - Vec<&Task>
    */
    fn tasks(&self, indices: &BTreeSet<usize>) -> Vec<&super::dashboard::Task> {
        self.data
            .tasks
            .iter()
            .filter(|task| task.sessions.iter().any(|index| indices.contains(index)))
            .collect()
    }
}

/** Most events a policy or zone timeline lists (the newest; the total is reported beside it) */
const TIMELINE: usize = 500;

/** The newest decisions, at most TIMELINE of them, without sorting all of them
 * Input
    - decisions: &[(&Session, &Act)] - decisions
 * Output
    - impl Iterator<Item = (&Session, &Act)>
*/
fn newest<'b, 'a>(
    decisions: &[(&'b Session<'a>, &'b Act)],
) -> impl Iterator<Item = (&'b Session<'a>, &'b Act)> {
    let mut chosen = decisions.to_vec();
    if chosen.len() > TIMELINE {
        chosen.select_nth_unstable_by_key(TIMELINE, |(_, act)| std::cmp::Reverse(act.at));
        chosen.truncate(TIMELINE);
    }
    chosen.into_iter()
}

/** Check whether a policy label ("payments" or "payments/preserve") names a policy
 * Input
    - label: &str - label
    - policy: &str - policy id
 * Output
    - bool
*/
fn names(label: &str, policy: &str) -> bool {
    label == policy || label.split('/').next() == Some(policy)
}

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

/** Count values into a list of {name, count}, largest first
 * Input
    - values: impl Iterator<Item = String> - values
    - key: &str - name of the value field
    - limit: usize - most entries
 * Output
    - Vec<Value>
*/
fn top(values: impl Iterator<Item = String>, key: &str, limit: usize) -> Vec<Value> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for value in values {
        *counts.entry(value).or_default() += 1;
    }
    let mut sorted = counts.into_iter().collect::<Vec<_>>();
    sorted.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    sorted
        .into_iter()
        .take(limit)
        .map(|(name, count)| json!({key: name, "count": count}))
        .collect()
}

/** The policies to list: active ones with their layer, retired and proposed policy files, and
 * every policy a run was bound to
 * Input
    - data: &Dataset - every session
 * Output
    - Result<BTreeMap<String, Value>, String> policy id to its metadata
*/
fn known_policies(data: &Dataset) -> Result<BTreeMap<String, Value>, String> {
    let mut policies: BTreeMap<String, Value> = BTreeMap::new();
    let state = crate::task_contracts::activation::current()?;
    for policy in state["policies"].as_array().into_iter().flatten() {
        if let Some(name) = policy["policy"].as_str() {
            policies.insert(name.to_string(), json!({"status": "active", "type": policy["layer"], "file": format!(".crane/{}", policy["file"].as_str().unwrap_or_default()), "digest": policy["digest"], "origin": policy["origin"], "checkpoint": policy["checkpoint"], "rules": policy["rules"], "malformed": policy["malformed"]}));
        }
    }
    let crane = super::crane_root()?;
    for (status, folder) in [("retired", "retired"), ("proposed", "proposals")] {
        for entry in fs::read_dir(crane.join(folder))
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".crane"))
                .map(String::from)
            else {
                continue;
            };
            if policies.contains_key(&name) {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap_or_default();
            policies.insert(name.clone(), json!({"status": status, "type": if name.starts_with("task_") { "task" } else { "repository" }, "file": format!(".crane/{folder}/{name}.crane"), "digest": sha256(text.as_bytes())}));
        }
    }
    for session in &data.sessions {
        for contract in session.run.contracts().as_array().into_iter().flatten() {
            if let Some(name) = contract["policy_id"].as_str() {
                policies.entry(name.to_string()).or_insert_with(|| {
                    json!({"status": "bound only", "type": if name.starts_with("task_") { "task" } else { "repository" }, "file": null, "digest": contract["contract_hash"]})
                });
            }
        }
    }
    Ok(policies)
}

/** Read a policy's text from wherever it is stored
 * Input
    - metadata: &Value - its metadata from known_policies
 * Output
    - Option<String>
*/
fn policy_text(metadata: &Value) -> Option<String> {
    let file = metadata["file"].as_str()?.strip_prefix(".crane/")?;
    fs::read_to_string(super::crane_root().ok()?.join(file)).ok()
}

/** How runs and their decisions met one policy
 * Fields
    - bound: Vec<(usize, &Session)> - runs bound to it
    - decisions: Vec<(&Session, &Act)> - decisions it took part in
    - violations: Vec<Value> - violations attributed to it
*/
struct Usage<'a, 'b> {
    bound: Vec<(usize, &'b Session<'a>)>,
    decisions: Vec<(&'b Session<'a>, &'b Act)>,
    violations: Vec<Value>,
}

/** Work out how runs met one policy
 * Input
    - context: &'b Context - dataset and window
    - sessions: &[(usize, &'b Session)] - the runs the filters keep
    - id: &str - policy id
 * Output
    - Usage
*/
fn usage<'a, 'b>(
    context: &'b Context<'a>,
    sessions: &[(usize, &'b Session<'a>)],
    id: &str,
) -> Usage<'a, 'b> {
    let bound = sessions
        .iter()
        .filter(|(_, session)| {
            session
                .attributes
                .get("policy")
                .is_some_and(|policies| policies.contains(id))
        })
        .copied()
        .collect::<Vec<_>>();
    let decisions = context
        .decisions(sessions)
        .into_iter()
        .filter(|(_, record)| record.policies.contains(&id))
        .collect::<Vec<_>>();
    let violations = sessions
        .iter()
        .flat_map(|(_, session)| session_violations(session))
        .filter(|violation| context.window.holds(violation["at"].as_u64().unwrap_or(0)))
        .filter(|violation| {
            strings(&violation["policy"])
                .iter()
                .any(|label| names(label, id))
        })
        .collect::<Vec<_>>();
    Usage {
        bound,
        decisions,
        violations,
    }
}

/** Answer the Policies page: one row per policy with how runs met it in the window
 * Input
    - query: &Query - window and filters
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn policies(query: &Query) -> Result<Value, (u16, String)> {
    let runs = Run::all().map_err(|error| (500, error))?;
    let context = Context::new(&runs, query)?;
    let sessions = context.sessions(query);
    let known = known_policies(&context.data).map_err(|error| (500, error))?;
    let (zones, _) = crate::zones::model::load(&super::crane_root().map_err(|error| (500, error))?)
        .map_err(|error| (500, error))?;
    let wanted = query.set("policy");
    let rows = known
        .iter()
        .filter(|(id, _)| wanted.as_ref().is_none_or(|wanted| wanted.contains(*id)))
        .map(|(id, metadata)| {
            let used = usage(&context, &sessions, id);
            let indices = used.bound.iter().map(|(index, _)| *index).collect::<BTreeSet<_>>();
            let count = |decision: &str| used.decisions.iter().filter(|(_, record)| record.decision == Some(decision)).count();
            let passed = used.bound.iter().filter(|(_, session)| session.verification == "passed").count();
            let failed = used.bound.iter().filter(|(_, session)| session.verification == "failed").count();
            let zone_scope = used
                .decisions
                .iter()
                .flat_map(|(_, record)| record.zones.iter().map(|zone| zone.to_string()))
                .chain(zones.iter().filter(|zone| zone.policy_reference.as_deref() == Some(id.as_str())).map(|zone| zone.zone_id.clone()))
                .collect::<BTreeSet<_>>();
            json!({
                "policy_id": id,
                "name": id,
                "status": metadata["status"],
                "type": metadata["type"],
                "version": metadata["digest"],
                "versions_bound": used.bound.iter().filter_map(|(_, session)| session.run.contracts().as_array()?.iter().find(|contract| contract["policy_id"] == id.as_str()).and_then(|contract| contract["contract_hash"].as_str().map(String::from))).collect::<BTreeSet<_>>(),
                "repository_scope": used.bound.iter().flat_map(|(_, session)| session.attributes.get("repository").cloned().unwrap_or_default()).collect::<BTreeSet<_>>(),
                "zone_scope": zone_scope,
                "tasks": context.tasks(&indices).len(),
                "sessions": used.bound.len(),
                "allowed": count("allow"),
                "denied": count("deny"),
                "approval_required": count("approval_required"),
                "violations": used.violations.len(),
                "critical_violations": used.violations.iter().filter(|violation| violation["severity"] == "critical").count(),
                "last_triggered": used.decisions.iter().map(|(_, record)| record.at).max(),
                "success_rate": rate(passed, passed + failed),
                "link": format!("#/policies/{id}"),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "now": context.now,
        "window": {"range": context.window.name, "from": context.window.from, "to": context.window.to},
        "policies": rows,
        "success_rate_definition": "runs bound to the policy whose verification passed, of those with a verification outcome",
    }))
}

/** Answer a policy's page: metadata, version and digest, its rules as semantic selectors, the
 * repositories, zones, tasks, and agents it affected, how often it was evaluated, denied, and
 * violated, and its timeline
 * Input
    - id: &str - policy id (validated)
    - query: &Query - window and filters
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn policy(id: &str, query: &Query) -> Result<Value, (u16, String)> {
    let runs = Run::all().map_err(|error| (500, error))?;
    let context = Context::new(&runs, query)?;
    let known = known_policies(&context.data).map_err(|error| (500, error))?;
    let metadata = known
        .get(id)
        .ok_or_else(|| (404, format!("no policy '{id}'")))?;
    let sessions = context.sessions(query);
    let used = usage(&context, &sessions, id);
    let text = policy_text(metadata);
    let selectors = text.as_deref().map(crate::policy::parse).map(|parsed| match parsed {
        Ok(policy) => json!({"checkpoint": policy.checkpoint, "rules": policy.rules.iter().map(|rule| rule.describe()).collect::<Vec<_>>()}),
        Err(error) => json!({"error": error}),
    });
    let (zones, _) = crate::zones::model::load(&super::crane_root().map_err(|error| (500, error))?)
        .map_err(|error| (500, error))?;
    let indices = used
        .bound
        .iter()
        .map(|(index, _)| *index)
        .collect::<BTreeSet<_>>();
    let mut by_task: BTreeMap<String, BTreeMap<&str, u64>> = BTreeMap::new();
    let mut by_agent: BTreeMap<String, BTreeMap<&str, u64>> = BTreeMap::new();
    for (session, record) in &used.decisions {
        let decision = match record.decision {
            Some("allow") => "allow",
            Some("deny") => "deny",
            _ => "approval_required",
        };
        for task in session.attributes.get("task").into_iter().flatten() {
            *by_task
                .entry(task.clone())
                .or_default()
                .entry(decision)
                .or_default() += 1;
        }
        *by_agent
            .entry(session.run.agent_id())
            .or_default()
            .entry(decision)
            .or_default() += 1;
    }
    let distribution = |map: BTreeMap<String, BTreeMap<&str, u64>>, key: &str, link: &str| {
        map.into_iter()
            .map(|(name, counts)| {
                json!({key: name, "allow": counts.get("allow").copied().unwrap_or(0), "deny": counts.get("deny").copied().unwrap_or(0), "approval_required": counts.get("approval_required").copied().unwrap_or(0), "link": format!("#/{link}/{name}")})
            })
            .collect::<Vec<_>>()
    };
    let mut timeline = newest(&used.decisions)
        .map(|(session, record)| {
            json!({"event_id": session.run.event_id(record.seq), "at": record.at, "kind": "decision", "run_id": session.run.id, "agent": actor(&session.run.agent()), "outcome": record.decision, "resource": act_resources(record), "zone": record.zones, "detail": record.reasons.join(" "), "link": format!("#/runs/{}?seq={}", session.run.id, record.seq)})
        })
        .chain(used.violations.iter().map(|violation| {
            json!({"at": violation["at"], "kind": "violation", "run_id": violation["session_id"], "outcome": violation["severity"], "resource": violation["resource"], "zone": violation["zone"], "detail": format!("{}: {}", violation["kind"].as_str().unwrap_or_default(), violation["consequence"].as_str().unwrap_or_default()), "link": violation["link"]})
        }))
        .chain(used.bound.iter().map(|(_, session)| {
            json!({"at": session.started, "kind": "bound", "run_id": session.run.id, "agent": actor(&session.run.agent()), "outcome": "bound", "detail": "a run started bound to this policy", "link": format!("#/runs/{}", session.run.id)})
        }))
        .collect::<Vec<_>>();
    timeline.sort_by_key(|event| std::cmp::Reverse(event["at"].as_u64().unwrap_or(0)));
    let timeline_total = used.decisions.len() + used.violations.len() + used.bound.len();
    timeline.truncate(TIMELINE);
    let count = |decision: &str| {
        used.decisions
            .iter()
            .filter(|(_, record)| record.decision == Some(decision))
            .count()
    };
    Ok(json!({
        "now": context.now,
        "window": {"range": context.window.name, "from": context.window.from, "to": context.window.to},
        "policy_id": id,
        "metadata": metadata,
        "version": metadata["digest"],
        "digest": text.as_deref().map(|text| sha256(text.as_bytes())).or_else(|| metadata["digest"].as_str().map(String::from)),
        "versions_bound": used.bound.iter().filter_map(|(_, session)| session.run.contracts().as_array()?.iter().find(|contract| contract["policy_id"] == id).and_then(|contract| contract["contract_hash"].as_str().map(String::from))).collect::<BTreeSet<_>>(),
        "definition": text,
        "selectors": selectors,
        "repositories": used.bound.iter().flat_map(|(_, session)| session.attributes.get("repository").cloned().unwrap_or_default()).collect::<BTreeSet<_>>().into_iter().map(|name| json!({"repository": name.clone(), "link": format!("#/repositories/{}", repository_id(&name))})).collect::<Vec<_>>(),
        "zones": used.decisions.iter().flat_map(|(_, record)| record.zones.iter().map(|zone| zone.to_string())).chain(zones.iter().filter(|zone| zone.policy_reference.as_deref() == Some(id)).map(|zone| zone.zone_id.clone())).collect::<BTreeSet<_>>().into_iter().map(|zone| json!({"zone": zone.clone(), "link": format!("#/zones/{zone}")})).collect::<Vec<_>>(),
        "counts": {
            "runs_bound": used.bound.len(),
            "executions": used.decisions.len(),
            "allowed": count("allow"),
            "denials": count("deny"),
            "approval_required": count("approval_required"),
            "violations": used.violations.len(),
            "critical_violations": used.violations.iter().filter(|violation| violation["severity"] == "critical").count(),
        },
        "false_positives": {"available": false, "note": "Crane records no false-positive classification of decisions; a denial a human later overrides through an approval or exception appears in the run's timeline"},
        "tasks": context.tasks(&indices).iter().map(|task| json!({"task_id": task.id, "status": Keys::new(task, &context.data, context.now).status, "link": format!("#/tasks/{}", task.id)})).collect::<Vec<_>>(),
        "task_distribution": distribution(by_task, "task_id", "tasks"),
        "agent_distribution": distribution(by_agent, "agent_id", "agents"),
        "runs": used.bound.iter().map(|(_, session)| json!({"run_id": session.run.id, "agent": actor(&session.run.agent()), "status": session.summary["lifecycle"], "start": session.started, "verification": session.verification, "link": format!("#/runs/{}", session.run.id)})).collect::<Vec<_>>(),
        "violations": used.violations,
        "timeline": timeline,
        "timeline_total": timeline_total,
    }))
}

/** Read the zones' last resolution kept by `crane zones` (never refreshed here)
 * Input
    - None
 * Output
    - Value {zone id: {selector: {targets, files, symbols}}}, null when there is none
*/
fn last_resolution() -> Value {
    super::crane_root()
        .ok()
        .and_then(|crane| {
            fs::read_to_string(crane.join("runtime").join("zones").join("resolution.json")).ok()
        })
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .map_or(Value::Null, |state| state["zones"].clone())
}

/** Describe a zone's resolved resources from its last resolution
 * Input
    - zone: &crate::zones::model::Zone - zone
    - resolution: &Value - its last resolution
 * Output
    - Value {selectors: [{selector, resolved, targets, files, symbols}], files, symbols}
*/
fn zone_resources(zone: &crate::zones::model::Zone, resolution: &Value) -> Value {
    let mut files = BTreeSet::new();
    let mut symbols = BTreeSet::new();
    let selectors = zone
        .selectors
        .iter()
        .map(|selector| {
            let text = selector.text();
            let record = &resolution[&text];
            let selector_files = record["files"].as_object().map(|files| files.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
            let selector_symbols = record["symbols"].as_array().into_iter().flatten().filter_map(|symbol| symbol["id"].as_str().map(String::from)).collect::<Vec<_>>();
            files.extend(selector_files.iter().cloned());
            symbols.extend(selector_symbols.iter().cloned());
            json!({"selector": text, "semantic": selector.kind.semantic(), "resolved": !record.is_null(), "targets": record["targets"], "files": selector_files, "symbols": selector_symbols})
        })
        .collect::<Vec<_>>();
    json!({"selectors": selectors, "files": files, "symbols": symbols})
}

/** How runs met one zone: the active runs it constrained, the decisions it took part in, and the
 * violations in it
 * Input
    - context: &'b Context - dataset and window
    - sessions: &[(usize, &'b Session)] - runs the filters keep
    - id: &str - zone id
 * Output
    - (Vec<&Session>, Vec<(&Session, &Act)>, Vec<Value>)
*/
#[allow(clippy::type_complexity)]
fn zone_usage<'a, 'b>(
    context: &'b Context<'a>,
    sessions: &[(usize, &'b Session<'a>)],
    id: &str,
) -> (
    Vec<&'b Session<'a>>,
    Vec<(&'b Session<'a>, &'b Act)>,
    Vec<Value>,
) {
    let constrained = sessions
        .iter()
        .filter(|(_, session)| {
            session.run.governance()["files"]
                .as_object()
                .is_some_and(|files| {
                    files.values().any(|constraint| {
                        strings(&constraint["zones"]).iter().any(|zone| zone == id)
                            || constraint["symbols"].as_object().is_some_and(|symbols| {
                                symbols.values().any(|symbol| {
                                    strings(&symbol["zones"]).iter().any(|zone| zone == id)
                                })
                            })
                    })
                })
        })
        .map(|(_, session)| *session)
        .collect::<Vec<_>>();
    let decisions = context
        .decisions(sessions)
        .into_iter()
        .filter(|(_, record)| record.zones.contains(&id))
        .collect::<Vec<_>>();
    let violations = sessions
        .iter()
        .flat_map(|(_, session)| session_violations(session))
        .filter(|violation| context.window.holds(violation["at"].as_u64().unwrap_or(0)))
        .filter(|violation| strings(&violation["zone"]).iter().any(|zone| zone == id))
        .collect::<Vec<_>>();
    (constrained, decisions, violations)
}

/** Answer the Zones page: one row per zone with its resources and how runs met it
 * Input
    - query: &Query - window and filters
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn zones(query: &Query) -> Result<Value, (u16, String)> {
    let runs = Run::all().map_err(|error| (500, error))?;
    let context = Context::new(&runs, query)?;
    let sessions = context.sessions(query);
    let crane = super::crane_root().map_err(|error| (500, error))?;
    let (zones, problems) = crate::zones::model::load(&crane).map_err(|error| (500, error))?;
    let resolution = last_resolution();
    let wanted = query.set("zone");
    let rows = zones
        .iter()
        .filter(|zone| wanted.as_ref().is_none_or(|wanted| wanted.contains(&zone.zone_id)))
        .map(|zone| {
            let resources = zone_resources(zone, &resolution[&zone.zone_id]);
            let (constrained, decisions, violations) = zone_usage(&context, &sessions, &zone.zone_id);
            let indices = decisions
                .iter()
                .filter_map(|(session, _)| context.data.sessions.iter().position(|candidate| candidate.run.id == session.run.id))
                .collect::<BTreeSet<_>>();
            let unresolved = resources["selectors"].as_array().into_iter().flatten().any(|selector| selector["resolved"] == false);
            let state = if unresolved { zone.safety_state.max(crate::zones::model::SafetyState::Degraded) } else { zone.safety_state };
            let effective = zone.default_autonomy.min(zone.criticality.autonomy_cap()).min(state.autonomy_cap());
            json!({
                "zone_id": zone.zone_id,
                "criticality": zone.criticality.name(),
                "repository": context.data.repository,
                "repository_id": repository_id(&context.data.repository),
                "resources": {"files": resources["files"].as_array().map_or(0, Vec::len), "symbols": resources["symbols"].as_array().map_or(0, Vec::len), "selectors": zone.selectors.len(), "unresolved": unresolved},
                "active_sessions": constrained.iter().filter(|session| session.active()).count(),
                "tasks": context.tasks(&indices).len(),
                "decisions": decisions.len(),
                "violations": violations.len(),
                "autonomy": {"declared": zone.default_autonomy.name(), "effective": effective.name()},
                "state": state.name(),
                "policy": zone.policy_reference,
                "source": zone.source,
                "last_triggered": decisions.iter().map(|(_, record)| record.at).max(),
                "link": format!("#/zones/{}", zone.zone_id),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "now": context.now,
        "window": {"range": context.window.name, "from": context.window.from, "to": context.window.to},
        "zones": rows,
        "problems": problems,
        "resolution_known": !resolution.is_null(),
    }))
}

/** Answer a zone's page: its definition, its semantic resources as last resolved, and its
 * history: decisions, violations, the runs it constrained, and its review audit events
 * Input
    - id: &str - zone id (validated)
    - query: &Query - window and filters
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn zone(id: &str, query: &Query) -> Result<Value, (u16, String)> {
    let runs = Run::all().map_err(|error| (500, error))?;
    let context = Context::new(&runs, query)?;
    let crane = super::crane_root().map_err(|error| (500, error))?;
    let (zones, _) = crate::zones::model::load(&crane).map_err(|error| (500, error))?;
    let zone = zones
        .iter()
        .find(|zone| zone.zone_id == id)
        .ok_or_else(|| (404, format!("no zone '{id}'")))?;
    let resolution = last_resolution();
    let resolved = zone_resources(zone, &resolution[id]);
    let sessions = context.sessions(query);
    let (constrained, decisions, violations) = zone_usage(&context, &sessions, id);
    let audit = crate::zones::review::audit_events()
        .unwrap_or_default()
        .into_iter()
        .filter(|event| event["zone_id"] == id || (event["recommendation"] == "map" && id.starts_with("map_")))
        .map(|event| json!({"at": event["at"], "event": event["event"], "by": event["by"], "revision": event["revision"], "digest": event["digest"]}))
        .collect::<Vec<_>>();
    let mut history = newest(&decisions)
        .map(|(session, record)| json!({"event_id": session.run.event_id(record.seq), "at": record.at, "kind": "decision", "run_id": session.run.id, "agent": actor(&session.run.agent()), "outcome": record.decision, "resource": act_resources(record), "policy": record.policies, "link": format!("#/runs/{}?seq={}", session.run.id, record.seq)}))
        .chain(violations.iter().map(|violation| json!({"at": violation["at"], "kind": "violation", "run_id": violation["session_id"], "outcome": violation["severity"], "resource": violation["resource"], "policy": violation["policy"], "detail": violation["consequence"], "link": violation["link"]})))
        .chain(audit.iter().map(|event| json!({"at": event["at"], "kind": "review", "outcome": event["event"], "detail": format!("by {}", event["by"].as_str().unwrap_or("unknown"))})))
        .collect::<Vec<_>>();
    history.sort_by_key(|event| std::cmp::Reverse(event["at"].as_u64().unwrap_or(0)));
    let history_total = decisions.len() + violations.len() + audit.len();
    history.truncate(TIMELINE);
    Ok(json!({
        "now": context.now,
        "window": {"range": context.window.name, "from": context.window.from, "to": context.window.to},
        "zone_id": id,
        "definition": {
            "criticality": zone.criticality.name(),
            "autonomy": zone.default_autonomy.name(),
            "effective_autonomy": zone.default_autonomy.min(zone.criticality.autonomy_cap()).name(),
            "state": zone.safety_state.name(),
            "policy": zone.policy_reference,
            "source": zone.source,
            "version": zone.version,
        },
        "repository": {"name": context.data.repository, "link": format!("#/repositories/{}", repository_id(&context.data.repository))},
        "resources": resolved,
        "resolution_known": !resolution.is_null(),
        "runs": constrained.iter().map(|session| json!({"run_id": session.run.id, "agent": actor(&session.run.agent()), "status": session.summary["lifecycle"], "start": session.started, "link": format!("#/runs/{}", session.run.id)})).collect::<Vec<_>>(),
        "counts": {"decisions": decisions.len(), "allowed": decisions.iter().filter(|(_, record)| record.decision == Some("allow")).count(), "denied": decisions.iter().filter(|(_, record)| record.decision == Some("deny")).count(), "approval_required": decisions.iter().filter(|(_, record)| record.decision == Some("approval_required")).count(), "violations": violations.len()},
        "violations": violations,
        "history": history,
        "history_total": history_total,
        "reviews": audit,
    }))
}

/** Build one row of the violations explorer
 * Input
    - violation: Value - violation from detail::session_violations
    - session: &Session - its run
    - repository: &str - repository name
 * Output
    - Value
*/
fn violation_row(mut violation: Value, session: &Session, repository: &str) -> Value {
    let tasks = session.attributes.get("task").cloned().unwrap_or_default();
    let before = &violation["previous_state"];
    let after = &violation["resulting_state"];
    let impact = if before.is_null()
        || (before["autonomy"] == after["autonomy"] && before["safety"] == after["safety"])
    {
        "none".to_string()
    } else {
        format!(
            "{} / {} → {} / {}",
            before["autonomy"].as_str().unwrap_or_default(),
            before["safety"].as_str().unwrap_or_default(),
            after["autonomy"].as_str().unwrap_or_default(),
            after["safety"].as_str().unwrap_or_default()
        )
    };
    violation["task_id"] = json!(tasks.iter().next());
    violation["run_id"] = json!(session.run.id);
    violation["agent"] = json!(actor(&session.run.agent()));
    violation["agent_id"] = json!(session.run.agent_id());
    violation["provider"] = json!(session.run.provider());
    violation["repository"] = json!(repository);
    violation["repository_id"] = json!(repository_id(repository));
    violation["autonomy_impact"] = json!(impact);
    violation["budget_impact"] = violation["budget"]["change"].clone();
    violation["resolution"] = if violation["resolved"] == true {
        json!(format!(
            "resolved: {}",
            violation["resolved_by"]["how"].as_str().unwrap_or("repair")
        ))
    } else {
        json!("unresolved")
    };
    violation
}

/** Answer the violations explorer: every violation the window and filters keep (severity, policy,
 * and zone filter the violation itself; status is resolved or unresolved), newest first and
 * paginated, with the severity trend, the top policies, agents, and repositories, and the
 * critical count
 * Input
    - query: &Query - window, filters, status, page, page_size
 * Output
    - Result<Value, (u16, String)>
*/
pub(crate) fn violations(query: &Query) -> Result<Value, (u16, String)> {
    query
        .choice("status", &["resolved", "unresolved"])
        .map_err(|error| (400, error))?;
    let number = |key: &str, default: usize, max: usize| match query.get(key) {
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
    };
    let page_size = number("page_size", 50, 200)?;
    let page = number("page", 1, 1_000_000)?;
    let runs = Run::all().map_err(|error| (500, error))?;
    let context = Context::new(&runs, query)?;
    let sessions = context.sessions(query);
    let severity = query.set("severity");
    let policies = query.set("policy");
    let zones = query.set("zone");
    let status = query.get("status");
    let repository = &context.data.repository;
    let mut rows = sessions
        .iter()
        .flat_map(|(_, session)| {
            session_violations(session)
                .into_iter()
                .map(move |violation| violation_row(violation, session, repository))
        })
        .filter(|violation| context.window.holds(violation["at"].as_u64().unwrap_or(0)))
        .filter(|violation| {
            severity.as_ref().is_none_or(|wanted| {
                violation["severity"]
                    .as_str()
                    .is_some_and(|value| wanted.contains(value))
            })
        })
        .filter(|violation| {
            policies.as_ref().is_none_or(|wanted| {
                strings(&violation["policy"])
                    .iter()
                    .any(|label| wanted.iter().any(|policy| names(label, policy)))
            })
        })
        .filter(|violation| {
            zones.as_ref().is_none_or(|wanted| {
                strings(&violation["zone"])
                    .iter()
                    .any(|zone| wanted.contains(zone))
            })
        })
        .filter(|violation| {
            status.is_none_or(|status| (violation["resolved"] == true) == (status == "resolved"))
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|violation| {
        std::cmp::Reverse((
            violation["at"].as_u64().unwrap_or(0),
            violation["seq"].as_u64().unwrap_or(0),
        ))
    });
    // Severity trend over the window
    let window = &context.window;
    let start = window.from - window.from % window.bucket;
    let buckets = (0..)
        .map(|index| start + index * window.bucket)
        .take_while(|at| *at <= window.to)
        .collect::<Vec<_>>();
    let mut trend = buckets
        .iter()
        .map(|at| json!({"t": at, "critical": 0, "high": 0, "medium": 0, "low": 0}))
        .collect::<Vec<_>>();
    for violation in &rows {
        let at = violation["at"].as_u64().unwrap_or(0);
        let index = ((at.max(start) - start) / window.bucket) as usize;
        if let (Some(point), Some(level)) = (trend.get_mut(index), violation["severity"].as_str()) {
            point[level] = json!(point[level].as_u64().unwrap_or(0) + 1);
        }
    }
    let total = rows.len();
    let count = |level: &str| {
        rows.iter()
            .filter(|violation| violation["severity"] == level)
            .count()
    };
    let summary = json!({
        "total": total,
        "critical": count("critical"),
        "high": count("high"),
        "medium": count("medium"),
        "low": count("low"),
        "resolved": rows.iter().filter(|violation| violation["resolved"] == true).count(),
        "unresolved": rows.iter().filter(|violation| violation["resolved"] != true).count(),
    });
    let top_policies = top(
        rows.iter().flat_map(|violation| {
            strings(&violation["policy"])
                .into_iter()
                .map(|label| label.split('/').next().unwrap_or_default().to_string())
                .collect::<BTreeSet<_>>()
        }),
        "policy_id",
        10,
    );
    let top_agents = top(
        rows.iter()
            .filter_map(|violation| violation["agent_id"].as_str().map(String::from)),
        "agent_id",
        10,
    );
    let top_repositories = top(
        rows.iter()
            .filter_map(|violation| violation["repository"].as_str().map(String::from)),
        "repository",
        10,
    );
    let mut facets: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for session in &context.data.sessions {
        for (key, values) in &session.attributes {
            facets
                .entry(key)
                .or_default()
                .extend(values.iter().cloned());
        }
    }
    facets
        .entry("severity")
        .or_default()
        .extend(SEVERITIES.iter().map(|value| value.to_string()));
    let items = rows
        .into_iter()
        .skip((page - 1).saturating_mul(page_size))
        .take(page_size)
        .collect::<Vec<_>>();
    Ok(json!({
        "now": context.now,
        "window": {"range": window.name, "from": window.from, "to": window.to, "bucket": window.bucket},
        "summary": summary,
        "trend": trend,
        "top_policies": top_policies,
        "top_agents": top_agents,
        "top_repositories": top_repositories,
        "total": total,
        "page": page,
        "pages": total.div_ceil(page_size).max(1),
        "page_size": page_size,
        "items": items,
        "facets": super::dashboard::bounded(facets).0,
        "severity_definition": "critical: the run's autonomy policy treats the kind as critical (it quarantines); high: the autonomy state machine acted on it (it degrades); medium: found when verifying an executed tool; low: a final-reconciliation finding",
    }))
}
