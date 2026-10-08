// Read-only observability: typed queries over the records the runtime and the control plane already
// keep (session bindings and journals, the orchestrator, delivery journals, task records, task
// contracts, completions, the repository connection, and the audit logs). Every answer is a
// projection of those owners' records, read through their pure readers and the evidence
// projection; nothing here writes. The projections are held in memory (a compact per-session
// projection, a snapshot of every record, an index of the tasks) and recomputed whenever a source
// file changes, so they never become a second model. The router accepts GET only, on a fixed
// table of endpoints with typed parameters, and answers only what the caller's scope admits
// (see OBSERVABILITY.md).

pub(crate) mod access;
mod agents;
mod autonomy;
mod cache;
mod catalog;
mod compact;
pub(crate) mod dashboard;
mod detail;
mod repos;
mod runs;
pub(crate) mod server;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{json, Value};

use crate::agent_session::Governance;
use crate::session::{session_ids, ContractSession};

/** Version of the observability API layout */
pub(crate) const OBSERVE_FORMAT: u64 = 1;

/** Violation severities, from most to least severe */
pub(crate) const SEVERITIES: &[&str] = &["critical", "high", "medium", "low"];

/** Largest offset a list accepts */
const MAX_OFFSET: usize = 10_000_000;

/** Default and largest number of items a list returns */
const LIMITS: (usize, usize) = (100, 1000);

/** The endpoints: path pattern, accepted parameters, and the questions it answers */
pub(crate) const ENDPOINTS: &[(&str, &[&str], &str)] = &[
    ("/api/v1/overview", &[], "headline counts across repositories, agents, tasks, sessions, decisions, violations, budget, delivery, and completion"),
    ("/api/v1/repositories", &[], "which repositories are connected"),
    ("/api/v1/agents", &[], "which agents operate and which provider and models each uses"),
    ("/api/v1/tasks", &["state", "limit", "offset"], "which tasks run, and their PR, approval, merge, and completion"),
    ("/api/v1/tasks/{task}", &[], "one task end to end"),
    ("/api/v1/sessions", &["agent", "task", "lifecycle", "limit", "offset"], "which sessions ran, autonomy exercised, budget, verification"),
    ("/api/v1/sessions/{session}", &[], "one session end to end with its evidence"),
    ("/api/v1/sessions/{session}/actions", &["decision", "limit", "offset"], "what one agent did and how each action was decided"),
    ("/api/v1/sessions/{session}/autonomy", &[], "how autonomy, safety, and budget changed"),
    ("/api/v1/decisions", &["decision", "session", "task", "agent", "policy", "zone", "limit", "offset"], "allowed, denied, and approval-required actions, with the policies and zones involved"),
    ("/api/v1/violations", &["severity", "session", "task", "limit", "offset"], "how many violations occurred and how severe they were"),
    ("/api/v1/audit", &[], "the integrity of every journal"),
    ("/api/v1/dashboard/runs", runs::PARAMETERS, "the Runs page: one row per run (governed session) for a window and filters, searched (q), sorted (newest, oldest, duration, actions, violations, budget), and paginated (page, page_size)"),
    ("/api/v1/dashboard/runs/{run}", &[], "one run: its bindings, timeline, and an inspection of every action (digests and normalized summaries, never raw arguments); a long run's first 2000 events, the rest from its events endpoint"),
    ("/api/v1/dashboard/runs/{run}/events", &["after", "limit", "kind"], "a run's journal events after a sequence number (keyset paging on the immutable sequence; next names the cursor of the following page)"),
    ("/api/v1/dashboard/policies", catalog::PARAMETERS, "the Policies page: one row per policy (active, retired, proposed, or bound by a run) with how runs met it"),
    ("/api/v1/dashboard/policies/{policy}", catalog::PARAMETERS, "one policy: metadata, version, digest, rules as semantic selectors, affected repositories, zones, tasks, and agents, its counts, and its timeline"),
    ("/api/v1/dashboard/zones", catalog::PARAMETERS, "the Zones page: one row per zone with its resources and how runs met it"),
    ("/api/v1/dashboard/zones/{zone}", catalog::PARAMETERS, "one zone: its definition, semantic resources as last resolved, and history"),
    ("/api/v1/dashboard/autonomy", autonomy::PARAMETERS, "the Autonomy and Risk page: autonomy score, modes, safety, budget, and the intervention, denial, violation, verification, quarantine, and autonomous completion rates, with their series, distributions, and quarantine and degradation events"),
    ("/api/v1/dashboard/violations", catalog::VIOLATION_PARAMETERS, "the violations explorer: every violation for a window and filters (status resolved or unresolved), with the severity trend and the top policies, agents, and repositories"),
    ("/api/v1/dashboard/repositories", repos::PARAMETERS, "the Repositories page: one row per connected repository with its health, for a window and filters"),
    ("/api/v1/dashboard/repositories/{repository}", repos::PARAMETERS, "Repository Details: overview, health, activity over time, tasks, agents, sessions, policies, zones, violations, deliveries, and attestations"),
    ("/api/v1/dashboard/agents", agents::PARAMETERS, "the Agents page: one row per agent (adapter profile and model) and one aggregate per provider (Claude, Codex, Gemini, Other), for a window and filters"),
    ("/api/v1/dashboard/agents/{agent}", agents::PARAMETERS, "Agent Details: identity, provider, models, repositories, tasks, sessions, policies and zones encountered, violations, and autonomy, budget, verification, and delivery history"),
    ("/api/v1/dashboard/tasks/{task}", detail::PARAMETERS, "the Task Details page: header, task, contract, agents, autonomy, execution timeline, violations, verification, delivery, and evidence of one task"),
    ("/api/v1/dashboard/tasks", dashboard::TASK_PARAMETERS, "the Tasks page: one row per task for a window and filters, searched (q), sorted (newest, oldest, duration, violations, autonomy_score, budget, risk, status), and paginated (page, page_size) on the server"),
    ("/api/v1/dashboard/overview", dashboard::PARAMETERS, "the global Overview: metrics, trends, active now, and recent activity for a time window and filters (multi-select values are comma-separated)"),
    ("/api/v1/endpoints", &[], "this list"),
    ("/api/v1/whoami", &[], "who is reading and what the credential may read (organizations, repositories, teams); never the credential itself"),
];

/** Typed query parameters of one request
 * Fields
    - values: BTreeMap<String, String> - parameters, each from the endpoint's allow-list
*/
#[derive(Debug)]
pub(crate) struct Query {
    values: BTreeMap<String, String>,
}

impl Query {
    /** Parse a query string against an endpoint's allow-list; repeated, unknown, empty, or badly
     * encoded parameters are refused
     * Input
        - text: &str - text after "?" (may be empty)
        - allowed: &[&str] - accepted parameter names
     * Output
        - Result<Query, String>
    */
    pub(crate) fn parse(text: &str, allowed: &[&str]) -> Result<Self, String> {
        let mut values = BTreeMap::new();
        for pair in text.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| format!("parameter '{pair}' has no value"))?;
            if !allowed.contains(&key) {
                return Err(if allowed.is_empty() {
                    format!("this endpoint takes no parameters (got '{key}')")
                } else {
                    format!(
                        "unknown parameter '{key}'; this endpoint accepts {}",
                        allowed.join(", ")
                    )
                });
            }
            let value = decode(value)?;
            if value.is_empty() {
                return Err(format!("parameter '{key}' is empty"));
            }
            if value.len() > 1000 {
                return Err(format!("parameter '{key}' is longer than 1000 characters"));
            }
            if values.insert(key.to_string(), value).is_some() {
                return Err(format!("parameter '{key}' is given twice"));
            }
        }
        let query = Self { values };
        query.limit()?;
        query.choice("decision", &["allow", "deny", "approval_required"])?;
        query.choice("severity", SEVERITIES)?;
        query.choice("lifecycle", &["active", "closed", "cancelled", "finalized"])?;
        Ok(query)
    }

    /** Return a parameter
     * Input
        - key: &str - name
     * Output
        - Option<&str>
    */
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /** Check that a parameter, when given, is one of the accepted values
     * Input
        - key: &str - name
        - accepted: &[&str] - values
     * Output
        - Result<(), String>
    */
    fn choice(&self, key: &str, accepted: &[&str]) -> Result<(), String> {
        for value in self.set(key).into_iter().flatten() {
            if !accepted.contains(&value.as_str()) {
                return Err(format!(
                    "{key} must be one of {}, not '{value}'",
                    accepted.join(", ")
                ));
            }
        }
        Ok(())
    }

    /** Return the values of a multi-select parameter (comma-separated), None when not given
     * Input
        - key: &str - name
     * Output
        - Option<BTreeSet<String>>
    */
    pub(crate) fn set(&self, key: &str) -> Option<BTreeSet<String>> {
        self.get(key).map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(String::from)
                .collect()
        })
    }

    /** Return the list limit: the default, or the given number up to the maximum
     * Input
        - None (uses self)
     * Output
        - Result<usize, String>
    */
    fn limit(&self) -> Result<usize, String> {
        match self.get("limit") {
            None => Ok(LIMITS.0),
            Some(text) => match text.parse::<usize>() {
                Ok(limit) if (1..=LIMITS.1).contains(&limit) => Ok(limit),
                _ => Err(format!(
                    "limit must be a number from 1 to {}, not '{text}'",
                    LIMITS.1
                )),
            },
        }
    }

    /** Return the list offset: 0, or the given number of items to skip (bounded)
     * Input
        - None (uses self)
     * Output
        - Result<usize, String>
    */
    fn offset(&self) -> Result<usize, String> {
        match self.get("offset") {
            None => Ok(0),
            Some(text) => text
                .parse::<usize>()
                .ok()
                .filter(|offset| *offset <= MAX_OFFSET)
                .ok_or_else(|| {
                    format!("offset must be a number from 0 to {MAX_OFFSET}, not '{text}'")
                }),
        }
    }

    /** Check that a field matches the parameter of the same name, when that parameter is given
     * Input
        - key: &str - parameter name
        - value: &Value - field value
     * Output
        - bool
    */
    fn admits(&self, key: &str, value: &Value) -> bool {
        self.set(key)
            .is_none_or(|wanted| value.as_str().is_some_and(|value| wanted.contains(value)))
    }
}

/** Decode %XX escapes and "+" in a query value
 * Input
    - text: &str - encoded value
 * Output
    - Result<String, String>
*/
fn decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = text
                    .get(index + 1..index + 3)
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                    .ok_or_else(|| format!("bad escape in '{text}'"))?;
                out.push(hex);
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| format!("'{text}' is not UTF-8"))
}

/** Check that a path segment names a record (session or task id), so no path can reach anything
 * else
 * Input
    - segment: &str - path segment
 * Output
    - Result<&str, String>
*/
fn identifier(segment: &str) -> Result<&str, String> {
    let valid = !segment.is_empty()
        && segment.len() <= 128
        && !segment.starts_with('.')
        && segment
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character));
    valid
        .then_some(segment)
        .ok_or_else(|| format!("'{segment}' is not a valid identifier"))
}

/** The model providers the dashboard groups agents by, in display order */
pub(crate) const PROVIDERS: &[&str] = &["Claude", "Codex", "Gemini", "Other"];

/** Name the provider family of an agent: its adapter profile decides for Claude Code and Codex;
 * a generic agent is placed by the models it reported, and is Other otherwise
 * Input
    - agent: &str - agent profile
    - models: &[String] - models the session reported
 * Output
    - &'static str one of PROVIDERS
*/
fn provider(agent: &str, models: &[String]) -> &'static str {
    match agent {
        "claude" => "Claude",
        "codex" => "Codex",
        _ => {
            let named = |words: &[&str]| {
                models.iter().any(|model| {
                    let model = model.to_lowercase();
                    words.iter().any(|word| model.contains(word))
                })
            };
            if named(&["gemini"]) {
                "Gemini"
            } else if named(&["claude"]) {
                "Claude"
            } else if named(&["codex", "gpt", "o3", "o4"]) {
                "Codex"
            } else {
                "Other"
            }
        }
    }
}

/** Turn text into the characters an identifier may hold, for agent ids built from model names
 * Input
    - text: &str - text
 * Output
    - String
*/
pub(crate) fn slug(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || "._-".contains(character) {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_start_matches('.')
        .to_string()
}

/** One session (run) read in full from its journal: what the pages of one run or one task show,
 * and what the compact projection kept for every session is derived from
 * Fields
    - id: String - session id
    - document: Value - bound document (session.json)
    - described: Value - description with lifecycle, drift, and activity
    - events: Vec<Value> - journal
    - records: Vec<Value> - evidence records
    - attestation: Value - attestation computed from the journal
    - critical: BTreeSet<String> - violation kinds the bound autonomy policy treats as critical
    - phase: Value - orchestrator phase, null when the orchestrator does not govern it
    - delivery: Value - delivery state, null when nothing was delivered
    - environment: &'static str - "worktree" when the session works in an isolated worktree,
      otherwise "repository"
    - summarized: OnceLock<Value> - its summary, computed once
*/
#[derive(Default)]
struct Full {
    id: String,
    document: Value,
    described: Value,
    events: Vec<Value>,
    records: Vec<Value>,
    attestation: Value,
    critical: BTreeSet<String>,
    phase: Value,
    delivery: Value,
    environment: &'static str,
    summarized: OnceLock<Value>,
}

impl Full {
    /** Read one session through the owners' pure readers
     * Input
        - id: &str - session id
     * Output
        - Result<Option<Full>, String>
    */
    fn read(id: &str) -> Result<Option<Self>, String> {
        let Some(session) = ContractSession::load(id)? else {
            return Ok(None);
        };
        let document = session.document();
        let events = session.events();
        let records = crate::evidence::records(&document, &events)?;
        let attestation = crate::evidence::attest(&document, &events)?;
        let critical = Governance::from_json(&document["governance"])?
            .autonomy_policy
            .unwrap_or_default()
            .critical;
        let delivery = crate::delivery::journal(id)?;
        Ok(Some(Self {
            id: id.to_string(),
            summarized: OnceLock::new(),
            environment: if crate::effects::workspace_of(&session).is_some() {
                "worktree"
            } else {
                "repository"
            },
            described: session.describe(),
            document,
            events,
            records,
            attestation,
            critical,
            phase: crate::session_orchestrator::load(id)?
                .map_or(Value::Null, |record| record["phase"].clone()),
            delivery: if delivery.is_empty() {
                Value::Null
            } else {
                crate::delivery::state(&delivery)
            },
        }))
    }

    /** Return the agent profile
     * Input
        - None (uses self)
     * Output
        - String
    */
    fn agent(&self) -> String {
        self.document["agent"]
            .as_str()
            .unwrap_or("unknown")
            .to_string()
    }

    /** Return the models the session reported (session starts and the attestation)
     * Input
        - None (uses self)
     * Output
        - Vec<String> in the order first reported
    */
    fn models(&self) -> Vec<String> {
        let mut models: Vec<String> = Vec::new();
        for model in self
            .events
            .iter()
            .filter_map(|event| event["model"].as_str())
            .chain(
                self.attestation["agent"]["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            )
        {
            if !models.iter().any(|known| known == model) {
                models.push(model.to_string());
            }
        }
        models
    }

    /** Return the model the session ran last, "default" when it reported none
     * Input
        - None (uses self)
     * Output
        - String
    */
    fn model(&self) -> String {
        self.events
            .iter()
            .rev()
            .find_map(|event| event["model"].as_str().map(String::from))
            .or_else(|| self.models().pop())
            .unwrap_or_else(|| "default".into())
    }

    /** Return the session's provider family
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    fn provider(&self) -> &'static str {
        provider(&self.agent(), &self.models())
    }

    /** Return the id of the agent the session ran as: its adapter profile and model
     * Input
        - None (uses self)
     * Output
        - String such as "claude.claude-opus-5-5"
    */
    fn agent_id(&self) -> String {
        format!("{}.{}", self.agent(), slug(&self.model()))
    }

    /** Return the Crane version the session's adapter ran with, if recorded
     * Input
        - None (uses self)
     * Output
        - Option<String>
    */
    fn adapter_version(&self) -> Option<String> {
        self.events
            .iter()
            .rev()
            .find_map(|event| event["crane_version"].as_str().map(String::from))
    }

    /** Return the bound task id, if any
     * Input
        - None (uses self)
     * Output
        - Value
    */
    fn task(&self) -> Value {
        self.document["task_id"].clone()
    }

    /** Classify a violation: critical when the bound autonomy policy treats its kind as critical
     * (it quarantines); high when the autonomy state machine acted on it (it degrades); low for a
     * final-reconciliation finding; medium otherwise (found when verifying an executed tool)
     * Input
        - kind: &str - violation kind
        - event: &str - event it was found in
     * Output
        - &'static str critical, high, medium, or low
    */
    fn severity(&self, kind: &str, event: &str) -> &'static str {
        if self.critical.contains(kind) {
            "critical"
        } else {
            match event {
                "autonomy" => "high",
                "session_finalized" => "low",
                _ => "medium",
            }
        }
    }

    /** List the session's violations with their severity
     * Input
        - None (uses self)
     * Output
        - Vec<Value>
    */
    fn violations(&self) -> Vec<Value> {
        self.records
            .iter()
            .flat_map(|record| {
                record["violations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(move |kind| {
                        let kind = kind.as_str().map(String::from).unwrap_or_else(|| {
                            kind["violation_type"]
                                .as_str()
                                .unwrap_or("unknown")
                                .to_string()
                        });
                        let event = record["event"].as_str().unwrap_or_default();
                        json!({
                            "event_id": format!("{}:{}", self.id, record["seq"]),
                            "session_id": self.id,
                            "task_id": self.task(),
                            "agent": self.agent(),
                            "seq": record["seq"],
                            "at": record["at"],
                            "event": event,
                            "kind": kind,
                            "severity": self.severity(&kind, event),
                        })
                    })
            })
            .collect()
    }

    /** Project one evidence record of an authorization into an action
     * Input
        - record: &Value - evidence record
     * Output
        - Value
    */
    fn action(&self, record: &Value) -> Value {
        let list = |value: &Value| {
            if value.is_array() {
                value.clone()
            } else {
                json!([])
            }
        };
        json!({
            "event_id": format!("{}:{}", self.id, record["seq"]),
            "session_id": self.id,
            "task_id": self.task(),
            "agent": self.agent(),
            "seq": record["seq"],
            "at": record["at"],
            "event": record["event"],
            "tool": record["tool"],
            "operation": record["operation"],
            "resources": list(&record["resources"]),
            "summary": record["action_summary"],
            "action_digest": record["action_digest"],
            "decision": record["decision"],
            "reasons": list(&record["reasons"]),
            "policies": list(&record["policies"]),
            "zones": list(&record["zones"]),
            "autonomy": record["autonomy"],
            "safety": record["safety"],
            "budget": record["budget"],
            "chain": record["chain"],
        })
    }

    /** List the session's authorization decisions as actions
     * Input
        - None (uses self)
     * Output
        - Vec<Value>
    */
    fn actions(&self) -> Vec<Value> {
        self.records
            .iter()
            .filter(|record| record["category"] == "authorization")
            .map(|record| self.action(record))
            .collect()
    }

    /** Summarize the session: who, what task, lifecycle, autonomy exercised, decisions,
     * violations, budget, verification, tests, delivery, and evidence
     * Input
        - None (uses self)
     * Output
        - Value
    */
    fn summary(&self) -> Value {
        self.summarized.get_or_init(|| self.summarize()).clone()
    }

    /** Compute the summary (see summary)
     * Input
        - None (uses self)
     * Output
        - Value
    */
    fn summarize(&self) -> Value {
        let actions = self.actions();
        let count = |decision: &str| {
            actions
                .iter()
                .filter(|action| action["decision"] == decision)
                .count()
        };
        let order = ["observe", "assisted", "delegated", "autonomous"];
        let rank = |name: &str| order.iter().position(|level| *level == name);
        let mut exercised: BTreeMap<String, u64> = BTreeMap::new();
        for action in actions
            .iter()
            .filter(|action| action["decision"] == "allow" && action["operation"] != "read")
        {
            *exercised
                .entry(action["autonomy"].as_str().unwrap_or("unknown").to_string())
                .or_default() += 1;
        }
        let highest = exercised
            .keys()
            .filter_map(|level| rank(level))
            .max()
            .map(|index| order[index]);
        let violations = self.violations();
        let severity = |level: &str| {
            violations
                .iter()
                .filter(|violation| violation["severity"] == level)
                .count()
        };
        let activity = &self.described["activity"];
        let attestation = &self.attestation;
        let verification = self
            .events
            .iter()
            .rev()
            .find_map(|event| event["verification"].as_str().map(String::from));
        let contract_tests = &attestation["contract_tests"];
        let repository_tests = &attestation["ordinary_tests"];
        let statuses = repository_tests
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|test| test["status"].as_str())
            .collect::<Vec<_>>();
        let delivery = &self.delivery;
        json!({
            "session_id": self.id,
            "agent": self.agent(),
            "provider": self.provider(),
            "agent_id": self.agent_id(),
            "models": attestation["agent"]["models"],
            "task_id": self.task(),
            "task_title": self.document["governance"]["task_title"],
            "lifecycle": self.described["lifecycle"],
            "phase": self.phase,
            "started_at": activity["started_at"],
            "last_activity_at": activity["last_activity_at"],
            "ended_at": activity["ended_at"],
            "autonomy": {
                "initial": activity["initial_autonomy"],
                "current": activity["autonomy"],
                "highest_exercised": highest,
                "allowed_mutating_actions_by_level": exercised,
            },
            "safety": {"state": activity["safety_state"], "reason": activity["safety_reason"]},
            "actions": {
                "total": actions.len(),
                "allow": count("allow"),
                "deny": count("deny"),
                "approval_required": count("approval_required"),
                "executed": attestation["action_summary"]["executed"],
            },
            "violations": {
                "total": violations.len(),
                "critical": severity("critical"),
                "high": severity("high"),
                "medium": severity("medium"),
                "low": severity("low"),
            },
            "budget": {
                "consumed": attestation["action_summary"]["budget_consumed"],
                "final": attestation["final_state"]["budget"],
                "mutating_actions": activity["budget"]["mutating_actions"],
                "files": activity["budget"]["files"],
            },
            "verification": {
                "last": verification,
                "final_decision": attestation["final_decision"]["decision"],
                "contract_tests": contract_tests,
                "contract_tests_passed": contract_tests["failed"].as_u64().map(|failed| failed == 0),
                "repository_tests": repository_tests,
                "repository_tests_passed": if statuses.contains(&"failed") { json!(false) } else if statuses.contains(&"passed") { json!(true) } else { Value::Null },
            },
            "delivery": if delivery.is_null() { Value::Null } else { json!({
                "pull_request": delivery["pull_request"]["url"].as_str().map(String::from).or_else(|| delivery["pull_request"]["number"].as_u64().map(|number| format!("#{number}"))),
                "pull_request_created": !delivery["pull_request"].is_null(),
                "approvals": delivery["approvals"].as_array().map_or(0, Vec::len),
                "approved": delivery["approvals"].as_array().is_some_and(|approvals| !approvals.is_empty()),
                "blocks": delivery["blocks"].as_array().map_or(0, Vec::len),
                "exceptions": delivery["exceptions"].as_array().map_or(0, Vec::len),
                "merged": !delivery["merged"].is_null(),
                "merge_sha": delivery["merged"]["sha"],
                "task_completion_queued": !delivery["task_completion"].is_null(),
            })},
            "evidence": {
                "attestation_digest": attestation["attestation_digest"],
                "stored_attestation_digest": crate::session::read_attestation(&self.id).ok().flatten().map(|stored| stored["attestation_digest"].clone()),
                "journal_events": self.events.len(),
                "journal_chain": attestation["evidence"]["chain"]["status"],
                "delivery_chain": delivery["chain"]["status"],
            },
        })
    }

    /** The session's autonomy, safety, and budget timeline
     * Input
        - None (uses self)
     * Output
        - Value
    */
    fn timeline(&self) -> Value {
        let mut previous: Option<(Value, Value)> = None;
        let mut changes = Vec::new();
        let mut budget = Vec::new();
        for (record, event) in self.records.iter().zip(&self.events) {
            let now = (record["autonomy"].clone(), record["safety"].clone());
            if previous.as_ref() != Some(&now) {
                changes.push(json!({
                    "seq": record["seq"],
                    "at": record["at"],
                    "event": record["event"],
                    "autonomy": now.0,
                    "safety": now.1,
                    "from": previous.as_ref().map(|(autonomy, safety)| json!({"autonomy": autonomy, "safety": safety})),
                    "trigger": event["trigger"],
                    "kind": event["kind"],
                    "actor": event["actor"],
                }));
                previous = Some(now);
            }
            if record["budget"]["before"] != record["budget"]["after"] {
                budget.push(json!({"seq": record["seq"], "at": record["at"], "event": record["event"], "before": record["budget"]["before"], "after": record["budget"]["after"]}));
            }
        }
        json!({"session_id": self.id, "autonomy_changes": changes, "budget_changes": budget})
    }

    /** Derive the compact projection every page that spans sessions reads: the summary,
     * violations, and identity computed from the full journal, one Act per event, and the
     * bound document without what only the full view needs
     * Input
        - None (uses self)
     * Output
        - Run
    */
    fn compact(&self) -> Run {
        let summary = self.summary();
        let violation_list = self.violations();
        let mut document = self.document.clone();
        let governance = compact::share(document["governance"].take());
        let mut contracts = document["contracts"].take();
        for contract in contracts["contracts"].as_array_mut().into_iter().flatten() {
            if let Some(object) = contract.as_object_mut() {
                object.remove("clauses");
            }
        }
        if let Some(object) = document.as_object_mut() {
            object.remove("grants");
            object.remove("governance");
            object.remove("contracts");
        }
        let attestation = &self.attestation;
        let mut run = Run {
            id: self.id.clone(),
            acts: self
                .records
                .iter()
                .zip(&self.events)
                .map(|(record, event)| compact::Act::project(record, event))
                .collect(),
            chain: crate::evidence::verify_chain(&self.events),
            attestation: json!({
                "attestation_digest": attestation["attestation_digest"],
                "final_decision": attestation["final_decision"],
                "evidence": {"chain": attestation["evidence"]["chain"]},
                "contract": {"policies": attestation["contract"]["policies"]},
            }),
            models: self.models(),
            model: self.model(),
            adapter_version: self.adapter_version(),
            governance,
            contracts: compact::share(contracts),
            document,
            described: self.described.clone(),
            delivery: self.delivery.clone(),
            environment: self.environment,
            violation_list,
            violation_details: detail::violation_details(self),
            flow: self.budget_flow(),
            facts: dashboard::Facts::default(),
            summarized: summary,
        };
        run.facts = dashboard::Facts::derive(&run);
        run
    }

    /** Replay the session's budget through its bound budget model, event by event, with the
     * runtime's own operations (deterministic: it depends only on the journal and the binding)
     * Input
        - None (uses self)
     * Output
        - Vec<(u64, u64, u64, u64)> time, points consumed, regenerated, and refilled by a human,
          for each event that changed any of them
    */
    fn budget_flow(&self) -> Vec<(u64, u64, u64, u64)> {
        let Ok(governance) = Governance::from_json(&self.document["governance"]) else {
            return Vec::new();
        };
        let model = governance.budget_model.clone().unwrap_or_default();
        let mut budget = crate::budget::BudgetState::initial(model.max_for(governance.autonomy));
        self.events
            .iter()
            .map(|event| {
                let at = event["at"].as_u64().unwrap_or(0);
                budget.expire(at);
                let before = (budget.consumed, budget.regenerated, budget.refilled);
                for operation in crate::budget::manage::operations(&model, event) {
                    budget.apply(&operation, at);
                }
                (
                    at,
                    budget.consumed.saturating_sub(before.0),
                    budget.regenerated.saturating_sub(before.1),
                    budget.refilled.saturating_sub(before.2),
                )
            })
            .filter(|(_, consumed, regenerated, refilled)| consumed + regenerated + refilled > 0)
            .collect()
    }
}

/** One session (run) as every page that spans sessions reads it: a compact projection derived
 * from the full journal when the session's files change (see Full and compact::Act)
 * Fields
    - id: String - session id
    - document: Value - bound document, without governance, contracts, and grants
    - governance: Arc<Value> - bound governance (shared between sessions bound to the same)
    - contracts: Arc<Value> - bound contract set without its clauses (shared likewise)
    - described: Value - description with lifecycle, drift, and activity
    - acts: Vec<compact::Act> - one per journal event, in order
    - attestation: Value - the attestation's digest, final decision, journal chain, and policies
    - chain: Value - the journal's hash-chain verification
    - delivery: Value - delivery state, null when nothing was delivered
    - environment: &'static str - "worktree" or "repository"
    - models: Vec<String> - models the session reported, in the order first reported
    - model: String - the model it ran last, "default" when it reported none
    - adapter_version: Option<String> - Crane version its adapter ran with
    - violation_list: Vec<Value> - its violations with their severity
    - violation_details: Vec<Value> - its violations in detail (cause, consequence, resolution)
    - flow: Vec<(u64, u64, u64, u64)> - its budget replayed: time, points consumed, regenerated,
      and refilled, for each event that changed them
    - facts: dashboard::Facts - what the pages need of it, derived once
    - summarized: Value - its summary
*/
struct Run {
    id: String,
    document: Value,
    governance: Arc<Value>,
    contracts: Arc<Value>,
    described: Value,
    acts: Vec<compact::Act>,
    attestation: Value,
    chain: Value,
    delivery: Value,
    environment: &'static str,
    models: Vec<String>,
    model: String,
    adapter_version: Option<String>,
    violation_list: Vec<Value>,
    violation_details: Vec<Value>,
    flow: Vec<(u64, u64, u64, u64)>,
    facts: dashboard::Facts,
    summarized: Value,
}

impl Run {
    /** Read one session and derive its compact projection
     * Input
        - id: &str - session id
     * Output
        - Result<Option<Run>, String>
    */
    fn read(id: &str) -> Result<Option<Self>, String> {
        Ok(Full::read(id)?.map(|full| full.compact()))
    }

    /** Return the files and directories a session's projection is computed from, the first
     * being its directory (the cache key), under a known .crane directory
     * Input
        - root: &std::path::Path - the .crane directory
        - id: &str - session id
     * Output
        - Vec<PathBuf>
    */
    fn sources_in(root: &std::path::Path, id: &str) -> Vec<PathBuf> {
        let runtime = root.join("runtime");
        vec![
            runtime.join("sessions").join(id),
            runtime.join("delivery").join(id).join("journal.jsonl"),
            runtime.join("orchestrator").join(format!("{id}.json")),
        ]
    }

    /** Read one session through the cache (recomputed only when its files changed), if the
     * current request's scope admits it; a session it does not admit reads as absent, exactly
     * like one that does not exist
     * Input
        - id: &str - session id
     * Output
        - Result<Option<Arc<Run>>, String>
    */
    fn cached(id: &str) -> Result<Option<Arc<Self>>, String> {
        let organization = repository_tenant()?.0;
        if let Some(snapshot) = snapshot() {
            return Ok(snapshot
                .index
                .get(id)
                .map(|position| snapshot.runs[*position].clone())
                .filter(|run| Self::admitted(run, organization.as_deref())));
        }
        Self::cached_in(&crane_root()?, id, organization.as_deref())
    }

    /** Check whether the current request's scope admits a session
     * Input
        - run: &Run - the session
        - organization: Option<&str> - the repository's organization
     * Output
        - bool
    */
    fn admitted(run: &Run, organization: Option<&str>) -> bool {
        let (owner, team) = run.tenant(organization);
        access::current().admits_record(owner.as_deref(), team.as_deref())
    }

    /** Read one session through the cache, if the current scope admits it (see cached), under a
     * known .crane directory and repository organization
     * Input
        - root: &std::path::Path - the .crane directory
        - id: &str - session id
        - organization: Option<&str> - the repository's organization
     * Output
        - Result<Option<Arc<Run>>, String>
    */
    fn cached_in(
        root: &std::path::Path,
        id: &str,
        organization: Option<&str>,
    ) -> Result<Option<Arc<Self>>, String> {
        let sources = Self::sources_in(root, id);
        let run = cache::run(sources[0].clone(), sources, || Self::read(id))?;
        Ok(run.filter(|run| Self::admitted(run, organization)))
    }

    /** Return the tenant a session belongs to: the organization and team its governance
     * recorded (the evidence records carry the same), the repository's organization when it
     * recorded none
     * Input
        - repository: Option<&str> - the repository's organization
     * Output
        - (Option<String>, Option<String>) organization and team
    */
    fn tenant(&self, repository: Option<&str>) -> (Option<String>, Option<String>) {
        let organization = &self.governance["organization"];
        let text = |value: &Value| {
            value
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(String::from)
        };
        (
            text(&organization["organization"]).or_else(|| repository.map(String::from)),
            text(&organization["team"]),
        )
    }

    /** Read every session the current scope admits (through the cache); a session whose files
     * cannot be read is left out rather than failing every page (the audit lists it)
     * Input
        - None
     * Output
        - Result<Vec<Arc<Run>>, String>
    */
    fn all() -> Result<Vec<Arc<Self>>, String> {
        Ok(Self::scan()?.0)
    }

    /** Read every session the current scope admits, and name the ones that cannot be read
     * Input
        - None
     * Output
        - Result<(Vec<Arc<Run>>, Vec<Value>), String> the sessions, and {session_id, problem} for
          each unreadable one
    */
    fn scan() -> Result<(Vec<Arc<Self>>, Vec<Value>), String> {
        if let Some(snapshot) = snapshot() {
            let (organization, _) = repository_tenant()?;
            let runs = snapshot
                .runs
                .iter()
                .filter(|run| Self::admitted(run, organization.as_deref()))
                .cloned()
                .collect();
            return Ok((runs, snapshot.unreadable.clone()));
        }
        let mut runs = Vec::new();
        let mut unreadable = Vec::new();
        let mut keys = Vec::new();
        let root = crane_root()?;
        let ids = cache::listing(&format!("sessions:{}", root.display()), session_ids)?;
        let (organization, _) = repository_tenant()?;
        for id in ids.iter() {
            keys.push(Self::sources_in(&root, id).remove(0));
            match Self::cached_in(&root, id, organization.as_deref()) {
                Ok(Some(run)) => runs.push(run),
                Ok(None) => {}
                Err(problem) => unreadable.push(json!({"session_id": id, "problem": problem})),
            }
        }
        cache::retain_runs(&keys);
        Ok((runs, unreadable))
    }

    /** Read one session in full from its journal, if the current scope admits it (the pages of
     * one run or task; the compact projection serves everything else)
     * Input
        - id: &str - session id
     * Output
        - Result<Option<Full>, String>
    */
    fn full(id: &str) -> Result<Option<Full>, String> {
        if Self::cached(id)?.is_none() {
            return Ok(None);
        }
        Full::read(id)
    }

    /** Return the bound governance
     * Input
        - None (uses self)
     * Output
        - &Value
    */
    fn governance(&self) -> &Value {
        &self.governance
    }

    /** Return the bound contracts (each policy's id, version, hash, and checkpoint)
     * Input
        - None (uses self)
     * Output
        - &Value - list
    */
    fn contracts(&self) -> &Value {
        &self.contracts["contracts"]
    }

    /** Return the agent profile
     * Input
        - None (uses self)
     * Output
        - String
    */
    fn agent(&self) -> String {
        self.document["agent"]
            .as_str()
            .unwrap_or("unknown")
            .to_string()
    }

    /** Return the models the session reported
     * Input
        - None (uses self)
     * Output
        - Vec<String> in the order first reported
    */
    fn models(&self) -> Vec<String> {
        self.models.clone()
    }

    /** Return the model the session ran last, "default" when it reported none
     * Input
        - None (uses self)
     * Output
        - String
    */
    fn model(&self) -> String {
        self.model.clone()
    }

    /** Return the session's provider family
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    fn provider(&self) -> &'static str {
        provider(&self.agent(), &self.models)
    }

    /** Return the id of the agent the session ran as: its adapter profile and model
     * Input
        - None (uses self)
     * Output
        - String such as "claude.claude-opus-5-5"
    */
    fn agent_id(&self) -> String {
        format!("{}.{}", self.agent(), slug(&self.model))
    }

    /** Return the Crane version the session's adapter ran with, if recorded
     * Input
        - None (uses self)
     * Output
        - Option<String>
    */
    fn adapter_version(&self) -> Option<String> {
        self.adapter_version.clone()
    }

    /** Return the bound task id, if any
     * Input
        - None (uses self)
     * Output
        - Value
    */
    fn task(&self) -> Value {
        self.document["task_id"].clone()
    }

    /** List the session's violations with their severity
     * Input
        - None (uses self)
     * Output
        - Vec<Value>
    */
    fn violations(&self) -> Vec<Value> {
        self.violation_list.clone()
    }

    /** Return the session's summary
     * Input
        - None (uses self)
     * Output
        - Value
    */
    fn summary(&self) -> Value {
        self.summarized.clone()
    }

    /** Project one authorization act into an action (the full journal adds its digest,
     * summary, and chain link: see Full::action)
     * Input
        - act: &compact::Act - the act
     * Output
        - Value
    */
    fn action(&self, act: &compact::Act) -> Value {
        json!({
            "event_id": self.event_id(act.seq),
            "session_id": self.id,
            "task_id": self.task(),
            "agent": self.agent(),
            "seq": act.seq,
            "at": act.at,
            "event": act.event,
            "tool": act.tool,
            "operation": act.operation,
            "resources": act.resources,
            "decision": act.decision,
            "reasons": act.reasons,
            "policies": act.policies,
            "zones": act.zones,
            "autonomy": act.autonomy,
            "safety": act.safety,
            "budget": {"before": act.before.map(compact::Budget::to_json), "after": act.after.map(compact::Budget::to_json)},
        })
    }

    /** Return an event's immutable id: the session id and its sequence number (the journal is
     * append-only and hash-chained, so a sequence number never names another event)
     * Input
        - seq: u64 - sequence number
     * Output
        - String
    */
    fn event_id(&self, seq: u64) -> String {
        format!("{}:{seq}", self.id)
    }
}

/** The answer of every endpoint: the format marker plus the payload
 * Input
    - payload: Value - JSON object
 * Output
    - Value
*/
fn answer(mut payload: Value) -> Value {
    payload["observe_format"] = json!(OBSERVE_FORMAT);
    payload["read_only"] = json!(true);
    payload
}

/** List the connected repositories (one per .crane)
 * Input
    - None
 * Output
    - Result<Vec<Value>, String>
*/
fn repositories() -> Result<Vec<Value>, String> {
    Ok(crate::repo::load()?
        .into_iter()
        .map(|record| {
            json!({
                "provider": record["provider"],
                "full_name": record["full_name"],
                "default_branch": record["default_branch"],
                "connection_status": record["connection_status"],
                "connected_at": record["connected_at"],
                "discovery": record["discovery"],
            })
        })
        .collect())
}

/** Summarize the agents from their sessions
 * Input
    - runs: &[Arc<Run>] - sessions
 * Output
    - Vec<Value>
*/
fn agents(runs: &[Arc<Run>]) -> Vec<Value> {
    let mut grouped: BTreeMap<String, Vec<&Run>> = BTreeMap::new();
    for run in runs {
        grouped.entry(run.agent()).or_default().push(run);
    }
    grouped
        .into_iter()
        .map(|(agent, runs)| {
            let summaries = runs.iter().map(|run| run.summary()).collect::<Vec<_>>();
            let sum = |key: &str| summaries.iter().map(|summary| summary["actions"][key].as_u64().unwrap_or(0)).sum::<u64>();
            json!({
                "agent": agent,
                "provider": runs.first().map_or("Other", |run| run.provider()),
                "models": summaries.iter().flat_map(|summary| summary["models"].as_array().cloned().unwrap_or_default()).filter_map(|model| model.as_str().map(String::from)).collect::<BTreeSet<_>>(),
                "sessions": runs.len(),
                "active_sessions": summaries.iter().filter(|summary| summary["lifecycle"] == "active").count(),
                "tasks": runs.iter().filter_map(|run| run.task().as_str().map(String::from)).collect::<BTreeSet<_>>(),
                "decisions": {"allow": sum("allow"), "deny": sum("deny"), "approval_required": sum("approval_required")},
                "last_activity_at": summaries.iter().filter_map(|summary| summary["last_activity_at"].as_u64()).max(),
            })
        })
        .collect()
}

/** Project one task: its tracker lifecycle, contract, sessions, delivery, and completion
 * Input
    - record: &Value - orchestration task record (or a stub for a task known only from sessions)
    - runs: &[Arc<Run>] - every session
    - completions: &[Value] - every completion event
    - detail: bool - include the history and full session summaries
 * Output
    - Result<Value, String>
*/
fn task(
    record: &Value,
    runs: &[Arc<Run>],
    completions: &[Value],
    detail: bool,
) -> Result<Value, String> {
    let id = record["task_id"].as_str().unwrap_or_default();
    let listed = record["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let sessions = runs
        .iter()
        .filter(|run| run.task() == id || listed.contains(run.id.as_str()))
        .map(|run| run.summary())
        .collect::<Vec<_>>();
    let contract = contract(id);
    let completion = completions
        .iter()
        .filter(|event| event["task_id"] == id)
        .max_by_key(|event| event["created_at"].as_u64().unwrap_or(0));
    let delivered = sessions
        .iter()
        .filter(|session| !session["delivery"].is_null())
        .collect::<Vec<_>>();
    let mut value = json!({
        "task_id": id,
        "source": record["source"],
        "external_id": record["external_id"],
        "title": contract.as_ref().map(|contract| contract["task"]["title"].clone()).or_else(|| sessions.iter().find_map(|session| session["task_title"].as_str().map(|title| json!(title)))),
        "state": record["state"],
        "reason": record["reason"],
        "contract": contract.as_ref().map(|contract| json!({
            "contract_id": contract["contract_id"],
            "version": contract["version"],
            "status": contract["status"],
            "digest": contract["digest"],
        })),
        "sessions": sessions.iter().map(|session| session["session_id"].clone()).collect::<Vec<_>>(),
        "pull_request_created": delivered.iter().any(|session| session["delivery"]["pull_request_created"] == true),
        "approved": delivered.iter().any(|session| session["delivery"]["approved"] == true),
        "merged": delivered.iter().any(|session| session["delivery"]["merged"] == true),
        "completion": completion.map(|event| json!({
            "event_id": event["event_id"],
            "state": crate::task_completion::lifecycle_state(event),
            "status": event["status"],
            "merge_sha": event["merge_sha"],
        })),
        "completed": completion.is_some_and(|event| crate::task_completion::lifecycle_state(event) == "COMPLETED"),
    });
    if detail {
        value["history"] = record["history"].clone();
        value["session_details"] = json!(sessions);
    }
    Ok(value)
}

/** List the tasks: every orchestrated task plus tasks known only from session bindings
 * Input
    - runs: &[Arc<Run>] - every session
    - detail: bool - include detail
 * Output
    - Result<Vec<Value>, String>
*/
fn tasks(runs: &[Arc<Run>], detail: bool) -> Result<Vec<Value>, String> {
    let completions = completions()?;
    let mut records = task_records()?
        .iter()
        .map(|record| (**record).clone())
        .collect::<Vec<_>>();
    let known = records
        .iter()
        .filter_map(|record| record["task_id"].as_str().map(String::from))
        .collect::<BTreeSet<_>>();
    let bound = runs
        .iter()
        .filter_map(|run| run.task().as_str().map(String::from))
        .filter(|id| !known.contains(id))
        .collect::<BTreeSet<_>>();
    records.extend(
        bound
            .into_iter()
            .map(|id| json!({"task_id": id, "source": null, "state": null, "sessions": []})),
    );
    records
        .iter()
        .map(|record| task(record, runs, &completions, detail))
        .collect()
}

/** Count values into a sorted map
 * Input
    - values: impl Iterator<Item = String> - values
 * Output
    - Value
*/
fn tally(values: impl Iterator<Item = String>) -> Value {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for value in values {
        *counts.entry(value).or_default() += 1;
    }
    json!(counts)
}

/** Return the .crane directory of the working directory, located once per working directory
 * (pages look it up for every record they read; the lookup itself touches the file system)
 * Input
    - None
 * Output
    - Result<PathBuf, String>
*/
pub(crate) fn crane_root() -> Result<PathBuf, String> {
    static ROOT: OnceLock<std::sync::Mutex<Option<(PathBuf, PathBuf)>>> = OnceLock::new();
    let directory = std::env::current_dir().map_err(|error| error.to_string())?;
    let memo = ROOT.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(known) = memo.lock() {
        if let Some((working, root)) = known.as_ref().filter(|(working, _)| *working == directory) {
            let _ = working;
            return Ok(root.clone());
        }
    }
    let root = crate::repository::root()?;
    if let Ok(mut known) = memo.lock() {
        *known = Some((directory, root.clone()));
    }
    Ok(root)
}

/** Return the repository this server observes: its organization (as the evidence records name
 * it: .crane/organization.json, else the connected repository's owner, else the origin remote's)
 * and its full name (the connected repository's, else its directory name); read again only when
 * those files change
 * Input
    - None
 * Output
    - Result<(Option<String>, String), String>
*/
pub(crate) fn repository_tenant() -> Result<(Option<String>, String), String> {
    let crane = crane_root()?;
    let sources = vec![
        crane.join(crate::repo::CONNECTION_FILE),
        crane.join(crate::evidence::ORGANIZATION_FILE),
        crane
            .parent()
            .map(|parent| parent.join(".git").join("config"))
            .unwrap_or_default(),
    ];
    let value = cache::value(format!("tenant:{}", crane.display()), sources, || {
        let connection = crate::repo::load()?;
        let name = connection
            .as_ref()
            .and_then(|record| record["full_name"].as_str().map(String::from))
            .unwrap_or_else(|| {
                crane
                    .parent()
                    .and_then(|parent| parent.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "repository".into())
            });
        let organization = crate::evidence::load_organization()?["organization"].clone();
        Ok(json!({"organization": organization, "repository": name}))
    })?;
    Ok((
        value["organization"].as_str().map(String::from),
        value["repository"]
            .as_str()
            .unwrap_or("repository")
            .to_string(),
    ))
}

/** Read every orchestration task record the current scope admits, each one only again when its
 * file changed. Under a team-restricted scope a task is admitted when its record names an
 * admitted team or one of its sessions is admitted.
 * Input
    - None
 * Output
    - Result<Vec<Arc<Value>>, String> the records, shared with the cache
*/
pub(crate) fn task_records() -> Result<Vec<Arc<Value>>, String> {
    let scope = access::current();
    let (organization, _) = repository_tenant()?;
    let records = every_task_record()?;
    if !scope.restricts_teams() {
        return Ok(if scope.admits_record(organization.as_deref(), None) {
            records
        } else {
            Vec::new()
        });
    }
    let mut admitted = Vec::new();
    for record in records {
        let named = record["team"].as_str();
        let visible = (named.is_some() && scope.admits_record(organization.as_deref(), named))
            || record["sessions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|id| identifier(id).is_ok())
                .any(|id| matches!(Run::cached(id), Ok(Some(_))));
        if visible {
            admitted.push(record);
        }
    }
    Ok(admitted)
}

/** Everything the pages read, as of one background refresh: a request takes it whole, with one
 * lock, instead of checking thousands of files (the server refreshes it every few seconds)
 * Fields
    - runs: Vec<Arc<Run>> - every readable session (unscoped; requests apply their scope)
    - index: HashMap<String, usize> - position of each session by id
    - unreadable: Vec<Value> - sessions whose files cannot be read
    - tasks: Vec<Arc<Value>> - every task record (unscoped)
    - completions: Arc<Vec<Value>> - every task completion event
    - index: Arc<dashboard::TaskIndex> - the task records indexed for the pages
*/
struct Snapshot {
    runs: Vec<Arc<Run>>,
    index: std::collections::HashMap<String, usize>,
    unreadable: Vec<Value>,
    tasks: Vec<Arc<Value>>,
    completions: Arc<Vec<Value>>,
    task_index: Arc<dashboard::TaskIndex>,
}

/** The latest snapshot, once a server has refreshed one */
static SNAPSHOT: OnceLock<RwLock<Option<Arc<Snapshot>>>> = OnceLock::new();

/** Return the latest snapshot, None outside a server (the CLI reads the files directly)
 * Input
    - None
 * Output
    - Option<Arc<Snapshot>>
*/
fn snapshot() -> Option<Arc<Snapshot>> {
    SNAPSHOT
        .get()
        .and_then(|lock| lock.read().ok().and_then(|current| current.clone()))
}

/** Load or revalidate everything the pages read: list the sessions and task records again, check
 * every file now, recompute only what changed, and swap in a new snapshot (the server runs it on
 * every core before it listens, then in the background on a few; nothing is written)
 * Input
    - threads: usize - threads to use
 * Output
    - Result<(usize, usize), String> sessions and task records known
*/
pub(crate) fn refresh(threads: usize) -> Result<(usize, usize), String> {
    let root = crane_root()?;
    let ids = cache::relist(&format!("sessions:{}", root.display()), session_ids)?;
    cache::parallel(&ids, threads, |id| {
        let sources = Run::sources_in(&root, id);
        let _ = cache::revalidate_run(sources[0].clone(), sources, || Run::read(id));
    });
    cache::retain_runs(
        &ids.iter()
            .map(|id| Run::sources_in(&root, id).remove(0))
            .collect::<Vec<_>>(),
    );
    cache::relist(&format!("contracts:{}", root.display()), || {
        contract_ids(&root)
    })?;
    let tasks = cache::relist(&format!("tasks:{}", root.display()), task_ids)?;
    let directory = root.join("runtime").join("tasks");
    let path = |id: &String| directory.join(id).join("state.json");
    cache::parallel(&tasks, threads, |id| {
        let path = path(id);
        let _ = cache::revalidate_value(format!("task:{}", path.display()), vec![path], || {
            read_task(id)
        });
    });
    let keep = tasks
        .iter()
        .map(|id| format!("task:{}", path(id).display()))
        .collect::<std::collections::HashSet<_>>();
    cache::retain_values("task:", &keep);
    // Swap in a snapshot of what was just checked (each entry is reused, not recomputed)
    let mut runs = Vec::new();
    let mut unreadable = Vec::new();
    for id in ids.iter() {
        let sources = Run::sources_in(&root, id);
        match cache::run(sources[0].clone(), sources, || Run::read(id)) {
            Ok(Some(run)) => runs.push(run),
            Ok(None) => {}
            Err(problem) => unreadable.push(json!({"session_id": id, "problem": problem})),
        }
    }
    let records = tasks
        .iter()
        .filter_map(|id| {
            let path = path(id);
            cache::value(format!("task:{}", path.display()), vec![path], || {
                read_task(id)
            })
            .ok()
        })
        .filter(|record| !record.is_null())
        .collect::<Vec<_>>();
    let counts = (runs.len(), records.len());
    let completions = read_completions(true)?;
    let fresh = Snapshot {
        task_index: Arc::new(dashboard::TaskIndex::build(&records)),
        completions,
        index: runs
            .iter()
            .enumerate()
            .map(|(position, run)| (run.id.clone(), position))
            .collect(),
        runs,
        unreadable,
        tasks: records,
    };
    if let Ok(mut current) = SNAPSHOT.get_or_init(|| RwLock::new(None)).write() {
        *current = Some(Arc::new(fresh));
    }
    Ok(counts)
}

/** Return the task records the current scope admits, indexed for the pages: the snapshot's index
 * when the scope admits every record, otherwise one built for the records it admits
 * Input
    - None
 * Output
    - Result<Arc<dashboard::TaskIndex>, String>
*/
pub(crate) fn task_index() -> Result<Arc<dashboard::TaskIndex>, String> {
    let scope = access::current();
    if let Some(snapshot) = snapshot() {
        let (organization, _) = repository_tenant()?;
        if !scope.restricts_teams() && scope.admits_record(organization.as_deref(), None) {
            return Ok(snapshot.task_index.clone());
        }
    }
    Ok(Arc::new(dashboard::TaskIndex::build(&task_records()?)))
}

/** Return every task completion event (oldest first), from the snapshot in a server
 * Input
    - None
 * Output
    - Result<Arc<Vec<Value>>, String>
*/
pub(crate) fn completions() -> Result<Arc<Vec<Value>>, String> {
    match snapshot() {
        Some(snapshot) => Ok(snapshot.completions.clone()),
        None => read_completions(false),
    }
}

/** Read every task completion event, each file only again when it changed
 * Input
    - force: bool - check every file now (the background refresh)
 * Output
    - Result<Arc<Vec<Value>>, String>
*/
fn read_completions(force: bool) -> Result<Arc<Vec<Value>>, String> {
    let root = crane_root()?;
    let directory = root.join("runtime").join("completions");
    let key = format!("completions:{}", root.display());
    let list = || {
        Ok(match std::fs::read_dir(&directory) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .strip_suffix(".json")
                        .map(String::from)
                })
                .collect(),
            Err(_) => Vec::new(),
        })
    };
    let ids = if force {
        cache::relist(&key, list)?
    } else {
        cache::listing(&key, list)?
    };
    let read = |id: &String| {
        let path = directory.join(format!("{id}.json"));
        let compute = || Ok(crate::task_completion::load(id)?.unwrap_or(Value::Null));
        let key = format!("completion:{}", path.display());
        if force {
            cache::revalidate_value(key, vec![path], compute)
        } else {
            cache::value(key, vec![path], compute)
        }
    };
    let mut events = ids
        .iter()
        .filter_map(|id| read(id).ok())
        .filter(|event| !event.is_null())
        .map(|event| (*event).clone())
        .collect::<Vec<_>>();
    events.sort_by_key(|event| (event["created_at"].as_u64(), event["event_id"].to_string()));
    Ok(Arc::new(events))
}

/** List the tasks that have a contract (sorted)
 * Input
    - root: &std::path::Path - the .crane directory
 * Output
    - Result<Vec<String>, String>
*/
fn contract_ids(root: &std::path::Path) -> Result<Vec<String>, String> {
    let mut ids = match std::fs::read_dir(root.join("task-contracts")) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    ids.sort();
    Ok(ids)
}

/** List the orchestration task ids on disk
 * Input
    - None
 * Output
    - Result<Vec<String>, String>
*/
fn task_ids() -> Result<Vec<String>, String> {
    let mut ids = match std::fs::read_dir(crane_root()?.join("runtime").join("tasks")) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|id| identifier(id).is_ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    ids.sort();
    Ok(ids)
}

/** Read one task record (a record removed since the directory was listed counts as no task)
 * Input
    - id: &str - task id
 * Output
    - Result<Value, String>
*/
fn read_task(id: &str) -> Result<Value, String> {
    Ok(crate::orchestration::TaskRecord::load(id)?.map_or(Value::Null, |record| record.value))
}

/** Read every orchestration task record, each one only again when its file changed (unscoped:
 * only task_records may call it)
 * Input
    - None
 * Output
    - Result<Vec<Arc<Value>>, String> the records, shared with the cache
*/
fn every_task_record() -> Result<Vec<Arc<Value>>, String> {
    if let Some(snapshot) = snapshot() {
        return Ok(snapshot.tasks.clone());
    }
    let root = crane_root()?;
    let ids = cache::listing(&format!("tasks:{}", root.display()), task_ids)?;
    let directory = root.join("runtime").join("tasks");
    ids.iter()
        .map(|id| {
            let path = directory.join(id).join("state.json");
            cache::value(format!("task:{}", path.display()), vec![path], || {
                read_task(id)
            })
        })
        // A record that cannot be read is left out rather than failing every page
        .filter_map(Result::ok)
        .filter(|record| !record.is_null())
        .map(Ok)
        .collect()
}

/** Read a task's latest contract as stored, only again when its versions changed
 * Input
    - task: &str - task id
 * Output
    - Option<Arc<Value>>
*/
pub(crate) fn contract(task: &str) -> Option<Arc<Value>> {
    let root = crane_root().ok()?;
    // Most tasks have no contract: the directory's listing answers that without touching a file
    let ids = cache::listing(&format!("contracts:{}", root.display()), || {
        contract_ids(&root)
    })
    .ok()?;
    ids.binary_search_by(|id| id.as_str().cmp(task)).ok()?;
    let directory = root.join("task-contracts").join(task);
    cache::value(
        format!("contract:{}", directory.display()),
        vec![directory],
        || Ok(crate::task_contracts::peek(task)?.unwrap_or(Value::Null)),
    )
    .ok()
    .filter(|value| !value.is_null())
}

/** Answer the overview: headline counts
 * Input
    - None
 * Output
    - Result<Value, String>
*/
fn overview() -> Result<Value, String> {
    let runs = Run::all()?;
    let summaries = runs.iter().map(|run| run.summary()).collect::<Vec<_>>();
    let tasks = tasks(&runs, false)?;
    let sum = |path: &[&str]| {
        summaries
            .iter()
            .map(|summary| {
                path.iter()
                    .fold(summary, |value, key| &value[*key])
                    .as_u64()
                    .unwrap_or(0)
            })
            .sum::<u64>()
    };
    Ok(json!({
        "repositories": repositories()?.len(),
        "agents": agents(&runs).len(),
        "sessions": {
            "total": runs.len(),
            "by_lifecycle": tally(summaries.iter().filter_map(|summary| summary["lifecycle"].as_str().map(String::from))),
        },
        "tasks": {
            "total": tasks.len(),
            "by_state": tally(tasks.iter().filter_map(|task| task["state"].as_str().map(String::from))),
            "pull_requests_created": tasks.iter().filter(|task| task["pull_request_created"] == true).count(),
            "approved": tasks.iter().filter(|task| task["approved"] == true).count(),
            "merged": tasks.iter().filter(|task| task["merged"] == true).count(),
            "completed": tasks.iter().filter(|task| task["completed"] == true).count(),
        },
        "decisions": {
            "allow": sum(&["actions", "allow"]),
            "deny": sum(&["actions", "deny"]),
            "approval_required": sum(&["actions", "approval_required"]),
        },
        "violations": {
            "total": sum(&["violations", "total"]),
            "critical": sum(&["violations", "critical"]),
            "high": sum(&["violations", "high"]),
            "medium": sum(&["violations", "medium"]),
            "low": sum(&["violations", "low"]),
        },
        "budget_consumed": sum(&["budget", "consumed"]),
        "verification": {
            "contract_tests_passed": summaries.iter().filter(|summary| summary["verification"]["contract_tests_passed"] == true).count(),
            "contract_tests_failed": summaries.iter().filter(|summary| summary["verification"]["contract_tests_passed"] == false).count(),
            "repository_tests_passed": summaries.iter().filter(|summary| summary["verification"]["repository_tests_passed"] == true).count(),
            "repository_tests_failed": summaries.iter().filter(|summary| summary["verification"]["repository_tests_passed"] == false).count(),
        },
    }))
}

/** Read one session in full or answer 404 (also for a session the scope does not admit)
 * Input
    - id: &str - session id (validated)
 * Output
    - Result<Full, (u16, String)>
*/
fn one(id: &str) -> Result<Full, (u16, String)> {
    Run::full(identifier(id).map_err(|error| (400, error))?)
        .map_err(|error| (500, error))?
        .ok_or_else(|| (404, format!("no session '{id}'")))
}

/** Answer one GET request
 * Input
    - path: &str - path without the query
    - query: &str - query string
 * Output
    - Result<Value, (u16, String)>
*/
fn get(path: &str, query: &str) -> Result<Value, (u16, String)> {
    let segments = path
        .trim_start_matches("/api/v1")
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let pattern = match segments.as_slice() {
        [] | ["endpoints"] => "/api/v1/endpoints",
        ["whoami"] => "/api/v1/whoami",
        ["overview"] => "/api/v1/overview",
        ["repositories"] => "/api/v1/repositories",
        ["agents"] => "/api/v1/agents",
        ["tasks"] => "/api/v1/tasks",
        ["tasks", _] => "/api/v1/tasks/{task}",
        ["sessions"] => "/api/v1/sessions",
        ["sessions", _] => "/api/v1/sessions/{session}",
        ["sessions", _, "actions"] => "/api/v1/sessions/{session}/actions",
        ["sessions", _, "autonomy"] => "/api/v1/sessions/{session}/autonomy",
        ["decisions"] => "/api/v1/decisions",
        ["violations"] => "/api/v1/violations",
        ["audit"] => "/api/v1/audit",
        ["dashboard", "overview"] => "/api/v1/dashboard/overview",
        ["dashboard", "tasks"] => "/api/v1/dashboard/tasks",
        ["dashboard", "agents"] => "/api/v1/dashboard/agents",
        ["dashboard", "runs"] => "/api/v1/dashboard/runs",
        ["dashboard", "runs", _] => "/api/v1/dashboard/runs/{run}",
        ["dashboard", "runs", _, "events"] => "/api/v1/dashboard/runs/{run}/events",
        ["dashboard", "policies"] => "/api/v1/dashboard/policies",
        ["dashboard", "policies", _] => "/api/v1/dashboard/policies/{policy}",
        ["dashboard", "zones"] => "/api/v1/dashboard/zones",
        ["dashboard", "zones", _] => "/api/v1/dashboard/zones/{zone}",
        ["dashboard", "violations"] => "/api/v1/dashboard/violations",
        ["dashboard", "autonomy"] => "/api/v1/dashboard/autonomy",
        ["dashboard", "repositories"] => "/api/v1/dashboard/repositories",
        ["dashboard", "repositories", _] => "/api/v1/dashboard/repositories/{repository}",
        ["dashboard", "agents", _] => "/api/v1/dashboard/agents/{agent}",
        ["dashboard", "tasks", _] => "/api/v1/dashboard/tasks/{task}",
        _ => {
            return Err((
                404,
                format!("no observability endpoint {path}; see /api/v1/endpoints"),
            ))
        }
    };
    let allowed = ENDPOINTS
        .iter()
        .find(|(endpoint, _, _)| *endpoint == pattern)
        .map_or(&[][..], |(_, parameters, _)| *parameters);
    let query = Query::parse(query, allowed).map_err(|error| (400, error))?;
    let limit = query.limit().map_err(|error| (400, error))?;
    let offset = query.offset().map_err(|error| (400, error))?;
    // Every identifier in the path is checked before anything is read
    for (segment, part) in segments
        .iter()
        .zip(pattern.trim_start_matches("/api/v1/").split('/'))
    {
        if part.starts_with('{') {
            identifier(segment).map_err(|error| (400, error))?;
        }
    }
    let failed = |error: String| (500, error);
    // Every endpoint that answers with records needs the scope to admit this repository
    let scope = access::current();
    let (organization, repository) = repository_tenant().map_err(failed)?;
    let admitted = scope.admits_repository(organization.as_deref(), &repository);
    if !admitted && !matches!(segments.as_slice(), [] | ["endpoints"] | ["whoami"]) {
        return Err((
            403,
            format!(
                "viewer '{}' may not read repository '{repository}'",
                scope.viewer
            ),
        ));
    }
    let capped = |items: Vec<Value>| {
        let total = items.len();
        json!({"total": total, "offset": offset, "limit": limit, "items": items.into_iter().skip(offset).take(limit).collect::<Vec<_>>()})
    };
    let value = match segments.as_slice() {
        [] | ["endpoints"] => {
            json!({"endpoints": ENDPOINTS.iter().map(|(path, parameters, answers)| json!({"path": path, "method": "GET", "parameters": parameters, "answers": answers})).collect::<Vec<_>>()})
        }
        ["overview"] => overview().map_err(failed)?,
        ["repositories"] => json!({"repositories": repositories().map_err(failed)?}),
        ["agents"] => json!({"agents": agents(&Run::all().map_err(failed)?)}),
        ["tasks"] => {
            let runs = Run::all().map_err(failed)?;
            let list = tasks(&runs, false)
                .map_err(failed)?
                .into_iter()
                .filter(|task| query.admits("state", &task["state"]))
                .collect();
            capped(list)
        }
        ["tasks", id] => {
            let id = identifier(id).map_err(|error| (400, error))?;
            let runs = Run::all().map_err(failed)?;
            tasks(&runs, true)
                .map_err(failed)?
                .into_iter()
                .find(|task| task["task_id"] == id)
                .ok_or_else(|| (404, format!("no task '{id}'")))?
        }
        ["sessions"] => {
            let list = Run::all()
                .map_err(failed)?
                .iter()
                .map(|run| run.summary())
                .filter(|summary| {
                    query.admits("agent", &summary["agent"])
                        && query.admits("task", &summary["task_id"])
                        && query.admits("lifecycle", &summary["lifecycle"])
                })
                .collect();
            capped(list)
        }
        ["sessions", id] => {
            let run = one(id)?;
            let mut value = run.summary();
            value["event_count"] = json!(run.events.len());
            value["governance"] = json!({
                "autonomy": run.document["governance"]["autonomy"],
                "zone_set_version": run.document["governance"]["zone_set_version"],
                "zones": run.document["governance"]["zones"],
                "scope_modules": run.document["governance"]["scope_modules"],
                "task_contract": run.document["governance"]["task_contract"],
            });
            value["contracts"] = run.attestation["contract"].clone();
            value["violation_list"] = json!(run.violations());
            value["attestation"] = run.attestation.clone();
            value
        }
        ["sessions", id, "actions"] => {
            let run = one(id)?;
            capped(
                run.actions()
                    .into_iter()
                    .filter(|action| query.admits("decision", &action["decision"]))
                    .collect(),
            )
        }
        ["sessions", id, "autonomy"] => one(id)?.timeline(),
        ["decisions"] => {
            // Filter the compact projection; only the page asked for is rendered
            let runs = Run::all().map_err(failed)?;
            let has = |key: &str, items: &[&str]| {
                query
                    .set(key)
                    .is_none_or(|wanted| items.iter().any(|item| wanted.contains(*item)))
            };
            let mut total = 0;
            let mut items = Vec::new();
            for run in runs.iter().filter(|run| {
                query.admits("session", &json!(run.id))
                    && query.admits("task", &run.task())
                    && query.admits("agent", &json!(run.agent()))
            }) {
                for act in run.acts.iter().filter(|act| {
                    act.category == Some("authorization")
                        && query.admits("decision", &json!(act.decision))
                        && has("policy", act.policies)
                        && has("zone", act.zones)
                }) {
                    if total >= offset && items.len() < limit {
                        items.push(run.action(act));
                    }
                    total += 1;
                }
            }
            json!({"total": total, "offset": offset, "limit": limit, "items": items})
        }
        ["violations"] => {
            let list = Run::all()
                .map_err(failed)?
                .iter()
                .flat_map(|run| run.violations())
                .filter(|violation| {
                    query.admits("severity", &violation["severity"])
                        && query.admits("session", &violation["session_id"])
                        && query.admits("task", &violation["task_id"])
                })
                .collect();
            capped(list)
        }
        ["dashboard", "overview"] => dashboard::overview(&query)?,
        ["dashboard", "tasks"] => dashboard::tasks(&query)?,
        ["dashboard", "agents"] => agents::list(&query)?,
        ["dashboard", "runs"] => runs::list(&query)?,
        ["dashboard", "runs", id] => runs::detail(identifier(id).map_err(|error| (400, error))?)?,
        ["dashboard", "runs", id, "events"] => {
            runs::events(identifier(id).map_err(|error| (400, error))?, &query)?
        }
        ["dashboard", "policies"] => catalog::policies(&query)?,
        ["dashboard", "policies", id] => {
            catalog::policy(identifier(id).map_err(|error| (400, error))?, &query)?
        }
        ["dashboard", "zones"] => catalog::zones(&query)?,
        ["dashboard", "zones", id] => {
            catalog::zone(identifier(id).map_err(|error| (400, error))?, &query)?
        }
        ["dashboard", "violations"] => catalog::violations(&query)?,
        ["dashboard", "autonomy"] => autonomy::overview(&query)?,
        ["dashboard", "repositories"] => repos::list(&query)?,
        ["dashboard", "repositories", id] => {
            repos::detail(identifier(id).map_err(|error| (400, error))?, &query)?
        }
        ["dashboard", "agents", id] => {
            agents::detail(identifier(id).map_err(|error| (400, error))?, &query)?
        }
        ["dashboard", "tasks", id] => {
            detail::task(identifier(id).map_err(|error| (400, error))?, &query)?
        }
        ["audit"] => {
            let (runs, unreadable) = Run::scan().map_err(failed)?;
            // An unreadable session's tenant is unknown, so only the operator is told of it
            let unreadable = if scope.operator {
                unreadable
            } else {
                Vec::new()
            };
            json!({
                "sessions": runs.iter().map(|run| json!({"session_id": run.id, "chain": run.chain, "delivery_chain": run.delivery["chain"]})).collect::<Vec<_>>(),
                "unreadable_sessions": unreadable,
                "zone_review": crate::zones::review::audit_log().map_err(failed)?["chain"],
            })
        }
        ["whoami"] => {
            let mut value = scope.to_json();
            value["repository"] =
                json!({"name": repository, "organization": organization, "readable": admitted});
            value
        }
        _ => unreachable!("every pattern above is answered"),
    };
    Ok(answer(value))
}

/** Route one request as the operator (the local CLI, which reads the same files directly)
 * Input
    - method: &str - HTTP method
    - target: &str - path with an optional query string
 * Output
    - (u16, Value) status and JSON answer
*/
pub(crate) fn route(method: &str, target: &str) -> (u16, Value) {
    route_as(access::Scope::operator(), method, target)
}

/** Route one request under a scope: GET on the fixed endpoints only (every other method is
 * refused before anything is read), and only the records the scope admits
 * Input
    - scope: access::Scope - what the caller may read
    - method: &str - HTTP method
    - target: &str - path with an optional query string
 * Output
    - (u16, Value) status and JSON answer
*/
pub(crate) fn route_as(scope: access::Scope, method: &str, target: &str) -> (u16, Value) {
    access::within(scope, || answer_request(method, target))
}

/** Answer one request under the scope already set
 * Input
    - method: &str - HTTP method
    - target: &str - path with an optional query string
 * Output
    - (u16, Value) status and JSON answer
*/
fn answer_request(method: &str, target: &str) -> (u16, Value) {
    if method != "GET" {
        return (
            405,
            json!({"error": "the observability API is read-only; changes are made with the Crane CLI and its control-plane API", "allowed": ["GET"]}),
        );
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = if path.starts_with("/api/") {
        path.to_string()
    } else {
        format!("/api/v1/{}", path.trim_start_matches('/'))
    };
    if !path.starts_with("/api/v1") {
        return (
            404,
            json!({"error": format!("no observability endpoint {path}; see /api/v1/endpoints")}),
        );
    }
    match get(&path, query) {
        Ok(value) => (200, value),
        Err((status, error)) => (status, json!({"error": error})),
    }
}
