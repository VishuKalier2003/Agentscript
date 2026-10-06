// Task intake: the path a user takes from a connected repository's tracker tasks to an agent
// session ready to run. Tasks come from the task source adapters (Jira, Asana) and their
// repository mapping; planning compiles the task into a task contract (crate::task_contracts),
// which validates its quality; a human approves the contract; the user picks the agent, and the
// launch creates the contract session with an immutable binding (task, contract digest,
// repository, checkpoint, agent, autonomy, budget, zones, policy version). The state shown for a
// task is derived from those facts every time it is read, so it cannot drift from them; the
// runtime authority engine only ever sees a task id and a contract, never a tracker.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::adapter::AgentKind;
use crate::agent_session::{self, SessionOptions};
use crate::orchestration::adapters::{adapter, SourceEvent};
use crate::orchestration::{SourceConfig, TaskRecord};
use crate::proposals::store::agent_environment;
use crate::repository::root;
use crate::session::{session_ids, ContractSession, Lifecycle};
use crate::task_contracts;
use crate::tasks::{valid_task_id, TaskInput};
use crate::util::{io_error, now_unix, sha256};
use crate::zones::model::Autonomy;

/** Version of the intake record layout */
const RECORD_FORMAT: u64 = 1;

/** Task sources the intake reads */
const SOURCES: &[&str] = &["jira", "asana"];

/** Where a task stands on the way from the tracker to a running agent session
 * Variants
    - Available - mapped to this repository, not planned yet
    - Planning - its contract is being compiled
    - NeedsClarification - the task is too vague to bound
    - ContractPendingApproval - its contract waits for a human
    - Ready - the contract is approved; ready to launch, or launched and waiting for the agent
    - Running - the agent is working in the bound session
    - Verified - the session was verified and has nothing to deliver
    - DeliveryPending - the verified change waits for delivery
    - PrReview - the pull request is under review
    - Merged - the change merged; its tracker completion is not queued yet
    - TaskCompletionPending - the tracker completion is queued or being sent
    - CompletionRetryPending - the tracker could not be reached; the completion is retried
    - Completed - the tracker issue is completed (after the merge, or it was done in the tracker)
    - Blocked - a human has to act (conflict, rejection, invalidation, obsolete contract)
    - Failed - fetching or planning failed
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntakeState {
    Available,
    Planning,
    NeedsClarification,
    ContractPendingApproval,
    Ready,
    Running,
    Verified,
    DeliveryPending,
    PrReview,
    Merged,
    TaskCompletionPending,
    CompletionRetryPending,
    Completed,
    Blocked,
    Failed,
}

impl IntakeState {
    /** Return the state's name
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Available => "AVAILABLE",
            Self::Planning => "PLANNING",
            Self::NeedsClarification => "NEEDS_CLARIFICATION",
            Self::ContractPendingApproval => "CONTRACT_PENDING_APPROVAL",
            Self::Ready => "READY",
            Self::Running => "RUNNING",
            Self::Verified => "VERIFIED",
            Self::DeliveryPending => "DELIVERY_PENDING",
            Self::PrReview => "PR_REVIEW",
            Self::Merged => "MERGED",
            Self::TaskCompletionPending => "TASK_COMPLETION_PENDING",
            Self::CompletionRetryPending => "COMPLETION_RETRY_PENDING",
            Self::Completed => "COMPLETED",
            Self::Blocked => "BLOCKED",
            Self::Failed => "FAILED",
        }
    }
}

/** Refuse an intake action on behalf of an agent: choosing, planning, approving, and launching
 * are a human's
 * Input
    - action: &str - prepare, approve, or launch
 * Output
    - Result<(), String>
*/
fn require_human(action: &str) -> Result<(), String> {
    match agent_environment() {
        Some(marker) => Err(format!(
            "crane task {action} refuses to run in an agent environment ({marker} is set); a human chooses, plans, approves, and launches tasks"
        )),
        None => Ok(()),
    }
}

/** A task offered by a source, with its repository mapping
 * Fields
    - source: &'static str - source name
    - event: SourceEvent - the listed task (its payload is the task context)
    - project: Option<String> - tracker project
    - mapping: Option<Value> - {repositories, team} from .crane/sources/config.json
    - here: bool - the mapping names this repository
*/
struct Candidate {
    source: &'static str,
    event: SourceEvent,
    project: Option<String>,
    mapping: Option<Value>,
    here: bool,
}

/** Return the names the connected repository answers to (name and owner/name, lowercase);
 * refused when no repository is connected
 * Input
    - None
 * Output
    - Result<BTreeSet<String>, String>
*/
fn repository_names() -> Result<BTreeSet<String>, String> {
    crate::repo::connected()
        .ok_or("connect the repository first: crane repo connect (or POST /api/connection)")?;
    let mut names = BTreeSet::new();
    if let (owner, Some(name)) = crate::repo::owner_and_name() {
        names.insert(name.to_ascii_lowercase());
        if let Some(owner) = owner {
            names.insert(format!("{owner}/{name}").to_ascii_lowercase());
        }
    }
    Ok(names)
}

/** List every task the sources offer, with whether its project maps to this repository
 * Input
    - config: &SourceConfig - source settings and repository mapping
    - names: &BTreeSet<String> - names of this repository
 * Output
    - Result<Vec<Candidate>, String>
*/
fn candidates(config: &SourceConfig, names: &BTreeSet<String>) -> Result<Vec<Candidate>, String> {
    let snapshots = root()?.join("sources");
    let mut found = Vec::new();
    for source in SOURCES {
        let adapter = adapter(source)?;
        for event in adapter.list(&snapshots)? {
            if !valid_task_id(&event.task_id) {
                continue;
            }
            let project = adapter.project(&event.payload);
            let mapping = project
                .as_deref()
                .and_then(|project| config.mapping(source, project))
                .cloned();
            let here = mapping.as_ref().is_some_and(|mapping| {
                mapping["repositories"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .any(|repository| {
                        let repository = repository.trim_end_matches(".git").to_ascii_lowercase();
                        names.contains(&repository)
                            || names.contains(repository.rsplit('/').next().unwrap_or_default())
                    })
            });
            found.push(Candidate {
                source,
                event,
                project,
                mapping,
                here,
            });
        }
    }
    Ok(found)
}

/** Find a task of this repository by id
 * Input
    - task: &str - task id
 * Output
    - Result<(Candidate, SourceConfig), String>
    - Error when no source offers it, or it is not mapped to this repository
*/
fn find(task: &str) -> Result<(Candidate, SourceConfig), String> {
    let config = SourceConfig::load()?;
    let names = repository_names()?;
    let candidate = candidates(&config, &names)?
        .into_iter()
        .find(|candidate| candidate.event.task_id == task)
        .ok_or_else(|| {
            format!("no task source offers task '{task}'; list them with 'crane task list'")
        })?;
    if !candidate.here {
        return Err(format!(
            "task {task} ({} project {}) is not mapped to this repository in .crane/sources/config.json",
            candidate.source,
            candidate.project.as_deref().unwrap_or("unknown")
        ));
    }
    Ok((candidate, config))
}

/** Return the intake records directory, .crane/runtime/intake
 * Input
    - None
 * Output
    - Result<PathBuf, String>
*/
fn directory() -> Result<PathBuf, String> {
    let runtime = root()?.join("runtime");
    fs::create_dir_all(&runtime).map_err(io_error)?;
    let ignore = runtime.join(".gitignore");
    if !ignore.exists() {
        fs::write(ignore, "*\n").map_err(io_error)?;
    }
    Ok(runtime.join("intake"))
}

/** Load a task's intake record, or a new one
 * Input
    - task: &str - task id
 * Output
    - Result<Value, String>
*/
fn record(task: &str) -> Result<Value, String> {
    match fs::read_to_string(directory()?.join(format!("{task}.json"))) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| format!("intake record {task} is invalid: {error}")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(json!({
            "intake_format": RECORD_FORMAT,
            "task_id": task,
            "state": IntakeState::Available.name(),
            "planning": null,
            "failure": null,
            "launch": null,
            "launches": [],
            "history": [],
        })),
        Err(error) => Err(io_error(error)),
    }
}

/** Write a task's intake record through a temporary file
 * Input
    - record: &Value - record
 * Output
    - Result<(), String>
*/
fn save(record: &Value) -> Result<(), String> {
    let folder = directory()?;
    fs::create_dir_all(&folder).map_err(io_error)?;
    let path = folder.join(format!(
        "{}.json",
        record["task_id"].as_str().unwrap_or_default()
    ));
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_string_pretty(record).map_err(io_error)? + "\n",
    )
    .map_err(io_error)?;
    fs::rename(&temporary, &path).map_err(io_error)
}

/** Record a state change in the intake history (when the state differs from the last one)
 * Input
    - record: &mut Value - record
    - state: IntakeState - new state
    - reason: &str - why
 * Output
    - bool, whether the state changed
*/
fn note(record: &mut Value, state: IntakeState, reason: &str) -> bool {
    if record["state"] == state.name() {
        return false;
    }
    let entry =
        json!({"from": record["state"], "to": state.name(), "at": now_unix(), "reason": reason});
    if let Some(history) = record["history"].as_array_mut() {
        history.push(entry);
    }
    record["state"] = json!(state.name());
    true
}

/** Normalize a candidate's task with its adapter and repository mapping, as stored in
 * .crane/tasks/ID.json
 * Input
    - candidate: &Candidate - task
    - config: &SourceConfig - settings
    - context: &Value - fetched context
 * Output
    - Result<TaskInput, String>
*/
fn normalize(
    candidate: &Candidate,
    config: &SourceConfig,
    context: &Value,
) -> Result<TaskInput, String> {
    let adapter = adapter(candidate.source)?;
    let mut input = adapter.normalize(
        &candidate.event,
        context,
        config.value[candidate.source]["acceptance_field"].as_str(),
    )?;
    if let Some(mapping) = &candidate.mapping {
        input.repositories = mapping["repositories"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|repository| repository.as_str().map(String::from))
            .collect();
        if let Some(team) = mapping["team"].as_str() {
            input.team = Some(team.into());
        }
    }
    Ok(input)
}

/** Fetch a candidate's full context through its adapter
 * Input
    - candidate: &Candidate - task
 * Output
    - Result<Value, String>
*/
fn fetch(candidate: &Candidate) -> Result<Value, String> {
    adapter(candidate.source)?.fetch(&candidate.event, &root()?.join("sources"))
}

/** Build the immutable binding of a launched session: everything the session was started with
 * that later decisions rely on, from the session's own (digest-checked) binding and the approved
 * contract
 * Input
    - session: &ContractSession - session
    - contract: &Value - the approved contract version
 * Output
    - Value
*/
fn launch_binding(session: &ContractSession, contract: &Value) -> Value {
    let described = session.describe();
    let governance = &described["governance"];
    let bindings = &contract["bindings"];
    json!({
        "task_id": described["task_id"],
        "contract": {
            "contract_id": contract["contract_id"],
            "version": contract["version"],
            "digest": contract["digest"],
        },
        "session": {
            "session_id": described["session_id"],
            "binding_digest": described["binding_digest"],
            "provider_session": described["provider_session"],
        },
        "agent": described["agent"],
        "repository": {
            "identity": described["repository_id"],
            "owner": bindings["repository"]["owner"],
            "name": bindings["repository"]["name"],
        },
        "checkpoint": bindings["checkpoint"],
        "autonomy": {
            "mode": governance["autonomy"],
            "safety": "active",
            "max_autonomy": bindings["autonomy"]["max_autonomy"],
            "policy_version": bindings["autonomy"]["policy_version"],
        },
        "budget": {
            "mutating_actions": governance["budget"]["mutating_actions"],
            "files": governance["budget"]["files"],
            "model_version": bindings["budget"]["model_version"],
            "risk_budget_max": bindings["budget"]["max"],
        },
        "zones": {
            "zone_set_version": bindings["zone_set_version"],
            "zones": governance["zones"],
        },
        "policy_version": bindings["policy_version"],
        "contract_set_version": described["contracts"]["version"],
        "task_contract": governance["task_contract"],
    })
}

/** Check that a launch still matches what it recorded: the session loads (its own binding digest
 * is verified on load), and the binding rebuilt from it and the contract, the recorded binding,
 * and the recorded digest all agree
 * Input
    - launch: &Value - the recorded launch
 * Output
    - Result<Option<ContractSession>, String> the session, or why the binding no longer holds
*/
fn verify(launch: &Value) -> Result<Option<ContractSession>, String> {
    let id = launch["session_id"].as_str().unwrap_or_default();
    let session = ContractSession::load(id)
        .map_err(|error| format!("session {id} cannot be trusted: {error}"))?
        .ok_or_else(|| format!("session {id} no longer exists"))?;
    let task = launch["binding"]["task_id"].as_str().unwrap_or_default();
    let version = launch["binding"]["contract"]["version"]
        .as_u64()
        .unwrap_or(0);
    let contract = task_contracts::stored(task, version)?;
    let digest = sha256(launch_binding(&session, &contract).to_string().as_bytes());
    let recorded = sha256(launch["binding"].to_string().as_bytes());
    if launch["binding_digest"] != digest.as_str() || recorded != digest {
        return Err(format!(
            "the binding of session {id} no longer matches what was launched"
        ));
    }
    Ok(Some(session))
}

/** Derive a task's state from the facts: the tracker, the intake record, the current contract,
 * and the launched session
 * Input
    - candidate: &Candidate - task
    - record: &Value - intake record
 * Output
    - Result<(IntakeState, String, Value), String> state, reason, and details (contract and
      session summaries)
*/
fn derive(candidate: &Candidate, record: &Value) -> Result<(IntakeState, String, Value), String> {
    let task = candidate.event.task_id.as_str();
    let mut details = json!({"contract": null, "session": null, "ready_to_run": false});
    if adapter(candidate.source)?.done(&candidate.event.payload) {
        let (state, reason) = match crate::task_completion::for_task(task)? {
            Some(event) if crate::task_completion::lifecycle_state(&event) != "COMPLETED" => {
                let (state, reason) = completion_state(&event);
                (
                    state,
                    format!("{reason} (the issue is done in {})", candidate.source),
                )
            }
            _ => (
                IntakeState::Completed,
                format!("done in {}", candidate.source),
            ),
        };
        return Ok((state, reason, details));
    }
    if let Some(orchestrated) = TaskRecord::load(task)? {
        match orchestrated.value["state"].as_str() {
            Some("COMPLETED") => {
                return Ok((
                    IntakeState::Completed,
                    "completed by the task lifecycle".into(),
                    details,
                ))
            }
            Some("CANCELLED") => {
                return Ok((
                    IntakeState::Blocked,
                    "cancelled in the tracker".into(),
                    details,
                ))
            }
            _ => {}
        }
    }
    if record["planning"].is_object() {
        return Ok((
            IntakeState::Planning,
            "the contract is being compiled".into(),
            details,
        ));
    }
    let contract = task_contracts::load(task, None)?;
    let failed_after = |compiled: u64| {
        record["failure"]["at"]
            .as_u64()
            .filter(|at| *at >= compiled)
            .map(|_| {
                record["failure"]["error"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
    };
    let Some(contract) = contract else {
        return Ok(match failed_after(0) {
            Some(error) => (IntakeState::Failed, error, details),
            None => (IntakeState::Available, "not planned yet".into(), details),
        });
    };
    details["contract"] = json!({
        "contract_id": contract["contract_id"],
        "version": contract["version"],
        "status": contract["status"],
        "digest": contract["digest"],
        "checkpoint": contract["bindings"]["checkpoint"],
        "policy_version": contract["bindings"]["policy_version"],
        "zone_set_version": contract["bindings"]["zone_set_version"],
    });
    if let Some(error) = failed_after(contract["compiled"]["at"].as_u64().unwrap_or(0)) {
        return Ok((IntakeState::Failed, error, details));
    }
    let status = contract["status"].as_str().unwrap_or_default();
    let blocking = || {
        contract["clarifications"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["blocking"] != false)
            .chain(contract["conflicts"].as_array().into_iter().flatten())
            .filter_map(|item| item["message"].as_str())
            .collect::<Vec<_>>()
            .join("; ")
    };
    let state = match status {
        "clarification_required" => (IntakeState::NeedsClarification, blocking()),
        "proposed" => (
            IntakeState::ContractPendingApproval,
            format!(
                "contract {} waits for human approval",
                contract["contract_id"].as_str().unwrap_or_default()
            ),
        ),
        "retired" => match crate::task_completion::for_task(task)? {
            Some(event) => completion_state(&event),
            None => (IntakeState::Blocked, "the task contract was retired".into()),
        },
        "invalidated" => (
            IntakeState::Blocked,
            contract["invalidation"]["message"]
                .as_str()
                .unwrap_or("invalidated")
                .to_string(),
        ),
        "approved" => {
            let launch = &record["launch"];
            if launch["binding"]["contract"]["digest"] != contract["digest"] {
                (
                    IntakeState::Ready,
                    "contract approved; choose Claude or Codex to launch the session".into(),
                )
            } else {
                let session = verify(launch);
                details["session"] = json!({
                    "session_id": launch["session_id"],
                    "agent": launch["agent"],
                    "binding_digest": launch["binding_digest"],
                });
                match session {
                    Err(error) => (IntakeState::Blocked, error),
                    Ok(None) => (IntakeState::Ready, "launch again".into()),
                    Ok(Some(session)) => {
                        let lifecycle = session.lifecycle();
                        let activity = session.activity();
                        details["session"]["lifecycle"] = json!(lifecycle.name());
                        details["session"]["safety"] = json!(activity.state.safety.name());
                        details["session"]["autonomy"] = json!(activity.state.autonomy.name());
                        details["session"]["actions"] = json!(activity.actions);
                        match lifecycle {
                            Lifecycle::Finalized => after_session(task, &session)?,
                            Lifecycle::Cancelled => (
                                IntakeState::Ready,
                                "the session was cancelled; launch it again".into(),
                            ),
                            _ if activity.started_at.is_some() || activity.actions > 0 => (
                                IntakeState::Running,
                                format!(
                                    "the agent works in session {}",
                                    launch["session_id"].as_str().unwrap_or_default()
                                ),
                            ),
                            _ => {
                                details["ready_to_run"] = json!(true);
                                (
                                    IntakeState::Ready,
                                    format!(
                                        "READY TO RUN: session {} is bound and waits for the agent",
                                        launch["session_id"].as_str().unwrap_or_default()
                                    ),
                                )
                            }
                        }
                    }
                }
            }
        }
        other => (
            IntakeState::Blocked,
            format!("contract is {other}: {}", blocking()),
        ),
    };
    Ok((state.0, state.1, details))
}

/** Name the intake state of a completion event
 * Input
    - event: &Value - completion event
 * Output
    - (IntakeState, String)
*/
fn completion_state(event: &Value) -> (IntakeState, String) {
    let id = event["event_id"].as_str().unwrap_or_default();
    match crate::task_completion::lifecycle_state(event) {
        "COMPLETED" => (
            IntakeState::Completed,
            format!(
                "merged as {} and completed in the tracker ({id}{})",
                event["merge_sha"].as_str().unwrap_or_default(),
                if event["status"] == "completed_externally" {
                    ", completed externally"
                } else {
                    ""
                }
            ),
        ),
        "COMPLETION_RETRY_PENDING" => (
            IntakeState::CompletionRetryPending,
            format!(
                "merged; the tracker completion {id} is retried: {}",
                event["last_error"].as_str().unwrap_or("unavailable")
            ),
        ),
        _ => (
            IntakeState::TaskCompletionPending,
            format!("merged; the tracker completion {id} is queued"),
        ),
    }
}

/** Derive the state of a task whose session ended: the orchestrator's verdict, then the delivery
 * (pull request, merge), then the tracker completion; a task is COMPLETED only through a confirmed
 * completion after a verified merge
 * Input
    - task: &str - task id
    - session: &ContractSession - the launched session
 * Output
    - Result<(IntakeState, String), String>
*/
fn after_session(task: &str, session: &ContractSession) -> Result<(IntakeState, String), String> {
    let id = session.id();
    if let Some(event) = crate::task_completion::for_task(task)? {
        return Ok(completion_state(&event));
    }
    let delivery = crate::delivery::journal(id)?;
    if !delivery.is_empty() {
        let state = crate::delivery::state(&delivery);
        if let Some(sha) = state["merged"]["sha"].as_str() {
            return Ok((IntakeState::Merged, format!("merged as {sha}; the tracker completion is not queued yet (crane task completions reconcile)")));
        }
        if !state["head"].is_null() {
            return Ok((
                IntakeState::PrReview,
                format!(
                    "pull request {} is under review",
                    state["pull_request"]["url"].as_str().unwrap_or_default()
                ),
            ));
        }
    }
    Ok(
        match crate::session_orchestrator::load(id)?
            .map(|record| crate::session_orchestrator::phase(&record))
        {
            Some(crate::session_orchestrator::Phase::DeliveryReady) => (
                IntakeState::DeliveryPending,
                format!("verified; deliver it with 'crane deliver run {id}'"),
            ),
            Some(crate::session_orchestrator::Phase::Verified) => (
                IntakeState::Verified,
                "verified, with no changes to deliver".into(),
            ),
            Some(crate::session_orchestrator::Phase::Failed) => (
                IntakeState::Failed,
                format!("session {id} failed its verification"),
            ),
            Some(_) => (
                IntakeState::Running,
                format!("session {id} is being terminated"),
            ),
            None => (
                IntakeState::DeliveryPending,
                format!("the session was finalized; deliver it with 'crane deliver run {id}'"),
            ),
        },
    )
}

/** Find the tracker issue of a task offered by a task source
 * Input
    - task: &str - task id
 * Output
    - Option<Value> {source, external_id}
*/
pub(crate) fn source_of(task: &str) -> Option<Value> {
    let config = SourceConfig::load().ok()?;
    let snapshots = root().ok()?.join("sources");
    for source in SOURCES {
        let adapter = adapter(source).ok()?;
        if let Some(event) = adapter
            .list(&snapshots)
            .ok()?
            .into_iter()
            .find(|event| event.task_id == task)
        {
            let _ = &config;
            return Some(json!({"source": source, "external_id": event.external_id}));
        }
    }
    None
}

/** Describe a task for listing: tracker fields, mapping, state, contract, agent, checkpoint, and
 * session; a changed state is noted in the intake record (never on behalf of an agent)
 * Input
    - candidate: &Candidate - task
 * Output
    - Result<Value, String>
*/
fn view(candidate: &Candidate) -> Result<Value, String> {
    let task = candidate.event.task_id.as_str();
    let mut record = record(task)?;
    let (state, reason, details) = derive(candidate, &record)?;
    if note(&mut record, state, &reason) && agent_environment().is_none() {
        save(&record)?;
    }
    Ok(json!({
        "task_id": task,
        "source": candidate.source,
        "external_id": candidate.event.external_id,
        "title": candidate.event.detail.trim_start_matches("listed: "),
        "project": candidate.project,
        "team": candidate.mapping.as_ref().map(|mapping| mapping["team"].clone()),
        "state": state.name(),
        "reason": reason,
        "ready_to_run": details["ready_to_run"],
        "contract": details["contract"],
        "agent": record["launch"]["agent"],
        "checkpoint": details["contract"]["checkpoint"],
        "session": details["session"],
    }))
}

/** List the tasks of the connected repository: every task its sources offer whose project maps
 * to it, with its state
 * Input
    - None
 * Output
    - Result<Value, String> {repository, tasks, unmapped}
*/
pub(crate) fn list() -> Result<Value, String> {
    let config = SourceConfig::load()?;
    let names = repository_names()?;
    let found = candidates(&config, &names)?;
    let mut tasks = Vec::new();
    let mut unmapped = Vec::new();
    for candidate in &found {
        if candidate.here {
            tasks.push(view(candidate)?);
        } else {
            unmapped.push(json!({"task_id": candidate.event.task_id, "source": candidate.source, "project": candidate.project}));
        }
    }
    Ok(json!({"repository": names, "tasks": tasks, "unmapped": unmapped}))
}

/** Inspect one task: its listing, the task as normalized from the tracker, the contract, the
 * intake history, and the launch binding with its verification
 * Input
    - task: &str - task id
 * Output
    - Result<Value, String>
*/
pub(crate) fn show(task: &str) -> Result<Value, String> {
    let (candidate, config) = find(task)?;
    let mut value = view(&candidate)?;
    let context = fetch(&candidate);
    value["normalized"] = match &context {
        Ok(context) => normalize(&candidate, &config, context)?.to_json(),
        Err(error) => json!({"error": error}),
    };
    value["assignees"] = json!(context
        .as_ref()
        .map(|context| adapter(candidate.source)
            .map(|adapter| adapter.assignees(context))
            .unwrap_or_default())
        .unwrap_or_default());
    value["task_contract"] = task_contracts::load(task, None)?.unwrap_or(Value::Null);
    let record = record(task)?;
    value["launch"] = record["launch"].clone();
    value["launch_verified"] = match record["launch"].is_object() {
        true => json!(verify(&record["launch"])
            .map(|_| "intact".to_string())
            .unwrap_or_else(|error| error)),
        false => Value::Null,
    };
    value["history"] = record["history"].clone();
    Ok(value)
}

/** Plan a task: fetch its context, normalize and map it, store it as .crane/tasks/ID.json, and
 * compile its contract, which validates the task's quality (a vague task ends in
 * NEEDS_CLARIFICATION); a failure is recorded as FAILED; planning an unchanged task changes
 * nothing
 * Input
    - task: &str - task id
    - checkpoint: Option<String> - checkpoint (default: the configured one, else baseline)
    - by: &str - who plans
 * Output
    - Result<Value, String> the task as show() returns it
*/
pub(crate) fn prepare(task: &str, checkpoint: Option<String>, by: &str) -> Result<Value, String> {
    require_human("prepare")?;
    let (candidate, config) = find(task)?;
    let checkpoint = checkpoint.unwrap_or_else(|| config.text("checkpoint", "baseline"));
    let mut current = record(task)?;
    current["planning"] = json!({"at": now_unix(), "by": by});
    note(
        &mut current,
        IntakeState::Planning,
        "planning the task contract",
    );
    save(&current)?;
    let outcome = (|| -> Result<Value, String> {
        let context = fetch(&candidate)?;
        let input = normalize(&candidate, &config, &context)?;
        let tasks = root()?.join("tasks");
        fs::create_dir_all(&tasks).map_err(io_error)?;
        let path = tasks.join(format!("{task}.json"));
        let text = serde_json::to_string_pretty(&input.to_json()).map_err(io_error)? + "\n";
        // Rewriting identical content would not change the digest, but leaving it keeps mtime
        if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            fs::write(&path, &text).map_err(io_error)?;
        }
        task_contracts::compile(task, &checkpoint, &format!("{by} (crane task prepare)"))
    })();
    let mut current = record(task)?;
    current["planning"] = Value::Null;
    match outcome {
        Ok(_) => {
            current["failure"] = Value::Null;
            save(&current)?;
            show(task)
        }
        Err(error) => {
            current["failure"] = json!({"at": now_unix(), "error": error});
            note(&mut current, IntakeState::Failed, &error);
            save(&current)?;
            Err(format!("planning task {task} failed: {error}"))
        }
    }
}

/** Approve a task's contract (a human, quoting its digest)
 * Input
    - task: &str - task id
    - approver: Option<String> - who approves
    - confirm: Option<String> - digest prefix
 * Output
    - Result<Value, String> the task as show() returns it, with already_approved
*/
pub(crate) fn approve(
    task: &str,
    approver: Option<String>,
    confirm: Option<String>,
) -> Result<Value, String> {
    require_human("approve")?;
    find(task)?;
    let approved = task_contracts::approve(task, approver, confirm)?;
    let mut value = show(task)?;
    value["already_approved"] = approved["already_approved"].clone();
    Ok(value)
}

/** Check that a task's approved contract is still the one to launch: approved, its bindings still
 * hold, it is the latest version, the task file is unchanged, and the tracker's task still
 * normalizes to what was planned
 * Input
    - candidate: &Candidate - task
    - config: &SourceConfig - settings
 * Output
    - Result<Value, String> the contract, or why it is obsolete
*/
fn launchable(candidate: &Candidate, config: &SourceConfig) -> Result<Value, String> {
    let task = candidate.event.task_id.as_str();
    if adapter(candidate.source)?.done(&candidate.event.payload) {
        return Err(format!("task {task} is done in {}", candidate.source));
    }
    let contract = task_contracts::load(task, None)?.ok_or_else(|| {
        format!("task {task} has no contract; plan it with 'crane task prepare {task}'")
    })?;
    let id = contract["contract_id"].as_str().unwrap_or_default();
    match contract["status"].as_str().unwrap_or_default() {
        "approved" => {}
        "proposed" => {
            return Err(format!(
                "contract {id} is not approved yet; approve it with 'crane task approve {task}'"
            ))
        }
        "clarification_required" => {
            return Err(format!(
                "task {task} needs clarification; no session can start"
            ))
        }
        "invalidated" => {
            return Err(format!(
                "contract {id} is obsolete: {}",
                contract["invalidation"]["message"]
                    .as_str()
                    .unwrap_or_default()
            ))
        }
        other => {
            return Err(format!(
                "contract {id} is {other}; no session can start against it"
            ))
        }
    }
    if contract["task_changed"] == true {
        return Err(format!("contract {id} is obsolete: the task changed after it was compiled; plan it again with 'crane task prepare {task}'"));
    }
    let planned: Value = serde_json::from_str(
        &fs::read_to_string(root()?.join("tasks").join(format!("{task}.json")))
            .map_err(io_error)?,
    )
    .map_err(|error| error.to_string())?;
    let now = normalize(candidate, config, &fetch(candidate)?)?.to_json();
    if TaskInput::from_json(&planned)?.to_json() != now {
        return Err(format!("contract {id} is obsolete: task {task} changed in {} since it was planned; plan it again with 'crane task prepare {task}'", candidate.source));
    }
    Ok(contract)
}

/** Launch the agent session of a task's approved contract: refuse an obsolete contract, reuse
 * the session already launched for the same contract and agent (idempotent), refuse a second
 * agent while one holds the contract, and record the immutable launch binding
 * Input
    - task: &str - task id
    - agent: Option<String> - claude, codex, or generic (required)
    - autonomy: Option<Autonomy> - autonomy mode (default: configured, else delegated)
    - isolate: bool - work in a dedicated Git worktree
    - by: &str - who launches
 * Output
    - Result<Value, String> the task as show() returns it, with launched (false when reused)
*/
pub(crate) fn launch(
    task: &str,
    agent: Option<String>,
    autonomy: Option<Autonomy>,
    isolate: bool,
    by: &str,
) -> Result<Value, String> {
    require_human("launch")?;
    let agent = AgentKind::parse(Some(
        agent
            .as_deref()
            .ok_or("crane task launch requires --agent claude|codex|generic")?,
    ))?;
    let (candidate, config) = find(task)?;
    let contract = launchable(&candidate, &config)?;
    let digest = contract["digest"].as_str().unwrap_or_default().to_string();
    let version = contract["version"].as_u64().unwrap_or(0);

    // A session already holding this contract is reused by the same agent, refused to another
    for id in session_ids()? {
        let Ok(Some(session)) = ContractSession::load(&id) else {
            continue;
        };
        let described = session.describe();
        if described["task_id"] != task
            || described["governance"]["task_contract"]["digest"] != digest.as_str()
            || !session.resumable()
        {
            continue;
        }
        if described["agent"] != agent.name() {
            return Err(format!(
                "contract {} is already launched with {} as session {id}; cancel it first with 'crane agent session cancel {id}'",
                contract["contract_id"].as_str().unwrap_or_default(),
                described["agent"].as_str().unwrap_or_default()
            ));
        }
        let mut current = record(task)?;
        if current["launch"]["session_id"] != id.as_str() {
            record_launch(&mut current, &session, &contract, by)?;
        }
        let mut value = show(task)?;
        value["launched"] = json!(false);
        return Ok(value);
    }

    // A new session: one provider session id per contract version, and a fresh one after an
    // ended session
    let base = format!("task-{task}-v{version}");
    let mut provider = base.clone();
    let mut attempt = 1;
    while ContractSession::load(&format!("{}-{provider}", agent.name()))?.is_some() {
        attempt += 1;
        provider = format!("{base}-r{attempt}");
    }
    let options = SessionOptions {
        ttl: config.value["session_ttl"].as_u64(),
        idle_timeout: config.value["idle_timeout"].as_u64(),
        task: Some(task.to_string()),
        autonomy: match autonomy {
            Some(autonomy) => Some(autonomy),
            None => config.value["autonomy"]
                .as_str()
                .map(Autonomy::parse)
                .transpose()?,
        },
        max_actions: config.value["max_actions"].as_u64(),
        max_files: config.value["max_files"].as_u64(),
        checkpoint: contract["bindings"]["checkpoint"]["name"]
            .as_str()
            .unwrap_or("baseline")
            .to_string(),
        isolate,
    };
    let (session, _) = agent_session::open(agent, &provider, &options)?;
    let bound = &session.describe()["governance"]["task_contract"];
    if bound["digest"] != digest.as_str() || bound["status"] != "approved" {
        let id = session.id().to_string();
        agent_session::cancel(
            &id,
            "the session was not bound to the approved task contract",
        )?;
        return Err(format!(
            "session {id} was not bound to approved contract {}; it was cancelled",
            contract["contract_id"].as_str().unwrap_or_default()
        ));
    }
    let mut current = record(task)?;
    record_launch(&mut current, &session, &contract, by)?;
    let mut value = show(task)?;
    value["launched"] = json!(true);
    Ok(value)
}

/** Record a launch in the intake record: the session, agent, and the binding with its digest
 * Input
    - record: &mut Value - intake record
    - session: &ContractSession - launched session
    - contract: &Value - approved contract
    - by: &str - who launched
 * Output
    - Result<(), String>
*/
fn record_launch(
    record: &mut Value,
    session: &ContractSession,
    contract: &Value,
    by: &str,
) -> Result<(), String> {
    let binding = launch_binding(session, contract);
    let launch = json!({
        "session_id": session.id(),
        "agent": binding["agent"],
        "binding": binding,
        "binding_digest": sha256(binding.to_string().as_bytes()),
        "at": now_unix(),
        "by": by,
    });
    if let Some(launches) = record["launches"].as_array_mut() {
        launches.push(json!({"session_id": launch["session_id"], "agent": launch["agent"], "contract_id": contract["contract_id"], "binding_digest": launch["binding_digest"], "at": launch["at"], "by": by}));
    }
    record["launch"] = launch;
    note(
        record,
        IntakeState::Ready,
        &format!("READY TO RUN: session {} launched", session.id()),
    );
    save(record)
}
