// Task orchestration: external trackers (through TaskSourceAdapter) feed one lifecycle that
// receives a task, fetches its context, maps it to this repository, compiles a versioned task
// contract (crate::task_contracts), waits for human approval, starts the contract session, validates it, and follows
// the task to completion or cancellation. Events are replayed from the CLI or posted to a minimal
// local HTTP endpoint; there is no message bus.

pub(crate) mod adapters;
pub(crate) mod server;

#[cfg(test)]
mod tests;

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use crate::adapter::AgentKind;
use crate::agent_session::{self, SessionOptions};
use crate::proposals::store::{require_human, Proposal};
use crate::repository::root;
use crate::session::{read_attestation, session_ids, ContractSession};
use crate::task_contracts;
use crate::util::{io_error, now_unix, sha256};
use crate::zones::model::Autonomy;
use adapters::{adapter, EventKind, SourceEvent, TaskSourceAdapter};

/** Version of the task record layout */
const RECORD_FORMAT: u64 = 1;

/** Age after which an abandoned lock is taken over */
const STALE_LOCK: Duration = Duration::from_secs(120);

/** Where a task is in its lifecycle
 * Variants
    - Received - the society accepted the task
    - Analyzing - fetching context, mapping, and planning
    - ContractProposed - a task contract waits for human approval
    - Approved - the contract is active
    - Executing - an agent session works under the contract
    - Validating - the session's attestation is being checked
    - PrReady - the contract passed; a pull request can be opened
    - Review - the change is under review
    - Merged - the change was merged
    - Completed - the task is done
    - Blocked - the task cannot proceed without a human (clarification, conflict, mapping)
    - Failed - an unrecoverable error
    - Cancelled - the task was abandoned; its sessions were stopped
    - Degraded - the session can no longer be trusted (drift, repository mismatch, journal)
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskState {
    Received,
    Analyzing,
    ContractProposed,
    Approved,
    Executing,
    Validating,
    PrReady,
    Review,
    Merged,
    Completed,
    Blocked,
    Failed,
    Cancelled,
    Degraded,
}

impl TaskState {
    /** Every state */
    const ALL: [TaskState; 14] = [
        Self::Received,
        Self::Analyzing,
        Self::ContractProposed,
        Self::Approved,
        Self::Executing,
        Self::Validating,
        Self::PrReady,
        Self::Review,
        Self::Merged,
        Self::Completed,
        Self::Blocked,
        Self::Failed,
        Self::Cancelled,
        Self::Degraded,
    ];

    /** Return the state's name
     * Input
        - None (uses self)
     * Output
        - &'static str such as "CONTRACT_PROPOSED"
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Received => "RECEIVED",
            Self::Analyzing => "ANALYZING",
            Self::ContractProposed => "CONTRACT_PROPOSED",
            Self::Approved => "APPROVED",
            Self::Executing => "EXECUTING",
            Self::Validating => "VALIDATING",
            Self::PrReady => "PR_READY",
            Self::Review => "REVIEW",
            Self::Merged => "MERGED",
            Self::Completed => "COMPLETED",
            Self::Blocked => "BLOCKED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
            Self::Degraded => "DEGRADED",
        }
    }

    /** Parse a state name, ignoring case
     * Input
        - value: &str - state name
     * Output
        - Result<TaskState, String>
    */
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        let upper = value.to_ascii_uppercase().replace('-', "_");
        Self::ALL
            .into_iter()
            .find(|state| state.name() == upper)
            .ok_or_else(|| format!("unknown task state '{value}'"))
    }

    /** Check whether the lifecycle has ended (a reopen starts a new cycle)
     * Input
        - None (uses self)
     * Output
        - bool
    */
    pub(crate) fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }

    /** Check whether a transition is allowed by the state machine
     * Input
        - to: TaskState - next state
     * Output
        - bool
    */
    pub(crate) fn allows(self, to: TaskState) -> bool {
        use TaskState::*;
        if to == Cancelled {
            return !matches!(self, Completed | Cancelled | Merged);
        }
        matches!(
            (self, to),
            (Received, Analyzing)
                | (Analyzing, ContractProposed | Blocked | Failed)
                | (ContractProposed, Approved | Analyzing | Blocked)
                | (Approved, Executing | Analyzing)
                | (Executing, Validating | Analyzing | Degraded | Failed)
                | (
                    Validating,
                    PrReady | Executing | Degraded | Failed | Analyzing
                )
                | (PrReady, Review | Executing | Analyzing)
                | (Review, Merged | Executing | Analyzing)
                | (Merged, Completed)
                | (Blocked, Analyzing | Failed)
                | (Degraded, Executing | Analyzing | Failed)
                | (Failed, Analyzing)
                | (Cancelled | Completed, Received)
        )
    }
}

/** Exclusive access to the task records for one process, released on drop
 * Fields
    - path: PathBuf - lock file
*/
struct Lock {
    path: PathBuf,
}

impl Lock {
    /** Take the lock, waiting up to five seconds and taking over a lock older than two minutes
     * Input
        - None
     * Output
        - Result<Lock, String>
        - Error if another process holds the lock
    */
    fn acquire() -> Result<Self, String> {
        let directory = tasks_directory()?;
        fs::create_dir_all(&directory).map_err(io_error)?;
        let path = directory.join(".lock");
        for _ in 0..50 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age > STALE_LOCK);
                    if stale {
                        let _ = fs::remove_file(&path);
                    } else {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        Err("another crane task process holds the task lock; try again".into())
    }
}

impl Drop for Lock {
    /** Release the lock
     * Input
        - None (uses self)
     * Output
        - None
    */
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/** Return the task records directory, .crane/runtime/tasks, without creating anything (for reads)
 * Input
    - None
 * Output
    - Result<PathBuf, String>
*/
fn tasks_path() -> Result<PathBuf, String> {
    Ok(root()?.join("runtime").join("tasks"))
}

/** Return the task records directory, .crane/runtime/tasks, making sure the runtime directory
 * exists and is kept out of Git (for writes)
 * Input
    - None
 * Output
    - Result<PathBuf, String>
*/
fn tasks_directory() -> Result<PathBuf, String> {
    let runtime = root()?.join("runtime");
    fs::create_dir_all(&runtime).map_err(io_error)?;
    let ignore = runtime.join(".gitignore");
    if !ignore.exists() {
        fs::write(ignore, "*\n").map_err(io_error)?;
    }
    Ok(runtime.join("tasks"))
}

/** The orchestration settings and repository mapping in .crane/sources/config.json
 * Fields
    - value: Value - the configuration document
*/
pub(crate) struct SourceConfig {
    pub(crate) value: Value,
}

impl SourceConfig {
    /** Load the configuration (an empty one when the file does not exist)
     * Input
        - None
     * Output
        - Result<SourceConfig, String>
        - Error if the file is not valid JSON
    */
    pub(crate) fn load() -> Result<Self, String> {
        let path = root()?.join("sources").join("config.json");
        let value = match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content)
                .map_err(|error| format!("{}: {error}", path.display()))?,
            Err(error) if error.kind() == ErrorKind::NotFound => json!({}),
            Err(error) => return Err(io_error(error)),
        };
        Ok(Self { value })
    }

    /** Check whether an assignee means the society: any configured agent assignee matches one of
     * the assignee's names or ids (with none configured, every assigned task is accepted)
     * Input
        - assignees: &[String] - names and ids of the task's assignee
     * Output
        - bool
    */
    pub(crate) fn for_society(&self, assignees: &[String]) -> bool {
        match self.value["agent_assignees"].as_array() {
            Some(agents) if !agents.is_empty() => agents.iter().any(|agent| {
                agent.as_str().is_some_and(|agent| {
                    assignees
                        .iter()
                        .any(|assignee| assignee.eq_ignore_ascii_case(agent))
                })
            }),
            _ => !assignees.is_empty(),
        }
    }

    /** Return the repository mapping of a source project
     * Input
        - source: &str - source name
        - project: &str - project key or gid
     * Output
        - Option<&Value> {"repositories": [...], "team": ...}
    */
    pub(crate) fn mapping(&self, source: &str, project: &str) -> Option<&Value> {
        self.value[source]["projects"].get(project)
    }

    /** Return a text setting with a default
     * Input
        - key: &str - setting
        - default: &str - default value
     * Output
        - String
    */
    pub(crate) fn text(&self, key: &str, default: &str) -> String {
        self.value[key].as_str().unwrap_or(default).to_string()
    }
}

/** One task's lifecycle record, .crane/runtime/tasks/ID/state.json
 * Fields
    - id: String - task id
    - value: Value - the record document
*/
pub(crate) struct TaskRecord {
    id: String,
    pub(crate) value: Value,
}

impl TaskRecord {
    /** Return a record's file
     * Input
        - id: &str - task id
     * Output
        - Result<PathBuf, String>
    */
    fn path(id: &str) -> Result<PathBuf, String> {
        Ok(tasks_path()?.join(id).join("state.json"))
    }

    /** Load a record
     * Input
        - id: &str - task id
     * Output
        - Result<Option<TaskRecord>, String>, None for an unknown task
    */
    pub(crate) fn load(id: &str) -> Result<Option<Self>, String> {
        match fs::read_to_string(Self::path(id)?) {
            Ok(content) => Ok(Some(Self {
                id: id.into(),
                value: serde_json::from_str(&content)
                    .map_err(|error| format!("task record {id} is invalid: {error}"))?,
            })),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error(error)),
        }
    }

    /** List every record, sorted by task id
     * Input
        - None
     * Output
        - Result<Vec<TaskRecord>, String>
    */
    pub(crate) fn all() -> Result<Vec<Self>, String> {
        let mut ids = match fs::read_dir(tasks_path()?) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().join("state.json").is_file())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(io_error(error)),
        };
        ids.sort();
        ids.iter()
            .filter_map(|id| Self::load(id).transpose())
            .collect()
    }

    /** Create the record of a newly received task, in state RECEIVED
     * Input
        - event: &SourceEvent - first event
        - source: &str - source name
     * Output
        - TaskRecord
    */
    fn create(event: &SourceEvent, source: &str) -> Self {
        let mut record = Self {
            id: event.task_id.clone(),
            value: json!({
                "orchestration_format": RECORD_FORMAT,
                "task_id": event.task_id,
                "source": source,
                "external_id": event.external_id,
                "state": TaskState::Received.name(),
                "reason": null,
                "contract_version": 0,
                "task_digest": null,
                "proposal": null,
                "active_proposal": null,
                "sessions": [],
                "processed_events": [],
                "versions": [],
                "history": [],
                "validated_attestation": null,
            }),
        };
        record.note(
            None,
            TaskState::Received,
            &event.event_id,
            "task received by the society",
        );
        record
    }

    /** Return the current state
     * Input
        - None (uses self)
     * Output
        - TaskState
    */
    pub(crate) fn state(&self) -> TaskState {
        TaskState::parse(self.value["state"].as_str().unwrap_or("FAILED"))
            .unwrap_or(TaskState::Failed)
    }

    /** Append a history entry
     * Input
        - from: Option<TaskState> - previous state
        - to: TaskState - new state
        - cause: &str - event id or command
        - reason: &str - why
     * Output
        - None
    */
    fn note(&mut self, from: Option<TaskState>, to: TaskState, cause: &str, reason: &str) {
        if let Some(history) = self.value["history"].as_array_mut() {
            history.push(json!({
                "from": from.map(TaskState::name),
                "to": to.name(),
                "at": now_unix(),
                "cause": cause,
                "reason": reason,
            }));
        }
    }

    /** Move to a new state when the state machine allows it
     * Input
        - to: TaskState - next state
        - cause: &str - event id or command
        - reason: &str - why
     * Output
        - Result<(), String>
        - Error for a transition the state machine does not allow
    */
    fn transition(&mut self, to: TaskState, cause: &str, reason: &str) -> Result<(), String> {
        let from = self.state();
        if !from.allows(to) {
            return Err(format!(
                "task {} cannot move from {} to {}",
                self.id,
                from.name(),
                to.name()
            ));
        }
        self.value["state"] = json!(to.name());
        self.value["reason"] = json!(reason);
        self.note(Some(from), to, cause, reason);
        Ok(())
    }

    /** Check whether an event was already processed
     * Input
        - event_id: &str - event id
     * Output
        - bool
    */
    fn processed(&self, event_id: &str) -> bool {
        self.value["processed_events"]
            .as_array()
            .is_some_and(|events| events.iter().any(|event| event == event_id))
    }

    /** Record an event as processed
     * Input
        - event_id: &str - event id
     * Output
        - None
    */
    fn mark(&mut self, event_id: &str) {
        if !self.processed(event_id) {
            if let Some(events) = self.value["processed_events"].as_array_mut() {
                events.push(json!(event_id));
            }
        }
    }

    /** Return the recorded session ids
     * Input
        - None (uses self)
     * Output
        - Vec<String>
    */
    fn sessions(&self) -> Vec<String> {
        self.value["sessions"]
            .as_array()
            .map(|sessions| {
                sessions
                    .iter()
                    .filter_map(|session| session.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /** Write the record
     * Input
        - None (uses self)
     * Output
        - Result<(), String>
    */
    fn save(&self) -> Result<(), String> {
        tasks_directory()?;
        let path = Self::path(&self.id)?;
        fs::create_dir_all(path.parent().ok_or("invalid task record path")?).map_err(io_error)?;
        let temporary = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(
            &temporary,
            serde_json::to_string_pretty(&self.value).map_err(io_error)? + "\n",
        )
        .map_err(io_error)?;
        fs::rename(&temporary, &path).map_err(io_error)
    }
}

/** Build the outcome of one processed event
 * Input
    - event: &SourceEvent - event
    - result: &str - what happened
    - record: Option<&TaskRecord> - the task's record afterwards
 * Output
    - Value JSON object
*/
fn outcome(event: &SourceEvent, result: &str, record: Option<&TaskRecord>) -> Value {
    json!({
        "task_id": event.task_id,
        "event_id": event.event_id,
        "event": event.kind.name(),
        "detail": event.detail,
        "result": result,
        "state": record.map(|record| record.state().name()),
        "contract_version": record.map(|record| record.value["contract_version"].clone()),
        "proposal": record.map(|record| record.value["proposal"].clone()),
    })
}

/** Ingest one webhook delivery from a task source: translate it with the source's adapter and
 * run every event through the lifecycle under the task lock; refused on behalf of an agent
 * Input
    - source: &str - jira or asana
    - body: &Value - webhook JSON
    - delivery: Option<&str> - delivery id from the transport, if any
 * Output
    - Result<Vec<Value>, String> one outcome per event
    - Error for an unknown source, a malformed delivery, or an agent environment
*/
pub(crate) fn ingest(
    source: &str,
    body: &Value,
    delivery: Option<&str>,
) -> Result<Vec<Value>, String> {
    require_human("ingest")?;
    let adapter = adapter(source)?;
    let config = SourceConfig::load()?;
    let events = adapter.parse(body, delivery)?;
    let _lock = Lock::acquire()?;
    events
        .iter()
        .map(|event| {
            let result = process(adapter.as_ref(), &config, event)?;
            if let Some(mut record) = TaskRecord::load(&event.task_id)? {
                advance_automatically(&mut record, &config, &event.event_id)?;
                record.save()?;
                let mut result = result;
                result["state"] = json!(record.state().name());
                return Ok(result);
            }
            Ok(result)
        })
        .collect()
}

/** Run one event through the lifecycle: ignore duplicates (by event id) and events for unknown
 * tasks not assigned to the society; finish closed or cancelled tasks; otherwise fetch the
 * context, check the assignment, start a new cycle for a reopened task, and analyze it
 * Input
    - adapter: &dyn TaskSourceAdapter - source adapter
    - config: &SourceConfig - settings and repository mapping
    - event: &SourceEvent - event
 * Output
    - Result<Value, String> the outcome
*/
fn process(
    adapter: &dyn TaskSourceAdapter,
    config: &SourceConfig,
    event: &SourceEvent,
) -> Result<Value, String> {
    let existing = TaskRecord::load(&event.task_id)?;
    if existing
        .as_ref()
        .is_some_and(|record| record.processed(&event.event_id))
    {
        return Ok(outcome(event, "duplicate event ignored", existing.as_ref()));
    }
    let snapshots = root()?.join("sources");
    let context = match event.kind {
        EventKind::Cancelled | EventKind::Ignored => None,
        _ => Some(adapter.fetch(event, &snapshots)),
    };
    let kind = match (event.kind, &context) {
        (EventKind::Closed, Some(Ok(context))) if !adapter.closed(context) => EventKind::Reopened,
        (kind, _) => kind,
    };
    let Some(mut record) = existing else {
        // Only tasks given to the society start a lifecycle
        let assigned = matches!(&context, Some(Ok(context)) if config.for_society(&adapter.assignees(context)));
        let fetch_failed = matches!(&context, Some(Err(_))) && kind == EventKind::Created;
        if !(matches!(
            kind,
            EventKind::Created | EventKind::Assigned | EventKind::Reopened
        ) && (assigned || fetch_failed))
        {
            return Ok(outcome(
                event,
                "ignored: not a task assigned to the society",
                None,
            ));
        }
        let mut record = TaskRecord::create(event, adapter.name());
        let result = analyze(
            &mut record,
            adapter,
            config,
            event,
            context.unwrap_or_else(|| Err("no context".into())),
        )?;
        record.mark(&event.event_id);
        record.save()?;
        return Ok(outcome(event, &result, Some(&record)));
    };
    let result = match kind {
        EventKind::Ignored => "ignored".to_string(),
        EventKind::Closed | EventKind::Cancelled => {
            if record.state().terminal() {
                "already finished".to_string()
            } else if kind == EventKind::Closed && record.state() == TaskState::Merged {
                crate::task_completion::completed_externally(&record.id)?;
                finish(
                    &mut record,
                    TaskState::Completed,
                    &event.event_id,
                    "closed in the tracker after merge",
                )?;
                "completed".to_string()
            } else {
                let reason = if kind == EventKind::Closed {
                    "closed in the tracker before the change was merged"
                } else {
                    "cancelled in the tracker"
                };
                finish(&mut record, TaskState::Cancelled, &event.event_id, reason)?;
                "cancelled".to_string()
            }
        }
        _ => {
            let context = context.unwrap_or_else(|| Err("no context".into()));
            let assigned = context
                .as_ref()
                .map(|context| config.for_society(&adapter.assignees(context)))
                .unwrap_or(true);
            if record.state().terminal() {
                if kind == EventKind::Reopened || (kind == EventKind::Assigned && assigned) {
                    record.transition(
                        TaskState::Received,
                        &event.event_id,
                        "task reopened or reassigned to the society",
                    )?;
                    analyze(&mut record, adapter, config, event, context)?
                } else {
                    "ignored: the task is finished".to_string()
                }
            } else if kind == EventKind::Assigned && !assigned {
                finish(
                    &mut record,
                    TaskState::Cancelled,
                    &event.event_id,
                    "unassigned from the society",
                )?;
                "cancelled".to_string()
            } else {
                analyze(&mut record, adapter, config, event, context)?
            }
        }
    };
    record.mark(&event.event_id);
    record.save()?;
    Ok(outcome(event, &result, Some(&record)))
}

/** Analyze a task: normalize it with the adapter, map its project to repositories, and when its
 * content changed, write the new task version, supersede the previous pending contract, stop
 * sessions bound to an outdated contract, plan it, and propose the versioned contract (or block
 * with the planner's reasons); unchanged content changes nothing
 * Input
    - record: &mut TaskRecord - task record
    - adapter: &dyn TaskSourceAdapter - source adapter
    - config: &SourceConfig - settings and repository mapping
    - event: &SourceEvent - event
    - context: Result<Value, String> - fetched context, or why it is unavailable
 * Output
    - Result<String, String> what happened
*/
fn analyze(
    record: &mut TaskRecord,
    adapter: &dyn TaskSourceAdapter,
    config: &SourceConfig,
    event: &SourceEvent,
    context: Result<Value, String>,
) -> Result<String, String> {
    let cause = event.event_id.as_str();
    let block = |record: &mut TaskRecord, reason: String| -> Result<String, String> {
        if record.state() != TaskState::Analyzing {
            record.transition(TaskState::Analyzing, cause, "analyzing the task")?;
        }
        record.transition(TaskState::Blocked, cause, &reason)?;
        Ok(format!("blocked: {reason}"))
    };
    let context = match context {
        Ok(context) => context,
        Err(error) => return block(record, format!("task context unavailable: {error}")),
    };
    let mut input = match adapter.normalize(
        event,
        &context,
        config.value[adapter.name()]["acceptance_field"].as_str(),
    ) {
        Ok(input) => input,
        Err(error) => return block(record, format!("task could not be normalized: {error}")),
    };
    let project = adapter.project(&context).unwrap_or_default();
    let Some(mapping) = config.mapping(adapter.name(), &project) else {
        return block(
            record,
            format!("no repository mapping for {} project '{project}'; add it to .crane/sources/config.json", adapter.name()),
        );
    };
    input.repositories = mapping["repositories"]
        .as_array()
        .map(|repositories| {
            repositories
                .iter()
                .filter_map(|repository| repository.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if let Some(team) = mapping["team"].as_str() {
        input.team = Some(team.into());
    }
    let normalized = input.to_json();
    let digest = sha256(normalized.to_string().as_bytes());
    let state = record.state();
    if record.value["task_digest"] == digest.as_str() && state != TaskState::Received {
        return Ok(
            "unchanged: the task content is the same, so the contract version is kept".into(),
        );
    }
    let executing = matches!(
        state,
        TaskState::Executing
            | TaskState::Validating
            | TaskState::PrReady
            | TaskState::Review
            | TaskState::Degraded
    );
    record.transition(TaskState::Analyzing, cause, "analyzing the task")?;
    if executing {
        stop_sessions(
            record,
            "the task changed; its contract needs a new version and approval",
            false,
        )?;
    }
    record.value["task_digest"] = json!(digest);
    let tasks = root()?.join("tasks");
    fs::create_dir_all(&tasks).map_err(io_error)?;
    let text = serde_json::to_string_pretty(&normalized).map_err(io_error)? + "\n";
    fs::write(tasks.join(format!("{}.json", record.id)), &text).map_err(io_error)?;
    if let Some(previous) = record.value["proposal"].as_str().map(String::from) {
        if let Ok(mut proposal) = Proposal::load(&previous) {
            proposal.supersede("a new task version")?;
        }
    }
    // The task contract layer plans, binds, versions, and proposes the contract
    let checkpoint = config.text("checkpoint", "baseline");
    let generator = format!(
        "crane task ingest ({} {})",
        adapter.name(),
        event.kind.name()
    );
    let compiled = match task_contracts::compile(&record.id, &checkpoint, &generator) {
        Ok(compiled) => compiled,
        Err(error) => {
            record.transition(
                TaskState::Failed,
                cause,
                &format!("planning failed: {error}"),
            )?;
            return Ok(format!("failed: {error}"));
        }
    };
    let version = compiled["version"].as_u64().unwrap_or(1);
    record.value["contract_version"] = json!(version);
    let archive = TaskRecord::path(&record.id)?.with_file_name(format!("task.v{version}.json"));
    if let Some(folder) = archive.parent() {
        fs::create_dir_all(folder).map_err(io_error)?;
    }
    fs::write(archive, &text).map_err(io_error)?;
    let status = compiled["plan_status"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if let Some(versions) = record.value["versions"].as_array_mut() {
        versions.push(json!({"version": version, "task_digest": digest, "event": cause, "plan_status": status, "contract_status": compiled["status"], "contract_digest": compiled["digest"]}));
    }
    if compiled["status"] != "proposed" {
        let details = compiled["clarifications"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["blocking"] != false)
            .chain(compiled["conflicts"].as_array().into_iter().flatten())
            .filter_map(|item| item["message"].as_str())
            .collect::<Vec<_>>()
            .join("; ");
        record.value["proposal"] = Value::Null;
        record.transition(TaskState::Blocked, cause, &format!("{status}: {details}"))?;
        return Ok(format!("blocked: {status}"));
    }
    let name = compiled["proposal"]["proposal_id"]
        .as_str()
        .or(compiled["contract_id"].as_str())
        .unwrap_or_default()
        .to_string();
    record.value["proposal"] = compiled["proposal"]["proposal_id"].clone();
    record.transition(
        TaskState::ContractProposed,
        cause,
        &format!("task contract {name} proposed; waiting for human approval"),
    )?;
    Ok(format!("proposed {name}"))
}

/** Adopt a task contract version compiled after the one the record follows (a human compiled the
 * task again, after it was blocked or while its earlier version waited), when it is proposed or
 * already approved
 * Input
    - record: &mut TaskRecord - task record
    - cause: &str - event id or command
 * Output
    - Result<bool, String> whether a newer version was adopted
*/
fn adopt(record: &mut TaskRecord, cause: &str) -> Result<bool, String> {
    let Some(latest) = task_contracts::load(&record.id, None).ok().flatten() else {
        return Ok(false);
    };
    let version = latest["version"].as_u64().unwrap_or(0);
    if version <= record.value["contract_version"].as_u64().unwrap_or(0)
        || !matches!(latest["status"].as_str(), Some("proposed" | "approved"))
    {
        return Ok(false);
    }
    record.value["contract_version"] = json!(version);
    record.value["proposal"] = latest["proposal"]["proposal_id"].clone();
    if let Some(versions) = record.value["versions"].as_array_mut() {
        versions.push(json!({"version": version, "task_digest": latest["bindings"]["task"]["task_digest"], "event": cause, "plan_status": latest["plan_status"], "contract_status": latest["status"], "contract_digest": latest["digest"]}));
    }
    let id = latest["contract_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    record.transition(
        TaskState::Analyzing,
        cause,
        &format!("task contract {id} was compiled"),
    )?;
    record.transition(
        TaskState::ContractProposed,
        cause,
        &format!("task contract {id} proposed; waiting for human approval"),
    )?;
    Ok(true)
}

/** Stop a task whose contract was invalidated (its checkpoint, policies, zones, autonomy, budget,
 * organization, or task changed): its sessions are cancelled and it is blocked until a human
 * compiles and approves the contract again
 * Input
    - record: &mut TaskRecord - task record
    - cause: &str - event id or command
 * Output
    - Result<bool, String> whether the task was stopped
*/
fn stop_if_invalidated(record: &mut TaskRecord, cause: &str) -> Result<bool, String> {
    let version = record.value["contract_version"].as_u64().unwrap_or(0);
    let Some(contract) = task_contracts::load(&record.id, Some(version))
        .ok()
        .flatten()
    else {
        return Ok(false);
    };
    if contract["status"] != "invalidated" {
        return Ok(false);
    }
    let reason = format!(
        "contract {} invalidated: {}",
        contract["contract_id"].as_str().unwrap_or_default(),
        contract["invalidation"]["message"]
            .as_str()
            .unwrap_or_default()
    );
    stop_sessions(record, &reason, false)?;
    record.transition(
        TaskState::Analyzing,
        cause,
        "the task contract was invalidated",
    )?;
    record.transition(TaskState::Blocked, cause, &reason)?;
    Ok(true)
}

/** End every contract session of a task (recorded ones and any bound to the task id) through
 * the session manager: cancelled sessions can never act again, finalized ones are reconciled one
 * last time first; either way their runtime authority ends and the worktree is never touched, so
 * stopping is always safe
 * Input
    - record: &mut TaskRecord - task record
    - reason: &str - why, journaled in each session
    - finalize: bool - finalize (task completed) instead of cancelling
 * Output
    - Result<usize, String> sessions ended
*/
fn stop_sessions(record: &mut TaskRecord, reason: &str, finalize: bool) -> Result<usize, String> {
    let mut ids = record.sessions();
    for id in session_ids()? {
        if !ids.contains(&id) {
            if let Ok(Some(session)) = ContractSession::load(&id) {
                if session.describe()["task_id"] == record.id.as_str() {
                    ids.push(id);
                }
            }
        }
    }
    let mut ended = 0;
    for id in ids {
        let Ok(Some(session)) = ContractSession::load(&id) else {
            continue;
        };
        if !session.resumable() {
            continue;
        }
        session.record(json!({"event": "task_stopped", "reason": reason}))?;
        if finalize {
            agent_session::finalize(&id)?;
        } else {
            agent_session::cancel(&id, reason)?;
        }
        ended += 1;
    }
    Ok(ended)
}

/** End a task's lifecycle (Completed or Cancelled): stop its sessions, supersede its pending
 * contract, and retire its active contract
 * Input
    - record: &mut TaskRecord - task record
    - to: TaskState - Completed or Cancelled
    - cause: &str - event id or command
    - reason: &str - why
 * Output
    - Result<(), String>
*/
fn finish(record: &mut TaskRecord, to: TaskState, cause: &str, reason: &str) -> Result<(), String> {
    record.transition(to, cause, reason)?;
    stop_sessions(record, reason, to == TaskState::Completed)?;
    for key in ["proposal", "active_proposal"] {
        if let Some(name) = record.value[key].as_str().map(String::from) {
            if let Ok(mut proposal) = Proposal::load(&name) {
                proposal.supersede(&format!("task {}", to.name().to_ascii_lowercase()))?;
                proposal.retire(reason)?;
            }
        }
    }
    record.value["active_proposal"] = Value::Null;
    Ok(())
}

/** Advance a task as far as the facts allow, without a human decision: a proposed contract
 * becomes APPROVED once a human approved it (retiring the previous version) or BLOCKED if
 * rejected; an approved contract starts its session (EXECUTING); a new attestation of the session
 * is validated (VALIDATING, then PR_READY on PASS, DEGRADED on a session problem, or back to
 * EXECUTING for work the agent still has to do)
 * Input
    - record: &mut TaskRecord - task record
    - config: &SourceConfig - settings (agent profile and session lifetime)
    - cause: &str - event id or command
 * Output
    - Result<(), String>
*/
fn advance_automatically(
    record: &mut TaskRecord,
    config: &SourceConfig,
    cause: &str,
) -> Result<(), String> {
    for _ in 0..6 {
        match record.state() {
            TaskState::Blocked => {
                if !adopt(record, cause)? {
                    return Ok(());
                }
            }
            TaskState::ContractProposed => {
                if adopt(record, cause)? {
                    continue;
                }
                let version = record.value["contract_version"].as_u64().unwrap_or(0);
                let contract = task_contracts::load(&record.id, Some(version))
                    .ok()
                    .flatten();
                let Some(name) =
                    record.value["proposal"]
                        .as_str()
                        .map(String::from)
                        .or_else(|| {
                            contract.as_ref().and_then(|contract| {
                                contract["contract_id"].as_str().map(String::from)
                            })
                        })
                else {
                    return Ok(());
                };
                // The contract follows its proposal, so either approval path is seen here
                let status = match &contract {
                    Some(contract) => match contract["status"].as_str().unwrap_or_default() {
                        "approved" => "approved",
                        "rejected" => "rejected",
                        "invalidated" => {
                            stop_if_invalidated(record, cause)?;
                            return Ok(());
                        }
                        _ => "pending",
                    }
                    .to_string(),
                    None => Proposal::load(&name)?.status().to_string(),
                };
                let proposal = Proposal::load(&name).ok();
                match status.as_str() {
                    "approved" => {
                        if let Some(previous) =
                            record.value["active_proposal"].as_str().map(String::from)
                        {
                            if previous != name {
                                if let Ok(mut old) = Proposal::load(&previous) {
                                    old.retire(&format!("replaced by {name}"))?;
                                }
                            }
                        }
                        record.value["active_proposal"] = json!(name);
                        let approver = contract
                            .as_ref()
                            .and_then(|contract| contract["approval"]["approver"].as_str())
                            .or_else(|| {
                                proposal.as_ref().and_then(|proposal| {
                                    proposal.document["activation"]["approver"].as_str()
                                })
                            })
                            .unwrap_or("a human")
                            .to_string();
                        record.transition(
                            TaskState::Approved,
                            cause,
                            &format!("contract {name} approved by {approver}"),
                        )?;
                    }
                    "rejected" => {
                        let reason = contract
                            .as_ref()
                            .and_then(|contract| contract["rejection"]["reason"].as_str())
                            .or_else(|| {
                                proposal.as_ref().and_then(|proposal| {
                                    proposal.document["rejection"]["reason"].as_str()
                                })
                            })
                            .unwrap_or("no reason given")
                            .to_string();
                        record.transition(
                            TaskState::Blocked,
                            cause,
                            &format!("contract {name} rejected: {reason}"),
                        )?;
                    }
                    _ => return Ok(()),
                }
            }
            TaskState::Approved => {
                if stop_if_invalidated(record, cause)? {
                    return Ok(());
                }
                let session = start_session(record, config)?;
                record.transition(
                    TaskState::Executing,
                    cause,
                    &format!("contract session {session} started"),
                )?;
            }
            TaskState::Executing => {
                if stop_if_invalidated(record, cause)? {
                    return Ok(());
                }
                let Some(session) = record.sessions().last().cloned() else {
                    return Ok(());
                };
                let Some(attestation) = read_attestation(&session)? else {
                    return Ok(());
                };
                // Each stop writes a new attestation (its journal digest changes), so a digest
                // identifies the attestations already validated
                let digest = sha256(attestation.to_string().as_bytes());
                if record.value["validated_attestation"] == digest.as_str() {
                    return Ok(());
                }
                record.value["validated_attestation"] = json!(digest);
                record.transition(
                    TaskState::Validating,
                    cause,
                    &format!("validating the attestation of {session}"),
                )?;
                let session_problem = attestation["findings"].as_array().is_some_and(|findings| {
                    findings.iter().any(|finding| {
                        matches!(
                            finding["violation_type"].as_str(),
                            Some("contract_drift" | "repository_mismatch" | "journal_error")
                        )
                    })
                });
                if attestation["final_status"] == "PASS" {
                    record.transition(
                        TaskState::PrReady,
                        cause,
                        "the contract passed; a pull request can be opened",
                    )?;
                } else if session_problem {
                    record.transition(TaskState::Degraded, cause, "the session can no longer be trusted (drift, repository mismatch, or journal problem)")?;
                } else {
                    record.transition(
                        TaskState::Executing,
                        cause,
                        "the contract is not satisfied yet",
                    )?;
                    return Ok(());
                }
            }
            _ => return Ok(()),
        }
    }
    Ok(())
}

/** Start (or find) the contract session of the current contract version: its provider session id
 * is task-ID-vN, so the same version can never get two sessions, and it is bound to the task id
 * Input
    - record: &mut TaskRecord - task record
    - config: &SourceConfig - agent profile and session lifetime
 * Output
    - Result<String, String> the Crane session id
*/
fn start_session(record: &mut TaskRecord, config: &SourceConfig) -> Result<String, String> {
    let profile = AgentKind::parse(Some(&config.text("agent_profile", "generic")))?;
    let version = record.value["contract_version"].as_u64().unwrap_or(1);
    let provider = format!("task-{}-v{version}", record.id);
    let options = SessionOptions {
        ttl: config.value["session_ttl"].as_u64(),
        idle_timeout: config.value["idle_timeout"].as_u64(),
        task: Some(record.id.clone()),
        autonomy: config.value["autonomy"]
            .as_str()
            .map(Autonomy::parse)
            .transpose()?,
        max_actions: config.value["max_actions"].as_u64(),
        max_files: config.value["max_files"].as_u64(),
        checkpoint: config.text("checkpoint", "baseline"),
        isolate: config.value["isolate"] == true,
    };
    let (session, created) = agent_session::open(profile, &provider, &options)?;
    if created {
        session.record(json!({"event": "session_start", "source": "task orchestration", "crane_version": env!("CARGO_PKG_VERSION"), "model": null, "resumed": false}))?;
    }
    let id = session.describe()["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if !record.sessions().contains(&id) {
        if let Some(sessions) = record.value["sessions"].as_array_mut() {
            sessions.push(json!(id));
        }
    }
    Ok(id)
}

/** Reconcile one task (or every unfinished task) with approvals and attestations, advancing it
 * as far as the facts allow; refused on behalf of an agent
 * Input
    - id: Option<&str> - task id, every task when None
 * Output
    - Result<Vec<Value>, String> the records afterwards
*/
pub(crate) fn sync(id: Option<&str>) -> Result<Vec<Value>, String> {
    require_human("sync")?;
    let config = SourceConfig::load()?;
    let _lock = Lock::acquire()?;
    let records = match id {
        Some(id) => {
            vec![TaskRecord::load(id)?.ok_or_else(|| format!("no task '{id}'; ingest it first"))?]
        }
        None => TaskRecord::all()?,
    };
    let mut results = Vec::new();
    for mut record in records {
        if !record.state().terminal() {
            advance_automatically(&mut record, &config, "crane task sync")?;
            record.save()?;
        }
        results.push(record.value.clone());
    }
    Ok(results)
}

/** Move a task to a state on a human's or trusted process's word (for example PR_READY to
 * REVIEW, REVIEW to MERGED, MERGED to COMPLETED, or BLOCKED, FAILED, DEGRADED, CANCELLED);
 * finishing states stop sessions and retire the contract; refused on behalf of an agent
 * Input
    - id: &str - task id
    - to: TaskState - next state
    - reason: Option<String> - why
 * Output
    - Result<Value, String> the record afterwards
    - Error for an unknown task or a transition the state machine does not allow
*/
pub(crate) fn advance(id: &str, to: TaskState, reason: Option<String>) -> Result<Value, String> {
    require_human("advance")?;
    let _lock = Lock::acquire()?;
    let mut record = TaskRecord::load(id)?.ok_or_else(|| format!("no task '{id}'"))?;
    let reason = reason.unwrap_or_else(|| format!("moved to {} by a human", to.name()));
    if record.state().allows(to) {
        credit_milestone(id, to)?;
    }
    if to.terminal() {
        finish(&mut record, to, "crane task advance", &reason)?;
    } else {
        record.transition(to, "crane task advance", &reason)?;
    }
    record.save()?;
    Ok(record.value)
}

/** Move a task along as its delivery progresses, without a separate human command: an opened pull
 * request moves PR_READY to REVIEW; a verified merge moves the task to MERGED and then COMPLETED
 * (stopping its sessions and retiring its contract); untracked tasks are left alone
 * Input
    - id: &str - task id
    - merged: Option<&str> - the verified merge commit, None when a pull request was opened
 * Output
    - Result<Option<Value>, String> the task's tracker identity {source, external_id} when it was
      completed (for the tracker completion message), None otherwise
*/
pub(crate) fn delivery_progress(id: &str, merged: Option<&str>) -> Result<Option<Value>, String> {
    let _lock = Lock::acquire()?;
    let Some(mut record) = TaskRecord::load(id)? else {
        return Ok(None);
    };
    let cause = "crane deliver";
    match merged {
        None => {
            if record.state() == TaskState::PrReady {
                credit_milestone(id, TaskState::Review)?;
                record.transition(TaskState::Review, cause, "a pull request was opened")?;
                record.save()?;
            }
            Ok(None)
        }
        Some(sha) => {
            for step in [TaskState::Review, TaskState::Merged] {
                if record.state().allows(step) && record.state() != step {
                    credit_milestone(id, step)?;
                    record.transition(step, cause, &format!("merged as {sha}"))?;
                }
            }
            if record.state() != TaskState::Merged {
                record.save()?;
                return Err(format!(
                    "task {id} is {} and cannot be completed by a merge",
                    record.state().name()
                ));
            }
            // MERGED, not COMPLETED: the task completes once its tracker completion is confirmed
            record.value["merge_sha"] = json!(sha);
            record.save()?;
            Ok(Some(
                json!({"source": record.value["source"], "external_id": record.value["external_id"]}),
            ))
        }
    }
}

/** Complete a merged task once its tracker completion is confirmed (by the completion dispatcher,
 * or because the issue was already completed); a task that is not MERGED is left as it is
 * Input
    - id: &str - task id
    - event: &str - completion event id
 * Output
    - Result<bool, String> whether the task was completed now
*/
pub(crate) fn completion_confirmed(id: &str, event: &str) -> Result<bool, String> {
    let _lock = Lock::acquire()?;
    let Some(mut record) = TaskRecord::load(id)? else {
        return Ok(false);
    };
    if record.state() != TaskState::Merged {
        return Ok(false);
    }
    credit_milestone(id, TaskState::Completed)?;
    record.value["completion_event"] = json!(event);
    let reason = format!(
        "merged as {} and completed in the tracker ({event})",
        record.value["merge_sha"].as_str().unwrap_or_default()
    );
    finish(&mut record, TaskState::Completed, event, &reason)?;
    record.save()?;
    Ok(true)
}

/** Regenerate the budget of a task's open sessions for a milestone a human reported: a pull
 * request ready or the task completed is a task milestone, a merge is a passed human review and a
 * merge; each is credited once per task and state
 * Input
    - id: &str - task id
    - to: TaskState - the state the task moves to
 * Output
    - Result<(), String>
*/
fn credit_milestone(id: &str, to: TaskState) -> Result<(), String> {
    let sources: &[&str] = match to {
        TaskState::PrReady | TaskState::Completed => &["task_milestone"],
        TaskState::Merged => &["human_review", "merge"],
        _ => return Ok(()),
    };
    for session_id in session_ids()? {
        let Ok(Some(session)) = ContractSession::load(&session_id) else {
            continue;
        };
        if session.identity().1 != Some(id) || !session.resumable() {
            continue;
        }
        for source in sources {
            crate::budget::manage::regenerate(&session, source, &format!("{id}:{}", to.name()))?;
        }
    }
    Ok(())
}

/** Read the records for crane task status
 * Input
    - id: Option<&str> - task id, every task when None
 * Output
    - Result<Vec<Value>, String>
*/
pub(crate) fn status(id: Option<&str>) -> Result<Vec<Value>, String> {
    match id {
        Some(id) => Ok(vec![
            TaskRecord::load(id)?
                .ok_or_else(|| format!("no task '{id}'"))?
                .value,
        ]),
        None => Ok(TaskRecord::all()?
            .into_iter()
            .map(|record| record.value)
            .collect()),
    }
}

/** Read webhook bodies for replay: each file (or stdin when there are none) holds one delivery
 * Input
    - files: &[String] - delivery files
 * Output
    - Result<Vec<(String, Value)>, String> label and body of each delivery
*/
pub(crate) fn read_deliveries(files: &[String]) -> Result<Vec<(String, Value)>, String> {
    if files.is_empty() {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).map_err(io_error)?;
        return Ok(vec![(
            "stdin".into(),
            serde_json::from_str(&input).map_err(|error| format!("stdin: {error}"))?,
        )]);
    }
    files
        .iter()
        .map(|file| {
            let content = fs::read_to_string(Path::new(file))
                .map_err(|error| format!("{file}: {}", io_error(error)))?;
            Ok((
                file.clone(),
                serde_json::from_str(&content).map_err(|error| format!("{file}: {error}"))?,
            ))
        })
        .collect()
}
