// The global Overview of the observability dashboard: one read-only query that applies the time
// window and the global filters to the same projections the other endpoints use (sessions, tasks,
// evidence records, deliveries, completions), and answers the headline metrics, the trend series,
// what is active now, the recent activity stream, and the values each filter can take. Every
// metric carries its definition, so the page can explain it.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use std::sync::Arc;

use super::compact::Act;
use super::{Query, Run};
use crate::util::now_unix;

/** Parameters the Overview accepts */
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
    "limit",
];

/** The filters that describe a session (task_status through its task) */
const SESSION_FILTERS: &[&str] = &[
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

/** The filters a task answers by itself; every other filter needs one of its sessions to match */
const TASK_FILTERS: &[&str] = &["repository", "task", "task_status"];

/** Preset windows: name, length in seconds, and bucket size of their trends */
const RANGES: &[(&str, u64, u64)] = &[
    ("15m", 900, 60),
    ("1h", 3_600, 300),
    ("6h", 21_600, 900),
    ("24h", 86_400, 3_600),
    ("7d", 604_800, 21_600),
    ("30d", 2_592_000, 86_400),
];

/** Longest custom window, in seconds (a year and a day), so no query spans unbounded history */
pub(super) const MAX_SPAN: u64 = 366 * 86_400;

/** Task states that end a task */
pub(super) const TERMINAL: &[&str] = &["COMPLETED", "FAILED", "CANCELLED"];

/** The time window of a request
 * Fields
    - from: u64 - start, Unix seconds (inclusive)
    - to: u64 - end, Unix seconds (inclusive)
    - bucket: u64 - trend bucket size in seconds
    - name: String - preset name or "custom"
*/
pub(super) struct Window {
    pub(super) from: u64,
    pub(super) to: u64,
    pub(super) bucket: u64,
    pub(super) name: String,
}

impl Window {
    /** Read the window from the query: a preset range (default 24h) ending now or at "to", or a
     * custom range given by "from" and "to"
     * Input
        - query: &Query - parameters
        - now: u64 - current time
     * Output
        - Result<Window, String>
    */
    pub(super) fn parse(query: &Query, now: u64) -> Result<Self, String> {
        let number = |key: &str| {
            query
                .get(key)
                .map(|text| {
                    text.parse::<u64>()
                        .map_err(|_| format!("{key} must be Unix seconds, not '{text}'"))
                })
                .transpose()
        };
        let to = number("to")?.unwrap_or(now);
        if to > now + 86_400 {
            return Err("to may not be more than a day in the future".into());
        }
        let range = query
            .get("range")
            .unwrap_or(if query.get("from").is_some() {
                "custom"
            } else {
                "24h"
            });
        if range == "custom" {
            let from = number("from")?
                .ok_or("a custom range needs from (and optionally to), in Unix seconds")?;
            if from >= to {
                return Err("from must be before to".into());
            }
            if to - from > MAX_SPAN {
                return Err(format!(
                    "a custom range spans at most {} days",
                    MAX_SPAN / 86_400
                ));
            }
            let bucket = ((to - from) / 30).max(60).div_ceil(60) * 60;
            return Ok(Self {
                from,
                to,
                bucket,
                name: "custom".into(),
            });
        }
        if query.get("from").is_some() {
            return Err("from is only accepted with range=custom".into());
        }
        let (name, length, bucket) = RANGES
            .iter()
            .find(|(name, _, _)| *name == range)
            .ok_or_else(|| {
                format!("range must be one of 15m, 1h, 6h, 24h, 7d, 30d, custom, not '{range}'")
            })?;
        Ok(Self {
            from: to.saturating_sub(*length),
            to,
            bucket: *bucket,
            name: (*name).into(),
        })
    }

    /** Check whether a time is in the window
     * Input
        - at: u64 - Unix seconds
     * Output
        - bool
    */
    pub(super) fn holds(&self, at: u64) -> bool {
        (self.from..=self.to).contains(&at)
    }

    /** Return the start of every trend bucket
     * Input
        - None (uses self)
     * Output
        - Vec<u64>
    */
    fn buckets(&self) -> Vec<u64> {
        let start = self.from - self.from % self.bucket;
        (0..)
            .map(|index| start + index * self.bucket)
            .take_while(|at| *at <= self.to)
            .collect()
    }

    /** Return the index of the bucket holding a time
     * Input
        - at: u64 - Unix seconds within the window
     * Output
        - usize
    */
    fn bucket_of(&self, at: u64) -> usize {
        ((at - (self.from - self.from % self.bucket)) / self.bucket) as usize
    }
}

/** Name the actor of an agent profile as people know it
 * Input
    - agent: &str - agent profile
 * Output
    - &'static str
*/
pub(super) fn actor(agent: &str) -> &'static str {
    match agent {
        "claude" => "Claude Code",
        "codex" => "Codex",
        _ => "Generic agent",
    }
}

/** Numeric value of an autonomy level for the autonomy score
 * Input
    - level: &str - observe, assisted, delegated, or autonomous
 * Output
    - Option<f64> from 0 to 100
*/
pub(super) fn level(level: &str) -> Option<f64> {
    match level {
        "observe" => Some(0.0),
        "assisted" => Some(100.0 / 3.0),
        "delegated" => Some(200.0 / 3.0),
        "autonomous" => Some(100.0),
        _ => None,
    }
}

/** What the Overview needs of one session
 * Fields
    - run: &Run - the session
    - summary: &'a Value - its summary
    - attributes: BTreeMap<&str, BTreeSet<String>> - its value for every filter
    - started: u64 - when it started
    - last: u64 - its last activity
    - ended: Option<u64> - when it ended
    - verification: &str - passed, failed, or pending
    - verified_at: u64 - when its verification outcome was recorded
    - human: bool - a human approved a tool call or intervened
    - score: Option<f64> - autonomy score, 0 to 100
    - budget: Option<f64> - budget consumed, percent of its maximum
    - violations: &'a [Value] - its violations
*/
pub(super) struct Session<'a> {
    pub(super) run: &'a Run,
    pub(super) summary: &'a Value,
    pub(super) attributes: BTreeMap<&'static str, BTreeSet<String>>,
    pub(super) started: u64,
    pub(super) last: u64,
    pub(super) ended: Option<u64>,
    pub(super) verification: &'static str,
    pub(super) verified_at: u64,
    pub(super) human: bool,
    pub(super) score: Option<f64>,
    pub(super) budget: Option<f64>,
    pub(super) violations: &'a [Value],
}

/** What the pages need of one session that depends on the session alone: derived once, when the
 * session's files change (the filters that depend on other records are added per request)
 * Fields
    - attributes: BTreeMap<&str, BTreeSet<String>> - its value for every filter but repository
      and task status
    - started: u64 - when it started
    - last: u64 - its last activity
    - ended: Option<u64> - when it ended
    - verification: &str - passed, failed, or pending
    - verified_at: u64 - when its verification outcome was recorded
    - human: bool - a human approved a tool call or intervened
    - score: Option<f64> - autonomy score, 0 to 100
    - budget: Option<f64> - budget consumed, percent of its maximum
*/
#[derive(Default)]
pub(super) struct Facts {
    pub(super) attributes: BTreeMap<&'static str, BTreeSet<String>>,
    pub(super) started: u64,
    pub(super) last: u64,
    pub(super) ended: Option<u64>,
    pub(super) verification: &'static str,
    pub(super) verified_at: u64,
    pub(super) human: bool,
    pub(super) score: Option<f64>,
    pub(super) budget: Option<f64>,
}

impl Facts {
    /** Derive a session's own facts
     * Input
        - run: &Run - the session
     * Output
        - Facts
    */
    pub(super) fn derive(run: &Run) -> Self {
        let summary = &run.summarized;
        let at = |value: &Value| value.as_u64().unwrap_or(0);
        let started = summary["started_at"]
            .as_u64()
            .unwrap_or_else(|| at(&run.document["created_at"]));
        let last = at(&summary["last_activity_at"]).max(started);
        let finalized = summary["verification"]["final_decision"]
            .as_str()
            .unwrap_or_default();
        let failed = finalized == "FAIL"
            || summary["verification"]["contract_tests_passed"] == false
            || summary["verification"]["repository_tests_passed"] == false
            || summary["verification"]["last"] == "fail";
        let verification = if failed {
            "failed"
        } else if finalized == "PASS" || summary["verification"]["last"] == "pass" {
            "passed"
        } else {
            "pending"
        };
        let verified_at = run
            .acts
            .iter()
            .rev()
            .find(|act| {
                act.extra().verification.is_some()
                    || matches!(act.event, "stop" | "session_finalized")
            })
            .map_or(last, |act| act.at);
        let mutating = run
            .acts
            .iter()
            .filter(|act| act.mutating())
            .collect::<Vec<_>>();
        let scores = mutating
            .iter()
            .filter_map(|act| act.autonomy.and_then(level))
            .collect::<Vec<_>>();
        let human = mutating
            .iter()
            .any(|act| act.decision == Some("approval_required"))
            || run
                .acts
                .iter()
                .any(|act| act.extra().human.is_some_and(|action| action != "prompt"));
        let consumed = summary["budget"]["consumed"].as_f64();
        let max = summary["budget"]["final"]["max"]
            .as_f64()
            .filter(|max| *max > 0.0);
        let delivery = &summary["delivery"];
        let delivery_status = if delivery.is_null() {
            "none"
        } else if delivery["merged"] == true {
            "merged"
        } else if delivery["approved"] == true {
            "approved"
        } else if delivery["pull_request_created"] == true {
            "pr_created"
        } else {
            "submitted"
        };
        let violations = run.violation_list.as_slice();
        let text = |value: &Value| value.as_str().map(String::from);
        let one = |value: Option<String>| value.into_iter().collect::<BTreeSet<_>>();
        let contracts = run.contracts().as_array().cloned().unwrap_or_default();
        let organization = &run.governance()["organization"];
        let mut attributes = BTreeMap::new();
        attributes.insert("task", one(text(&run.task())));
        attributes.insert("agent", one(Some(run.agent())));
        attributes.insert("provider", one(Some(run.provider().to_string())));
        attributes.insert("session", one(Some(run.id.clone())));
        attributes.insert(
            "policy",
            contracts
                .iter()
                .filter_map(|contract| text(&contract["policy_id"]))
                .collect(),
        );
        attributes.insert(
            "policy_version",
            contracts
                .iter()
                .filter_map(|contract| {
                    Some(format!(
                        "{}@{}",
                        contract["policy_id"].as_str()?,
                        contract["contract_hash"]
                            .as_str()?
                            .trim_start_matches("sha256:")
                            .chars()
                            .take(12)
                            .collect::<String>()
                    ))
                })
                .collect(),
        );
        attributes.insert(
            "zone",
            run.acts
                .iter()
                .flat_map(|act| act.zones.iter().map(|zone| zone.to_string()))
                .collect(),
        );
        attributes.insert("autonomy", one(text(&summary["autonomy"]["current"])));
        attributes.insert("safety", one(text(&summary["safety"]["state"])));
        attributes.insert("session_status", one(text(&summary["lifecycle"])));
        attributes.insert(
            "severity",
            violations
                .iter()
                .filter_map(|violation| text(&violation["severity"]))
                .collect(),
        );
        attributes.insert("verification", one(Some(verification.to_string())));
        attributes.insert("delivery", one(Some(delivery_status.to_string())));
        attributes.insert("environment", one(Some(run.environment.to_string())));
        attributes.insert("branch", one(text(&run.delivery["branch"])));
        attributes.insert(
            "team",
            one(text(&organization["team"]).or_else(|| text(&organization["organization"]))),
        );
        Self {
            ended: summary["ended_at"].as_u64(),
            attributes,
            started,
            last,
            verification,
            verified_at,
            human,
            score: (!scores.is_empty()).then(|| scores.iter().sum::<f64>() / scores.len() as f64),
            budget: consumed
                .zip(max)
                .map(|(consumed, max)| consumed / max * 100.0),
        }
    }
}

impl<'a> Session<'a> {
    /** Take a session's facts for one request: its own, plus the repository and its task's state
     * Input
        - run: &'a Run - the session
        - repository: &str - name of the connected repository
        - tasks: &BTreeMap<String, String> - task states by task id
     * Output
        - Session<'a>
    */
    pub(super) fn new(run: &'a Run, repository: &str, tasks: &BTreeMap<String, String>) -> Self {
        let facts = &run.facts;
        let mut attributes = facts.attributes.clone();
        attributes.insert("repository", BTreeSet::from([repository.to_string()]));
        attributes.insert(
            "task_status",
            run.task()
                .as_str()
                .and_then(|task| tasks.get(task).cloned())
                .into_iter()
                .collect(),
        );
        Self {
            run,
            summary: &run.summarized,
            attributes,
            started: facts.started,
            last: facts.last,
            ended: facts.ended,
            verification: facts.verification,
            verified_at: facts.verified_at,
            human: facts.human,
            score: facts.score,
            budget: facts.budget,
            violations: &run.violation_list,
        }
    }

    /** Check the session against the filters
     * Input
        - query: &Query - filters
     * Output
        - bool
    */
    fn matches(&self, query: &Query) -> bool {
        SESSION_FILTERS.iter().all(|key| {
            query.set(key).is_none_or(|wanted| {
                self.attributes
                    .get(key)
                    .is_some_and(|values| values.iter().any(|value| wanted.contains(value)))
            })
        })
    }

    /** Check whether the session was active during the window
     * Input
        - window: &Window - window
     * Output
        - bool
    */
    pub(super) fn during(&self, window: &Window) -> bool {
        self.started <= window.to && self.last >= window.from
    }

    /** Check whether the session is active now
     * Input
        - None (uses self)
     * Output
        - bool
    */
    pub(super) fn active(&self) -> bool {
        self.summary["lifecycle"] == "active"
    }
}

/** What the dashboard pages need of one task
 * Fields
    - id: String - task id
    - state: Option<String> - orchestration state, None for a task known only from sessions
    - received: Option<u64> - when the society received it (or its first session started)
    - completed: Option<u64> - when it reached COMPLETED
    - failed: Option<u64> - when it reached FAILED
    - cancelled: Option<u64> - when it reached CANCELLED
    - confirmed: Option<u64> - when the tracker confirmed completion
    - completion: Option<String> - the completion event's state
    - sessions: Vec<usize> - indices of its sessions
    - record: Arc<Value> - its orchestration record (shared with the cache, never copied), null
      for a task known only from sessions
*/
pub(super) struct Task {
    pub(super) id: String,
    pub(super) state: Option<String>,
    pub(super) received: Option<u64>,
    pub(super) completed: Option<u64>,
    pub(super) failed: Option<u64>,
    pub(super) cancelled: Option<u64>,
    pub(super) confirmed: Option<u64>,
    pub(super) completion: Option<String>,
    pub(super) sessions: Vec<usize>,
    pub(super) record: Arc<Value>,
}

/** Format a share as a percentage, None when there is nothing to divide by
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

/** Average values, None when there are none
 * Input
    - values: Vec<f64> - values
 * Output
    - Value
*/
fn average(values: Vec<f64>) -> Value {
    if values.is_empty() {
        Value::Null
    } else {
        json!(((values.iter().sum::<f64>() / values.len() as f64) * 10.0).round() / 10.0)
    }
}

/** Describe one journal event as an activity event, None for events the stream leaves out
 * Input
    - session: &Session - its session
    - act: &Act - the event (with its evidence record)
 * Output
    - Option<Value> {event_id, at, seq, actor, task_id, session_id, description, outcome, detail,
      link}
*/
fn activity(session: &Session, act: &Act) -> Option<Value> {
    let run = session.run;
    let agent = actor(&run.agent());
    let extra = act.extra();
    let text = |value: Option<&str>| value.unwrap_or_default().to_string();
    let (who, description, outcome, detail) = match (act.category, act.event) {
        (Some("authorization"), _) if act.operation != Some("read") => {
            let files = act
                .resources
                .iter()
                .filter_map(|resource| resource.strip_prefix("file:"))
                .collect::<Vec<_>>();
            let description = match act.operation {
                Some("write") if !files.is_empty() => format!("{} modified", files.join(", ")),
                Some("delete") if !files.is_empty() => format!("{} deleted", files.join(", ")),
                Some("execute") => format!("command: {}", act.programs.join(" | ")),
                _ => format!("{} call", text(act.tool)),
            };
            let outcome = match act.decision {
                Some("allow") => "ALLOW",
                Some("deny") => "DENY",
                Some("approval_required") => "ASK",
                _ => "UNKNOWN",
            };
            let reasons = act.reasons.join(" ");
            let rule = ["preserve", "target"]
                .into_iter()
                .find(|rule| reasons.contains(&format!("by {rule} ")));
            let detail = if outcome != "ALLOW" && !act.policies.is_empty() {
                format!(
                    "Policy: {}",
                    act.policies
                        .iter()
                        .map(|policy| rule
                            .map_or(policy.to_string(), |rule| format!("{policy}/{rule}")))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else if outcome == "ASK" && !act.zones.is_empty() {
                format!("Zone: {}", act.zones.join(", "))
            } else if outcome == "DENY" {
                act.reasons
                    .first()
                    .map(|reason| reason.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            (agent, description, outcome.to_string(), detail)
        }
        (_, "post_tool_use") if extra.verification.is_some() => (
            "Society",
            "Contract verification".to_string(),
            text(extra.verification).to_uppercase(),
            String::new(),
        ),
        (_, "stop") => (
            "Society",
            "Verification at stop".to_string(),
            text(extra.final_status).to_uppercase(),
            String::new(),
        ),
        (_, "session_finalized") => (
            "Society",
            "Session finalized".to_string(),
            text(extra.final_status).to_uppercase(),
            extra.findings.join("; "),
        ),
        (_, "session_start") => (
            agent,
            "Session started".to_string(),
            "START".to_string(),
            String::new(),
        ),
        (Some("autonomy"), _) => (
            if extra.actor == Some("human") {
                "Human"
            } else {
                "Society"
            },
            if extra.trigger == Some("violation") {
                format!("Violation: {}", text(extra.kind))
            } else {
                format!("Autonomy {}", text(extra.trigger).replace('_', " "))
            },
            format!("{} / {}", text(act.autonomy), text(act.safety)).to_uppercase(),
            text(extra.reason),
        ),
        (Some("delivery"), name) => (
            if name == "delivery_submitted" {
                "Society"
            } else {
                "Human"
            },
            match name {
                "delivery_submitted" => "Pull request submitted".to_string(),
                "delivery_merged" => "Pull request merged".to_string(),
                other => format!(
                    "Delivery {}",
                    other.trim_start_matches("delivery_").replace('_', " ")
                ),
            },
            name.trim_start_matches("delivery_").to_uppercase(),
            text(extra.by),
        ),
        _ => return None,
    };
    Some(json!({
        "event_id": run.event_id(act.seq),
        "at": act.at,
        "seq": act.seq,
        "actor": who,
        "agent": run.agent(),
        "agent_id": run.agent_id(),
        "task_id": run.task(),
        "session_id": run.id,
        "description": description,
        "outcome": outcome,
        "detail": detail,
        "link": format!("#/runs/{}?seq={}", run.id, act.seq),
    }))
}

/** Check, without describing it, whether an event appears in the activity stream (see activity)
 * Input
    - act: &Act - the event
 * Output
    - bool
*/
fn streamed(act: &Act) -> bool {
    match (act.category, act.event) {
        (Some("authorization"), _) => act.operation != Some("read"),
        (_, "post_tool_use") => act.extra().verification.is_some(),
        (_, "stop" | "session_finalized" | "session_start") => true,
        (Some("autonomy" | "delivery"), _) => true,
        _ => false,
    }
}

/** One task record with what every page derives from it, computed once per record version
 * Fields
    - id: String - task id
    - state: Option<String> - orchestration state
    - received: Option<u64> - when it was received
    - completed: Option<u64> - when it reached COMPLETED
    - failed: Option<u64> - when it reached FAILED
    - cancelled: Option<u64> - when it reached CANCELLED
    - record: Arc<Value> - the record
*/
pub(crate) struct TaskBase {
    pub(super) id: String,
    pub(super) state: Option<String>,
    pub(super) received: Option<u64>,
    pub(super) completed: Option<u64>,
    pub(super) failed: Option<u64>,
    pub(super) cancelled: Option<u64>,
    pub(super) record: Arc<Value>,
}

/** The task records indexed for the pages (a materialized view, rebuilt with the snapshot)
 * Fields
    - tasks: Vec<TaskBase> - every task record, by id
    - position: HashMap<String, usize> - where each task is
    - listed: HashMap<String, Vec<usize>> - the tasks each session is listed in
    - states: Arc<BTreeMap<String, String>> - state by task id
    - statuses: Arc<BTreeSet<String>> - the states tasks are in
*/
pub(crate) struct TaskIndex {
    pub(super) tasks: Vec<TaskBase>,
    pub(super) position: std::collections::HashMap<String, usize>,
    pub(super) listed: std::collections::HashMap<String, Vec<usize>>,
    pub(super) states: Arc<BTreeMap<String, String>>,
    pub(super) statuses: Arc<BTreeSet<String>>,
}

impl TaskIndex {
    /** Index task records
     * Input
        - records: &[Arc<Value>] - task records
     * Output
        - TaskIndex
    */
    pub(super) fn build(records: &[Arc<Value>]) -> Self {
        let mut tasks = records
            .iter()
            .map(|record| {
                let history = record["history"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let when = |state: &str| {
                    history
                        .iter()
                        .rev()
                        .find(|entry| entry["to"] == state)
                        .and_then(|entry| entry["at"].as_u64())
                };
                TaskBase {
                    id: record["task_id"].as_str().unwrap_or_default().to_string(),
                    state: record["state"].as_str().map(String::from),
                    received: history.first().and_then(|entry| entry["at"].as_u64()),
                    completed: when("COMPLETED"),
                    failed: when("FAILED"),
                    cancelled: when("CANCELLED"),
                    record: record.clone(),
                }
            })
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| left.id.cmp(&right.id));
        tasks.dedup_by(|later, earlier| later.id == earlier.id);
        let mut listed: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();
        for (position, task) in tasks.iter().enumerate() {
            for session in task.record["sessions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let positions = listed.entry(session.to_string()).or_default();
                if !positions.contains(&position) {
                    positions.push(position);
                }
            }
        }
        let states = tasks
            .iter()
            .filter_map(|task| Some((task.id.clone(), task.state.clone()?)))
            .collect::<BTreeMap<_, _>>();
        Self {
            position: tasks
                .iter()
                .enumerate()
                .map(|(position, task)| (task.id.clone(), position))
                .collect(),
            statuses: Arc::new(states.values().cloned().collect()),
            states: Arc::new(states),
            listed,
            tasks,
        }
    }
}

/** Everything the dashboard pages read, before filtering
 * Fields
    - repository: String - name of the connected repository
    - states: Arc<BTreeMap<String, String>> - orchestration state by task id
    - statuses: Arc<BTreeSet<String>> - the states tasks are in
    - sessions: Vec<Session<'a>> - every session
    - tasks: Vec<Task> - every task: orchestrated ones, plus tasks known only from sessions
*/
pub(super) struct Dataset<'a> {
    pub(super) repository: String,
    pub(super) states: Arc<BTreeMap<String, String>>,
    pub(super) statuses: Arc<BTreeSet<String>>,
    pub(super) sessions: Vec<Session<'a>>,
    pub(super) tasks: Vec<Task>,
}

impl<'a> Dataset<'a> {
    /** Read the dataset from the sessions, the task records, and the completion events
     * Input
        - runs: &'a [Arc<Run>] - every session
     * Output
        - Result<Dataset<'a>, String>
    */
    pub(super) fn new(runs: &'a [Arc<Run>]) -> Result<Self, String> {
        let (_, repository) = super::repository_tenant()?;
        let index = super::task_index()?;
        let states = index.states.clone();
        let mut sessions = runs
            .iter()
            .map(|run| Session::new(run, &repository, &states))
            .collect::<Vec<_>>();
        let completions = super::completions()?;
        let mut tasks = index
            .tasks
            .iter()
            .map(|base| Task {
                id: base.id.clone(),
                state: base.state.clone(),
                received: base.received,
                completed: base.completed,
                failed: base.failed,
                cancelled: base.cancelled,
                confirmed: None,
                completion: None,
                sessions: Vec::new(),
                record: base.record.clone(),
            })
            .collect::<Vec<_>>();
        // Tasks known only from a session's binding are added after the recorded ones
        let mut extra: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let none = Vec::new();
        // A session belongs to the tasks whose records list it, as well as to its bound task
        for session in sessions.iter_mut() {
            for position in index.listed.get(&session.run.id).unwrap_or(&none) {
                let task = &index.tasks[*position];
                session
                    .attributes
                    .entry("task")
                    .or_default()
                    .insert(task.id.clone());
                if let Some(state) = &task.state {
                    session
                        .attributes
                        .entry("task_status")
                        .or_default()
                        .insert(state.clone());
                }
            }
        }
        for (number, session) in sessions.iter().enumerate() {
            let mut positions = index.listed.get(&session.run.id).unwrap_or(&none).clone();
            if let Some(bound) = session.run.task().as_str() {
                let position = match index.position.get(bound).or_else(|| extra.get(bound)) {
                    Some(position) => *position,
                    None => {
                        tasks.push(Task {
                            id: bound.to_string(),
                            state: None,
                            received: Some(session.started),
                            completed: None,
                            failed: None,
                            cancelled: None,
                            confirmed: None,
                            completion: None,
                            sessions: Vec::new(),
                            record: Arc::new(Value::Null),
                        });
                        extra.insert(bound.to_string(), tasks.len() - 1);
                        tasks.len() - 1
                    }
                };
                positions.push(position);
            }
            positions.sort_unstable();
            positions.dedup();
            for position in positions {
                let task = &mut tasks[position];
                task.received = task
                    .received
                    .map(|at| at.min(session.started))
                    .or(Some(session.started));
                task.sessions.push(number);
            }
        }
        for event in completions.iter() {
            let position = event["task_id"]
                .as_str()
                .and_then(|id| index.position.get(id).or_else(|| extra.get(id)));
            if let Some(task) = position.map(|position| &mut tasks[*position]) {
                let state = crate::task_completion::lifecycle_state(event);
                task.completion = Some(state.to_string());
                if state == "COMPLETED" {
                    task.confirmed = event["completed_at"].as_u64().or(task.confirmed);
                }
            }
        }
        if !extra.is_empty() {
            tasks.sort_by(|left, right| left.id.cmp(&right.id));
        }
        Ok(Self {
            repository,
            states,
            statuses: index.statuses.clone(),
            sessions,
            tasks,
        })
    }

    /** Apply the filters: a session matches every filter; a task matches its own filters (task,
     * task status, repository) and, when another filter is set, has a matching session
     * Input
        - query: &Query - filters
     * Output
        - (BTreeSet<usize>, Vec<&Task>) indices of the matching sessions, and the matching tasks
    */
    pub(super) fn select(&self, query: &Query) -> (BTreeSet<usize>, Vec<&Task>) {
        let sessions = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| session.matches(query))
            .map(|(index, _)| index)
            .collect::<BTreeSet<_>>();
        let session_filtered = SESSION_FILTERS
            .iter()
            .any(|key| !TASK_FILTERS.contains(key) && query.get(key).is_some());
        let tasks = self
            .tasks
            .iter()
            .filter(|task| {
                query
                    .set("task")
                    .is_none_or(|wanted| wanted.contains(&task.id))
                    && query.set("task_status").is_none_or(|wanted| {
                        task.state
                            .as_ref()
                            .is_some_and(|state| wanted.contains(state))
                    })
                    && query
                        .set("repository")
                        .is_none_or(|wanted| wanted.contains(&self.repository))
                    && (!session_filtered
                        || task.sessions.iter().any(|index| sessions.contains(index)))
            })
            .collect();
        (sessions, tasks)
    }
}

/** Answer the Overview
 * Input
    - query: &Query - window and filters (already checked against PARAMETERS)
 * Output
    - Result<Value, (u16, String)> the answer, or 400 for a bad window or filter value and 500
      when a record cannot be read
*/
pub(crate) fn overview(query: &Query) -> Result<Value, (u16, String)> {
    let now = now_unix();
    let window = Window::parse(query, now).map_err(|error| (400, error))?;
    for (key, accepted) in [
        (
            "autonomy",
            &["observe", "assisted", "delegated", "autonomous"][..],
        ),
        ("safety", &["active", "degraded", "quarantined"]),
        (
            "session_status",
            &["active", "closed", "cancelled", "finalized"],
        ),
        ("severity", super::SEVERITIES),
        ("verification", &["passed", "failed", "pending"]),
        (
            "delivery",
            &["none", "submitted", "pr_created", "approved", "merged"],
        ),
        ("environment", &["repository", "worktree"]),
    ] {
        query.choice(key, accepted).map_err(|error| (400, error))?;
    }
    let limit = query.limit().map_err(|error| (400, error))?;
    let failed = |error: String| (500, error);
    let runs = Run::all().map_err(failed)?;
    let data = Dataset::new(&runs).map_err(failed)?;
    let repository = data.repository.clone();
    let states = data.states.clone();
    let all = &data.sessions;
    let (sessions, tasks) = data.select(query);
    let matching = sessions
        .iter()
        .map(|index| &all[*index])
        .collect::<Vec<_>>();
    let in_window = matching
        .iter()
        .filter(|session| session.during(&window))
        .copied()
        .collect::<Vec<_>>();
    let active = matching
        .iter()
        .filter(|session| session.active())
        .copied()
        .collect::<Vec<_>>();
    let severities = query.set("severity");
    let violations = matching
        .iter()
        .flat_map(|session| session.violations.iter())
        .filter(|violation| window.holds(violation["at"].as_u64().unwrap_or(0)))
        .filter(|violation| {
            severities.as_ref().is_none_or(|wanted| {
                violation["severity"]
                    .as_str()
                    .is_some_and(|severity| wanted.contains(severity))
            })
        })
        .collect::<Vec<_>>();
    let completed = tasks
        .iter()
        .copied()
        .filter(|task| task.completed.is_some_and(|at| window.holds(at)))
        .collect::<Vec<_>>();
    let autonomous_completion = |tasks: &[&Task]| {
        rate(
            tasks
                .iter()
                .filter(|task| task.sessions.iter().all(|index| !all[*index].human))
                .count(),
            tasks.len(),
        )
    };
    let delivery_at = |session: &&Session, key: &str| {
        session.run.delivery[key]["at"]
            .as_u64()
            .filter(|at| window.holds(*at))
            .is_some()
    };
    let verified = in_window
        .iter()
        .filter(|session| session.verification != "pending")
        .collect::<Vec<_>>();
    let metric = |key: &str,
                  label: &str,
                  value: Value,
                  unit: &str,
                  kind: &str,
                  definition: &str| {
        json!({"key": key, "label": label, "value": value, "unit": unit, "kind": kind, "definition": definition})
    };
    let metrics = vec![
        metric("active_tasks", "Active Tasks", json!(tasks.iter().filter(|task| match &task.state { Some(state) => !TERMINAL.contains(&state.as_str()), None => task.sessions.iter().any(|index| all[*index].active()) }).count()), "", "now", "Tasks not yet completed, failed, or cancelled"),
        metric("active_sessions", "Active Sessions", json!(active.len()), "", "now", "Agent sessions that can still act"),
        metric("autonomous_sessions", "Autonomous Sessions", json!(active.iter().filter(|session| session.summary["autonomy"]["current"] == "autonomous").count()), "", "now", "Active sessions whose autonomy mode is autonomous"),
        metric("completed_tasks", "Completed Tasks", json!(completed.len()), "", "window", "Tasks whose lifecycle reached COMPLETED in the window"),
        metric("failed_tasks", "Failed Tasks", json!(tasks.iter().filter(|task| task.failed.is_some_and(|at| window.holds(at))).count()), "", "window", "Tasks whose lifecycle reached FAILED in the window"),
        metric("verification_failures", "Verification Failures", json!(in_window.iter().filter(|session| session.verification == "failed").count()), "", "window", "Sessions whose verification, contract tests, or repository tests failed"),
        metric("policy_violations", "Policy Violations", json!(violations.len()), "", "window", "Violations recorded in the window, every severity"),
        metric("critical_violations", "Critical Violations", json!(violations.iter().filter(|violation| violation["severity"] == "critical").count()), "", "window", "Violations the session's autonomy policy treats as critical (they quarantine)"),
        metric("quarantined_sessions", "Quarantined Sessions", json!(active.iter().filter(|session| session.summary["safety"]["state"] == "quarantined").count()), "", "now", "Active sessions that are quarantined and may not change anything"),
        metric("prs_created", "PRs Created", json!(matching.iter().filter(|session| delivery_at(session, "pull_request")).count()), "", "window", "Pull requests opened for sessions in the window"),
        metric("prs_merged", "PRs Merged", json!(matching.iter().filter(|session| delivery_at(session, "merged")).count()), "", "window", "Pull requests merged in the window"),
        metric("tasks_completed", "Tasks Completed", json!(tasks.iter().filter(|task| task.confirmed.is_some_and(|at| window.holds(at))).count()), "", "window", "Tasks the tracker (Jira or Asana) confirmed as done in the window"),
        metric("autonomous_completion_rate", "Autonomous Completion Rate", autonomous_completion(&completed), "%", "window", "Completed tasks whose sessions needed no human approval or intervention (the merge review excepted)"),
        metric("verification_pass_rate", "Verification Pass Rate", rate(verified.iter().filter(|session| session.verification == "passed").count(), verified.len()), "%", "window", "Sessions with a verification outcome that passed"),
        metric("human_intervention_rate", "Human Intervention Rate", rate(in_window.iter().filter(|session| session.human).count(), in_window.len()), "%", "window", "Sessions in which a human approved a tool call or intervened"),
        metric("average_task_duration", "Average Task Duration", average(completed.iter().filter_map(|task| Some(task.completed?.saturating_sub(task.received?) as f64)).collect()), "s", "window", "From receiving a task to completing it, for tasks completed in the window"),
        metric("average_session_duration", "Average Session Duration", average(in_window.iter().filter(|session| !session.active()).map(|session| session.ended.unwrap_or(session.last).saturating_sub(session.started) as f64).collect()), "s", "window", "From start to end of sessions that ended"),
        metric("average_violations_per_session", "Average Violations per Session", if in_window.is_empty() { Value::Null } else { json!(((violations.len() as f64 / in_window.len() as f64) * 100.0).round() / 100.0) }, "", "window", "Violations in the window divided by sessions active in it"),
        metric("average_autonomy_score", "Average Autonomy Score", average(in_window.iter().filter_map(|session| session.score).collect()), "/100", "window", "Autonomy each change ran at: observe 0, assisted 33, delegated 67, autonomous 100, averaged over a session's changes"),
        metric("average_budget_consumption", "Average Budget Consumption", average(in_window.iter().filter_map(|session| session.budget).collect()), "%", "window", "Autonomy budget consumed, as a share of the session's maximum"),
    ];

    // Trends, one value per bucket
    let buckets = window.buckets();
    let empty = || vec![0usize; buckets.len()];
    let place = |at: u64| {
        window
            .holds(at)
            .then(|| window.bucket_of(at))
            .filter(|index| *index < buckets.len())
    };
    let mut started = empty();
    let mut received = empty();
    let mut violation_counts = empty();
    let mut critical_counts = empty();
    let mut budget = vec![0u64; buckets.len()];
    let mut completed_by: Vec<Vec<&Task>> = vec![Vec::new(); buckets.len()];
    let mut verified_by: Vec<(usize, usize)> = vec![(0, 0); buckets.len()];
    let mut human_by: Vec<(usize, usize)> = vec![(0, 0); buckets.len()];
    let mut score_by: Vec<Vec<f64>> = vec![Vec::new(); buckets.len()];
    for session in &matching {
        if let Some(index) = place(session.started) {
            started[index] += 1;
            human_by[index].1 += 1;
            human_by[index].0 += usize::from(session.human);
        }
        if let (Some(index), true) = (
            place(session.verified_at),
            session.verification != "pending",
        ) {
            verified_by[index].1 += 1;
            verified_by[index].0 += usize::from(session.verification == "passed");
        }
        if let (Some(index), Some(score)) = (place(session.last), session.score) {
            score_by[index].push(score);
        }
        for act in &session.run.acts {
            if let Some(index) = place(act.at) {
                budget[index] += act.consumed();
            }
        }
    }
    for violation in &violations {
        if let Some(index) = violation["at"].as_u64().and_then(place) {
            violation_counts[index] += 1;
            critical_counts[index] += usize::from(violation["severity"] == "critical");
        }
    }
    for task in tasks.iter().copied() {
        if let Some(index) = task.received.and_then(place) {
            received[index] += 1;
        }
        if let Some(index) = task.completed.and_then(place) {
            completed_by[index].push(task);
        }
    }
    let series = |values: Vec<Value>| {
        buckets
            .iter()
            .zip(values)
            .map(|(at, value)| json!({"t": at, "v": value}))
            .collect::<Vec<_>>()
    };
    let counts = |values: &[usize]| series(values.iter().map(|value| json!(value)).collect());
    let trends = json!([
        {"key": "sessions", "label": "Sessions started", "unit": "", "points": counts(&started)},
        {"key": "tasks", "label": "Tasks received", "unit": "", "points": counts(&received)},
        {"key": "violations", "label": "Violations", "unit": "", "points": counts(&violation_counts), "secondary": {"label": "critical", "points": counts(&critical_counts)}},
        {"key": "autonomous_completion_rate", "label": "Autonomous completion rate", "unit": "%", "points": series(completed_by.iter().map(|tasks| autonomous_completion(tasks)).collect())},
        {"key": "verification_success_rate", "label": "Verification success rate", "unit": "%", "points": series(verified_by.iter().map(|(passed, total)| rate(*passed, *total)).collect())},
        {"key": "human_intervention_rate", "label": "Human intervention rate", "unit": "%", "points": series(human_by.iter().map(|(human, total)| rate(*human, *total)).collect())},
        {"key": "autonomy_score", "label": "Autonomy score", "unit": "/100", "points": series(score_by.into_iter().map(average).collect())},
        {"key": "budget_consumption", "label": "Budget consumed", "unit": "", "points": series(budget.into_iter().map(|value| json!(value)).collect())},
    ]);

    // Active now
    let task_state = |task: &Value| task.as_str().and_then(|id| states.get(id)).cloned();
    let active_now = active
        .iter()
        .map(|session| {
            let latest = session
                .run
                .acts
                .iter()
                .rev()
                .find_map(|act| activity(session, act));
            let budget = session
                .run
                .acts
                .last()
                .and_then(|act| act.after)
                .map(|budget| budget.to_json());
            json!({
                "task_id": session.run.task(),
                "task_status": task_state(&session.run.task()),
                "repository": repository,
                "agent": actor(&session.run.agent()),
                "agent_id": session.run.agent_id(),
                "provider": session.run.provider(),
                "session_id": session.run.id,
                "status": if session.summary["phase"].is_null() { session.summary["lifecycle"].clone() } else { session.summary["phase"].clone() },
                "autonomy": session.summary["autonomy"]["current"],
                "safety": session.summary["safety"]["state"],
                "budget_remaining": budget.map(|budget| json!({"available": budget["available"], "max": budget["max"]})),
                "latest_event": latest,
                "elapsed": now.saturating_sub(session.started),
                "link": format!("#/runs/{}", session.run.id),
            })
        })
        .collect::<Vec<_>>();

    // Recent activity, newest first: every event in the window is counted, but only the newest
    // `limit` are described (a bounded selection, not a list of the whole window)
    let mut newest: std::collections::BinaryHeap<std::cmp::Reverse<(u64, u64, usize, usize)>> =
        std::collections::BinaryHeap::new();
    let mut total_events = 0;
    let mut keep = |key: (u64, u64, usize, usize)| {
        total_events += 1;
        newest.push(std::cmp::Reverse(key));
        if newest.len() > limit {
            newest.pop();
        }
    };
    for (position, session) in matching.iter().enumerate() {
        for (index, act) in session.run.acts.iter().enumerate() {
            if window.holds(act.at) && streamed(act) {
                keep((act.at, act.seq, position, index));
            }
        }
    }
    let task_list = tasks.to_vec();
    for (position, task) in task_list.iter().enumerate() {
        for (index, entry) in task.record["history"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let at = entry["at"].as_u64().unwrap_or(0);
            if window.holds(at) {
                keep((at, 0, usize::MAX - position, index));
            }
        }
    }
    let mut events = Vec::new();
    for std::cmp::Reverse((_, _, position, index)) in newest.into_sorted_vec() {
        if position < matching.len() {
            let session = matching[position];
            if let Some(item) = activity(session, &session.run.acts[index]) {
                events.push(item);
            }
        } else {
            let task = task_list[usize::MAX - position];
            let entry = &task.record["history"][index];
            {
                events.push(json!({
                    "event_id": format!("task:{}:{index}", task.id),
                    "at": entry["at"],
                    "seq": null,
                    "actor": "Society",
                    "agent": null,
                    "task_id": task.id,
                    "session_id": null,
                    "description": format!("Task {}", entry["reason"].as_str().unwrap_or("changed state")),
                    "outcome": entry["to"],
                    "detail": entry["from"].as_str().map_or(String::new(), |from| format!("from {from}")),
                    "link": format!("#/tasks/{}", task.id),
                }));
            }
        }
    }

    // The values every filter can take, from everything (not only what matches)
    let mut facets: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for session in all {
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
        .extend(states.keys().cloned());
    facets
        .entry("task_status")
        .or_default()
        .extend(states.values().cloned());
    facets
        .entry("repository")
        .or_default()
        .insert(repository.clone());
    for (key, values) in [
        (
            "autonomy",
            &["observe", "assisted", "delegated", "autonomous"][..],
        ),
        ("safety", &["active", "degraded", "quarantined"]),
        (
            "session_status",
            &["active", "closed", "cancelled", "finalized"],
        ),
        ("severity", super::SEVERITIES),
        ("verification", &["passed", "failed", "pending"]),
        (
            "delivery",
            &["none", "submitted", "pr_created", "approved", "merged"],
        ),
        ("environment", &["repository", "worktree"]),
    ] {
        facets
            .entry(key)
            .or_default()
            .extend(values.iter().map(|value| value.to_string()));
    }
    let (facets, facets_truncated) = bounded(facets);
    Ok(json!({
        "now": now,
        "window": {"range": window.name, "from": window.from, "to": window.to, "bucket": window.bucket},
        "filters": SESSION_FILTERS.iter().filter_map(|key| query.set(key).map(|values| (key.to_string(), json!(values)))).collect::<serde_json::Map<_, _>>(),
        "counts": {"sessions": matching.len(), "sessions_in_window": in_window.len(), "tasks": tasks.len()},
        "metrics": metrics,
        "trends": trends,
        "active_now_total": active_now.len(),
        "active_now": active_now.into_iter().take(DETAIL_LIMIT).collect::<Vec<_>>(),
        "recent_activity": {"total": total_events, "items": events},
        "facets": facets,
        "facets_truncated": facets_truncated,
    }))
}

/** Parameters the Tasks page accepts: the global filters and window, plus search, sort, and page */
pub(crate) const TASK_PARAMETERS: &[&str] = &[
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

/** Sort orders of the Tasks page */
const SORTS: &[&str] = &[
    "newest",
    "oldest",
    "duration",
    "violations",
    "autonomy_score",
    "budget",
    "risk",
    "status",
];

/** Default and largest page size of the Tasks page */
const PAGE_SIZES: (usize, usize) = (50, 200);

/** The lifecycle order the status sort follows */
const STATUS_ORDER: &[&str] = &[
    "RECEIVED",
    "ANALYZING",
    "CONTRACT_PROPOSED",
    "APPROVED",
    "EXECUTING",
    "VALIDATING",
    "PR_READY",
    "REVIEW",
    "MERGED",
    "COMPLETED",
    "BLOCKED",
    "DEGRADED",
    "FAILED",
    "CANCELLED",
    "RUNNING",
    "ENDED",
];

/** What the Tasks page sorts one task by, computed for every matching task (the full row is built
 * only for the rows of the requested page)
 * Fields
    - task: &'a Task - the task
    - sessions: Vec<&'a Session<'a>> - its sessions
    - status: String - its current or final status
    - terminal: bool - the status is final
    - start: u64 - when it started
    - end: u64 - when it ended, or now while it runs
    - violations: u64 - violations of its sessions
    - critical: u64 - critical violations of its sessions
    - score: Option<f64> - average autonomy score of its sessions
    - budget: f64 - budget consumed by its sessions
    - risk: f64 - risk score
    - rank: usize - position of its status in the lifecycle
*/
pub(super) struct Keys<'a> {
    pub(super) task: &'a Task,
    pub(super) sessions: Vec<&'a Session<'a>>,
    pub(super) status: String,
    pub(super) terminal: bool,
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) violations: u64,
    pub(super) critical: u64,
    pub(super) score: Option<f64>,
    pub(super) budget: f64,
    pub(super) risk: f64,
    pub(super) rank: usize,
}

impl<'a> Keys<'a> {
    /** Compute a task's sort keys from its record and its sessions
     * Input
        - task: &'a Task - task
        - data: &'a Dataset - every session
        - now: u64 - current time
     * Output
        - Keys<'a>
    */
    pub(super) fn new(task: &'a Task, data: &'a Dataset, now: u64) -> Self {
        let sessions = task
            .sessions
            .iter()
            .map(|index| &data.sessions[*index])
            .collect::<Vec<_>>();
        let sum = |path: &[&str]| {
            sessions
                .iter()
                .map(|session| {
                    path.iter()
                        .fold(session.summary, |value, key| &value[*key])
                        .as_u64()
                        .unwrap_or(0)
                })
                .sum::<u64>()
        };
        let running = sessions.iter().any(|session| session.active());
        let status = task.state.clone().unwrap_or_else(|| {
            if running {
                "RUNNING".into()
            } else {
                "ENDED".into()
            }
        });
        let terminal = TERMINAL.contains(&status.as_str()) || status == "ENDED";
        let start = task.received.unwrap_or(0);
        let last = sessions
            .iter()
            .map(|session| session.ended.unwrap_or(session.last))
            .max();
        let end = [task.completed, task.failed, task.cancelled]
            .into_iter()
            .flatten()
            .max()
            .or(if terminal { last } else { None })
            .unwrap_or(now)
            .max(start);
        let violations = sum(&["violations", "total"]);
        let critical = sum(&["violations", "critical"]);
        let scores = sessions
            .iter()
            .filter_map(|session| session.score)
            .collect::<Vec<_>>();
        let failed = sessions.iter().any(|session| {
            session.verification == "failed"
                || session.summary["verification"]["repository_tests_passed"] == false
        });
        let quarantined = sessions
            .iter()
            .filter(|session| session.summary["safety"]["state"] == "quarantined")
            .count();
        let risk = critical as f64 * 10.0
            + violations.saturating_sub(critical) as f64 * 3.0
            + if failed { 5.0 } else { 0.0 }
            + quarantined as f64 * 5.0
            + sum(&["actions", "deny"]) as f64;
        Self {
            task,
            rank: STATUS_ORDER
                .iter()
                .position(|state| *state == status)
                .unwrap_or(STATUS_ORDER.len()),
            status,
            terminal,
            start,
            end,
            violations,
            critical,
            score: (!scores.is_empty()).then(|| scores.iter().sum::<f64>() / scores.len() as f64),
            budget: sessions
                .iter()
                .filter_map(|session| session.summary["budget"]["consumed"].as_f64())
                .sum::<f64>(),
            risk,
            sessions,
        }
    }

    /** Return the task's title: from its sessions, else from its task contract
     * Input
        - None (uses self)
     * Output
        - Option<String>
    */
    pub(super) fn title(&self) -> Option<String> {
        self.sessions
            .iter()
            .find_map(|session| session.summary["task_title"].as_str().map(String::from))
            .or_else(|| {
                super::contract(&self.task.id)
                    .and_then(|contract| contract["task"]["title"].as_str().map(String::from))
            })
    }

    /** Return the latest delivered session's pull request, if any
     * Input
        - None (uses self)
     * Output
        - Option<&Value>
    */
    pub(super) fn pull_request(&self) -> Option<&'a Value> {
        self.sessions
            .iter()
            .filter(|session| !session.run.delivery.is_null())
            .max_by_key(|session| session.started)
            .map(|session| &session.run.delivery["pull_request"])
            .filter(|pull_request| !pull_request.is_null())
    }

    /** Build the lowercase text a search matches: task ID, title, repository, PR number and URL,
     * commit SHAs, session IDs, agent IDs, and the tracker's id
     * Input
        - repository: &str - repository name
     * Output
        - String
    */
    fn haystack(&self, repository: &str) -> String {
        let mut words = vec![
            self.task.id.clone(),
            self.title().unwrap_or_default(),
            repository.to_string(),
            self.task.record["external_id"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        ];
        if let Some(pull_request) = self.pull_request() {
            words.push(format!(
                "#{} {}",
                pull_request["number"],
                pull_request["url"].as_str().unwrap_or_default()
            ));
        }
        for session in &self.sessions {
            words.push(session.run.id.clone());
            words.push(session.run.agent());
            for value in [
                &session.run.document["provider_session"],
                &session.run.delivery["head"],
                &session.run.delivery["merged"]["sha"],
            ] {
                if let Some(text) = value.as_str() {
                    words.push(text.to_string());
                }
            }
        }
        words.join(" ").to_lowercase()
    }

    /** Build the full row
     * Input
        - repository: &str - repository name
     * Output
        - Value
    */
    pub(super) fn row(&self, repository: &str) -> Value {
        let sessions = &self.sessions;
        let values = |key: &str| {
            sessions
                .iter()
                .flat_map(|session| session.attributes.get(key).cloned().unwrap_or_default())
                .collect::<BTreeSet<_>>()
        };
        let worst = |key: &str, order: &[&str]| {
            let found = values(key);
            order
                .iter()
                .find(|value| found.contains(**value))
                .map(|value| value.to_string())
        };
        let tests = |passed: bool| {
            sessions
                .iter()
                .any(|session| session.summary["verification"]["repository_tests_passed"] == passed)
        };
        let latest = sessions.iter().max_by_key(|session| session.started);
        let delivered = sessions
            .iter()
            .filter(|session| !session.run.delivery.is_null())
            .max_by_key(|session| session.started);
        let approvals = sessions
            .iter()
            .map(|session| {
                session.summary["actions"]["approval_required"]
                    .as_u64()
                    .unwrap_or(0)
                    + session.summary["delivery"]["approvals"]
                        .as_u64()
                        .unwrap_or(0)
            })
            .sum::<u64>();
        json!({
            "task_id": self.task.id,
            "title": self.title(),
            "source": self.task.record["source"],
            "external_id": self.task.record["external_id"],
            "repository": repository,
            "team": values("team"),
            "agents": sessions.iter().map(|session| actor(&session.run.agent())).collect::<BTreeSet<_>>(),
            "providers": values("provider"),
            "runs": sessions.len(),
            "sessions": sessions.iter().map(|session| session.run.id.clone()).collect::<Vec<_>>(),
            "status": self.status,
            "final": self.terminal,
            "start": self.task.received,
            "duration": self.end - self.start,
            "autonomy": latest.map(|session| session.summary["autonomy"]["current"].clone()),
            "autonomy_score": self.score.map(|score| (score * 10.0).round() / 10.0),
            "violations": self.violations,
            "critical_violations": self.critical,
            "approvals": approvals,
            "contract_verification": worst("verification", &["failed", "passed", "pending"]).unwrap_or_else(|| "none".into()),
            "repository_tests": if tests(false) { "failed" } else if tests(true) { "passed" } else { "none" },
            "pull_request": self.pull_request().map(|pull_request| json!({"number": pull_request["number"], "url": pull_request["url"]})),
            "merge_status": worst("delivery", &["merged", "approved", "pr_created", "submitted"]).unwrap_or_else(|| "none".into()),
            "merge_sha": delivered.map(|session| session.run.delivery["merged"]["sha"].clone()),
            "completion_status": self.task.completion,
            "budget_consumed": self.budget,
            "risk": self.risk,
            "link": format!("#/tasks/{}", self.task.id),
        })
    }
}

/** Most values a filter's facet lists; the rest are found by typing them into the search */
pub(super) const FACET_LIMIT: usize = 500;

/** Most items a list inside a detail answer carries (the newest; its total is reported beside it) */
pub(super) const DETAIL_LIMIT: usize = 500;

/** Keep the newest DETAIL_LIMIT items of a list, in their original order
 * Input
    - items: Vec<Value> - the list
    - key: &str - the field holding each item's time
 * Output
    - (Vec<Value>, usize) the kept items, and how many there were
*/
pub(super) fn newest(items: Vec<Value>, key: &str) -> (Vec<Value>, usize) {
    let total = items.len();
    if total <= DETAIL_LIMIT {
        return (items, total);
    }
    let mut order = (0..total).collect::<Vec<_>>();
    order.sort_by_key(|index| std::cmp::Reverse(items[*index][key].as_u64().unwrap_or(0)));
    let kept = order
        .into_iter()
        .take(DETAIL_LIMIT)
        .collect::<BTreeSet<_>>();
    let items = items
        .into_iter()
        .enumerate()
        .filter(|(index, _)| kept.contains(index))
        .map(|(_, item)| item)
        .collect();
    (items, total)
}

/** Bound the values every filter offers: at most FACET_LIMIT per filter (a filter still accepts
 * any value typed into the URL; the list only feeds the pickers)
 * Input
    - facets: BTreeMap<&str, BTreeSet<String>> - values per filter
 * Output
    - (Value, Vec<String>) the bounded values, and the filters that were cut
*/
pub(super) fn bounded(facets: BTreeMap<&str, BTreeSet<String>>) -> (Value, Vec<String>) {
    let truncated = facets
        .iter()
        .filter(|(_, values)| values.len() > FACET_LIMIT)
        .map(|(key, _)| key.to_string())
        .collect::<Vec<_>>();
    let facets = facets
        .into_iter()
        .map(|(key, values)| {
            (
                key.to_string(),
                json!(values.into_iter().take(FACET_LIMIT).collect::<Vec<_>>()),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    (Value::Object(facets), truncated)
}

/** Answer the Tasks page: one row per task matching the filters, the window (a task overlaps
 * it), and the search, sorted on cheap keys and paginated on the server, building full rows only
 * for the requested page
 * Input
    - query: &Query - window, filters, q, sort, page, page_size
 * Output
    - Result<Value, (u16, String)> {window, total, page, pages, page_size, sort, items, facets}
*/
pub(crate) fn tasks(query: &Query) -> Result<Value, (u16, String)> {
    let now = now_unix();
    let bad = |error: String| (400, error);
    let window = Window::parse(query, now).map_err(bad)?;
    query.choice("sort", SORTS).map_err(bad)?;
    let number = |key: &str, default: usize, max: usize| match query.get(key) {
        None => Ok(default),
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|value| (1..=max).contains(value))
            .ok_or_else(|| format!("{key} must be a number from 1 to {max}, not '{text}'")),
    };
    let page_size = number("page_size", PAGE_SIZES.0, PAGE_SIZES.1).map_err(bad)?;
    let page = number("page", 1, 1_000_000).map_err(bad)?;
    let sort = query.get("sort").unwrap_or("newest");
    let search = query.get("q").map(str::to_lowercase);
    let failed = |error: String| (500, error);
    let runs = Run::all().map_err(failed)?;
    let data = Dataset::new(&runs).map_err(failed)?;
    let (_, tasks) = data.select(query);
    let mut keys = tasks
        .into_iter()
        .map(|task| Keys::new(task, &data, now))
        .filter(|keys| keys.start <= window.to && keys.end >= window.from)
        .filter(|keys| {
            search.as_ref().is_none_or(|search| {
                let haystack = keys.haystack(&data.repository);
                search
                    .split_whitespace()
                    .all(|word| haystack.contains(word))
            })
        })
        .collect::<Vec<_>>();
    let score = |keys: &Keys| keys.score.unwrap_or(-1.0);
    keys.sort_by(|left, right| {
        let order = match sort {
            "oldest" => left.start.cmp(&right.start),
            "duration" => (right.end - right.start).cmp(&(left.end - left.start)),
            "violations" => right.violations.cmp(&left.violations),
            "autonomy_score" => score(right).total_cmp(&score(left)),
            "budget" => right.budget.total_cmp(&left.budget),
            "risk" => right.risk.total_cmp(&left.risk),
            "status" => left.rank.cmp(&right.rank),
            _ => right.start.cmp(&left.start),
        };
        order
            .then_with(|| right.start.cmp(&left.start))
            .then_with(|| left.task.id.cmp(&right.task.id))
    });
    let total = keys.len();
    let pages = total.div_ceil(page_size).max(1);
    let items = keys
        .iter()
        .skip((page - 1).saturating_mul(page_size))
        .take(page_size)
        .map(|keys| keys.row(&data.repository))
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
    let mut newest = data.tasks.iter().collect::<Vec<_>>();
    newest.sort_by_key(|task| std::cmp::Reverse(task.received));
    facets
        .entry("task")
        .or_default()
        .extend(newest.iter().take(FACET_LIMIT).map(|task| task.id.clone()));
    facets
        .entry("task_status")
        .or_default()
        .extend(data.statuses.iter().cloned());
    facets
        .entry("repository")
        .or_default()
        .insert(data.repository.clone());
    let (facets, mut truncated) = bounded(facets);
    if data.tasks.len() > FACET_LIMIT && !truncated.contains(&"task".to_string()) {
        truncated.push("task".into());
    }
    Ok(json!({
        "now": now,
        "window": {"range": window.name, "from": window.from, "to": window.to},
        "total": total,
        "page": page,
        "pages": pages,
        "page_size": page_size,
        "sort": sort,
        "sorts": SORTS,
        "items": items,
        "facets": facets,
        "facets_truncated": truncated,
    }))
}
