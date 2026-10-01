// Provider-neutral agent session management: every agent session (Claude Code, Codex, or a
// generic agent) is a contract session bound to its task, contract, checkpoint, zones, autonomy
// mode, and autonomy budget, with start, resume, cancellation, timeout, and finalization handled
// the same way for every provider. The model stays external; Crane only decides what its tool
// calls may do and records what happened.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use serde_json::{json, Value};

use crate::adapter::AgentKind;
use crate::authority::{AgentAction, Decision, Verdict, BUDGET_EXHAUSTED};
use crate::commands::render_context;
use crate::proposals::store::require_human;
use crate::repository::root;
use crate::session::{session_ids, ContractSession, Lifecycle};
use crate::zones::model::{Autonomy, Criticality, SafetyState};

/** Session lifetime when none is given: authority never outlives a working day */
pub(crate) const DEFAULT_TTL: u64 = 8 * 60 * 60;

/** Idle time after which a session's authority lapses until a human or the agent host resumes it */
pub(crate) const DEFAULT_IDLE_TIMEOUT: u64 = 30 * 60;

/** The zone constraints on one file, frozen when the session was created
 * Fields
    - zones: Vec<String> - zones covering it
    - criticality: Criticality - highest criticality
    - autonomy: Autonomy - lowest effective autonomy
    - state: SafetyState - worst effective state
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileConstraint {
    pub(crate) zones: Vec<String>,
    pub(crate) criticality: Criticality,
    pub(crate) autonomy: Autonomy,
    pub(crate) state: SafetyState,
}

/** Everything a session is governed by besides its contract, fixed at creation and covered by the
 * binding digest
 * Fields
    - autonomy: Autonomy - the session's autonomy mode
    - max_actions: u64 - autonomy budget: mutating tool calls the agent may make
    - max_files: u64 - autonomy budget: distinct files the agent may write
    - idle_timeout: u64 - seconds without activity after which authority lapses
    - zone_set_version: Option<String> - version of the zones the snapshot was taken from
    - zones: Vec<Value> - summary of each zone touching the repository (id, criticality,
      autonomy, state, files)
    - files: BTreeMap<String, FileConstraint> - zone constraints per file path
    - task_title: Option<String> - title of the bound task
    - scope: Option<BTreeSet<String>> - files in the task scope, None without a task
    - scope_modules: Vec<String> - modules in the task scope
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Governance {
    pub(crate) autonomy: Autonomy,
    pub(crate) max_actions: u64,
    pub(crate) max_files: u64,
    pub(crate) idle_timeout: u64,
    pub(crate) zone_set_version: Option<String>,
    pub(crate) zones: Vec<Value>,
    pub(crate) files: BTreeMap<String, FileConstraint>,
    pub(crate) task_title: Option<String>,
    pub(crate) scope: Option<BTreeSet<String>>,
    pub(crate) scope_modules: Vec<String>,
}

impl Governance {
    /** Return the default budget of an autonomy mode
     * Input
        - autonomy: Autonomy - mode
     * Output
        - (u64, u64) mutating actions and distinct files
    */
    pub(crate) fn default_budget(autonomy: Autonomy) -> (u64, u64) {
        match autonomy {
            Autonomy::Observe => (0, 0),
            Autonomy::Assisted => (100, 50),
            Autonomy::Delegated => (300, 100),
            Autonomy::Autonomous => (1000, 500),
        }
    }

    /** Build an ungoverned default: delegated autonomy, its default budget, no zones, no scope
     * Input
        - None
     * Output
        - Governance
    */
    pub(crate) fn plain() -> Self {
        let (max_actions, max_files) = Self::default_budget(Autonomy::Delegated);
        Self {
            autonomy: Autonomy::Delegated,
            max_actions,
            max_files,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            zone_set_version: None,
            zones: Vec::new(),
            files: BTreeMap::new(),
            task_title: None,
            scope: None,
            scope_modules: Vec::new(),
        }
    }

    /** Return the zone constraints on a path: its own, or for a file that did not exist when the
     * session started, the most restrictive of the files in its folder
     * Input
        - path: &str - repository-relative path
     * Output
        - Option<FileConstraint>
    */
    pub(crate) fn constraint(&self, path: &str) -> Option<FileConstraint> {
        if let Some(found) = self.files.get(path) {
            return Some(found.clone());
        }
        let folder = path.rsplit_once('/').map_or("", |(folder, _)| folder);
        self.files
            .iter()
            .filter(|(other, _)| other.rsplit_once('/').map_or("", |(other, _)| other) == folder)
            .map(|(_, constraint)| constraint.clone())
            .reduce(|left, right| FileConstraint {
                zones: left
                    .zones
                    .into_iter()
                    .chain(right.zones)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                criticality: left.criticality.max(right.criticality),
                autonomy: left.autonomy.min(right.autonomy),
                state: left.state.max(right.state),
            })
    }

    /** Check whether a path is in the task scope: one of its files, or a file created during the
     * session in one of their folders; always true without a task
     * Input
        - path: &str - repository-relative path
        - exists: bool - whether the file exists now (existing files outside the scope never
          join it by folder)
     * Output
        - bool
    */
    pub(crate) fn in_scope(&self, path: &str, exists: bool) -> bool {
        let Some(scope) = &self.scope else {
            return true;
        };
        if scope.contains(path) {
            return true;
        }
        let folder = |path: &str| {
            path.rsplit_once('/')
                .map_or(String::new(), |(folder, _)| folder.to_string())
        };
        !exists && scope.iter().any(|file| folder(file) == folder(path))
    }

    /** Serialize for the session binding
     * Input
        - None (uses self)
     * Output
        - Value JSON object
    */
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "autonomy": self.autonomy.name(),
            "budget": {"mutating_actions": self.max_actions, "files": self.max_files},
            "idle_timeout": self.idle_timeout,
            "zone_set_version": self.zone_set_version,
            "zones": self.zones,
            "files": self.files.iter().map(|(path, constraint)| (path.clone(), json!({
                "zones": constraint.zones,
                "criticality": constraint.criticality.name(),
                "autonomy": constraint.autonomy.name(),
                "state": constraint.state.name(),
            }))).collect::<serde_json::Map<_, _>>(),
            "task_title": self.task_title,
            "scope": self.scope,
            "scope_modules": self.scope_modules,
        })
    }

    /** Read back from a session binding
     * Input
        - value: &Value - JSON written by to_json
     * Output
        - Result<Governance, String>
        - Error naming the invalid field
    */
    pub(crate) fn from_json(value: &Value) -> Result<Self, String> {
        let number = |value: &Value, key: &str| {
            value
                .as_u64()
                .ok_or_else(|| format!("governance '{key}' must be a number"))
        };
        let strings = |value: &Value| -> Vec<String> {
            value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut files = BTreeMap::new();
        for (path, constraint) in value["files"]
            .as_object()
            .ok_or("governance 'files' must be an object")?
        {
            files.insert(
                path.clone(),
                FileConstraint {
                    zones: strings(&constraint["zones"]),
                    criticality: Criticality::parse(
                        constraint["criticality"].as_str().unwrap_or_default(),
                    )?,
                    autonomy: Autonomy::parse(constraint["autonomy"].as_str().unwrap_or_default())?,
                    state: SafetyState::parse(constraint["state"].as_str().unwrap_or_default())?,
                },
            );
        }
        Ok(Self {
            autonomy: Autonomy::parse(value["autonomy"].as_str().unwrap_or_default())?,
            max_actions: number(&value["budget"]["mutating_actions"], "budget")?,
            max_files: number(&value["budget"]["files"], "budget")?,
            idle_timeout: number(&value["idle_timeout"], "idle_timeout")?,
            zone_set_version: value["zone_set_version"].as_str().map(String::from),
            zones: value["zones"].as_array().cloned().unwrap_or_default(),
            files,
            task_title: value["task_title"].as_str().map(String::from),
            scope: value["scope"]
                .as_array()
                .map(|_| strings(&value["scope"]).into_iter().collect()),
            scope_modules: strings(&value["scope_modules"]),
        })
    }
}

/** What a new session is started with; unset values take their defaults
 * Fields
    - ttl: Option<u64> - seconds until the session expires (default 8 hours)
    - idle_timeout: Option<u64> - seconds of inactivity before authority lapses (default 30 min)
    - task: Option<String> - task id to bind
    - autonomy: Option<Autonomy> - autonomy mode (default delegated)
    - max_actions: Option<u64> - mutating-action budget (default by mode)
    - max_files: Option<u64> - file budget (default by mode)
    - checkpoint: String - checkpoint used to plan the task scope
    - isolate: bool - work in a dedicated Git worktree instead of the repository itself
*/
#[derive(Default)]
pub(crate) struct SessionOptions {
    pub(crate) ttl: Option<u64>,
    pub(crate) idle_timeout: Option<u64>,
    pub(crate) task: Option<String>,
    pub(crate) autonomy: Option<Autonomy>,
    pub(crate) max_actions: Option<u64>,
    pub(crate) max_files: Option<u64>,
    pub(crate) checkpoint: String,
    pub(crate) isolate: bool,
}

impl SessionOptions {
    /** Read session options from command-line arguments (--ttl, --idle-timeout, --task or
     * CRANE_TASK_ID, --autonomy, --max-actions, --max-files, --checkpoint)
     * Input
        - args: &[String] - arguments
     * Output
        - Result<SessionOptions, String>
        - Error for a value that is not a number or a mode
    */
    pub(crate) fn from_args(args: &[String]) -> Result<Self, String> {
        let number = |key: &str| {
            crate::util::option(args, key)
                .map(|value| {
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("{key} must be a number, not '{value}'"))
                })
                .transpose()
        };
        Ok(Self {
            ttl: number("--ttl")?,
            idle_timeout: number("--idle-timeout")?,
            task: crate::util::option(args, "--task").or_else(|| {
                std::env::var("CRANE_TASK_ID")
                    .ok()
                    .filter(|value| !value.is_empty())
            }),
            autonomy: crate::util::option(args, "--autonomy")
                .map(|value| Autonomy::parse(&value))
                .transpose()?,
            max_actions: number("--max-actions")?,
            max_files: number("--max-files")?,
            checkpoint: crate::util::option(args, "--checkpoint")
                .unwrap_or_else(|| "baseline".into()),
            isolate: args.iter().any(|argument| argument == "--isolate"),
        })
    }

    /** Compute the governance of a new session: the autonomy mode and budget, and (only when
     * zones are defined or a task is bound, since it needs repository discovery) the zone
     * constraints per file and the task scope from the task's plan
     * Input
        - task: Option<&str> - validated task id
     * Output
        - Result<Governance, String>
        - Error if zones or the task cannot be resolved
    */
    pub(crate) fn governance(&self, task: Option<&str>) -> Result<Governance, String> {
        let autonomy = self.autonomy.unwrap_or(Autonomy::Delegated);
        let (actions, files) = Governance::default_budget(autonomy);
        let mut governance = Governance {
            autonomy,
            max_actions: self.max_actions.unwrap_or(actions),
            max_files: self.max_files.unwrap_or(files),
            idle_timeout: self.idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT),
            ..Governance::plain()
        };
        let zoned = fs::read_dir(root()?.join("zones"))
            .map(|entries| {
                entries.filter_map(|entry| entry.ok()).any(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "zone")
                })
            })
            .unwrap_or(false);
        if !zoned && task.is_none() {
            return Ok(governance);
        }
        let zones = crate::zones::inspect()?;
        let snapshot = &zones.inventory.snapshot;
        governance.zone_set_version = zoned.then(|| zones.version.clone());
        for result in &zones.resolution.zones {
            governance.zones.push(json!({
                "zone_id": result.zone.zone_id,
                "criticality": result.zone.criticality.name(),
                "autonomy": result.autonomy.name(),
                "state": result.state.name(),
                "files": result.files.len(),
            }));
        }
        for (file, effective) in &zones.resolution.files {
            governance.files.insert(
                snapshot.files[*file].path.clone(),
                FileConstraint {
                    zones: effective
                        .zones
                        .iter()
                        .map(|index| zones.resolution.zones[*index].zone.zone_id.clone())
                        .collect(),
                    criticality: effective.criticality,
                    autonomy: effective.autonomy,
                    state: effective.state,
                },
            );
        }
        if let Some(task) = task {
            let mut scope = BTreeSet::new();
            if let Ok((input, digest)) = crate::tasks::load(task) {
                governance.task_title = Some(input.title.clone());
                if let Ok(plan) = crate::tasks::plan(&input, &digest, &self.checkpoint) {
                    let modules = plan["contract"]["TASK_SCOPE"]["modules"]
                        .as_array()
                        .map(|modules| {
                            modules
                                .iter()
                                .filter_map(|module| module.as_str().map(String::from))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let graph = &zones.inventory.graph;
                    for (index, info) in graph.files.iter().enumerate() {
                        if info.module != usize::MAX
                            && modules.contains(&graph.modules[info.module].id)
                        {
                            scope.insert(snapshot.files[index].path.clone());
                        }
                    }
                    governance.scope_modules = modules;
                }
            }
            // A task that cannot be planned has an empty scope: every write needs approval
            governance.scope = Some(scope);
        }
        Ok(governance)
    }
}

/** Load or create the contract session of a provider session with these options, through the one
 * code path every provider uses
 * Input
    - agent: AgentKind - provider profile
    - provider_session: &str - provider's session id
    - options: &SessionOptions - options, used only when the session is created
 * Output
    - Result<(ContractSession, bool), String> the session and whether it was just created
*/
pub(crate) fn establish(
    agent: AgentKind,
    provider_session: &str,
    options: &SessionOptions,
) -> Result<(ContractSession, bool), String> {
    let (session, created) = ContractSession::establish(
        agent,
        provider_session,
        Some(options.ttl.unwrap_or(DEFAULT_TTL)),
        options.task.as_deref(),
        &|task| options.governance(task),
    )?;
    if created {
        // The starting point every later effect is measured against
        crate::effects::baseline(&session)?;
    }
    Ok((session, created))
}

/** Create (or reuse) the isolated worktree of a session: a Git worktree in
 * .crane/runtime/worktrees/NAME on its own branch crane/NAME, started from the current HEAD, with
 * the agent hosts' local hook settings copied in so hooks run there too
 * Input
    - name: &str - Crane session id
 * Output
    - Result<std::path::PathBuf, String> the worktree root
*/
fn workspace(name: &str) -> Result<std::path::PathBuf, String> {
    let crane = root()?;
    let repository = crane
        .parent()
        .ok_or("invalid .crane location")?
        .to_path_buf();
    let path = crane.join("runtime").join("worktrees").join(name);
    if !path.exists() {
        fs::create_dir_all(path.parent().ok_or("invalid worktree path")?)
            .map_err(|error| error.to_string())?;
        let branch = format!("crane/{name}");
        let target = path.to_string_lossy().into_owned();
        let output = std::process::Command::new("git")
            .args(["worktree", "add", "-b", &branch, &target, "HEAD"])
            .current_dir(&repository)
            .output()
            .map_err(|error| format!("failed to execute git: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "cannot create the session worktree: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        for settings in [
            ".claude/settings.local.json",
            ".claude/settings.json",
            ".codex/hooks.json",
            ".codex/config.toml",
        ] {
            let source = repository.join(settings);
            let destination = path.join(settings);
            if source.is_file() && !destination.exists() {
                fs::create_dir_all(destination.parent().ok_or("invalid settings path")?)
                    .map_err(|error| error.to_string())?;
                fs::copy(&source, &destination).map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(path)
}

/** Load or create a session the way an orchestrator does: in an isolated worktree when the
 * options ask for one (the session is then bound to that worktree), otherwise in the repository
 * Input
    - agent: AgentKind - provider profile
    - provider_session: &str - provider session id
    - options: &SessionOptions - options
 * Output
    - Result<(ContractSession, bool), String> the session and whether it was just created
*/
pub(crate) fn open(
    agent: AgentKind,
    provider_session: &str,
    options: &SessionOptions,
) -> Result<(ContractSession, bool), String> {
    if !options.isolate {
        return establish(agent, provider_session, options);
    }
    let name = format!("{}-{}", agent.name(), provider_session);
    let path = workspace(&name)?;
    crate::effects::within(&path, || establish(agent, provider_session, options))
}

/** Remove the worktree of an ended session (cancelled or finalized), keeping its branch for review
 * Input
    - id: &str - Crane session id
 * Output
    - Result<String, String> the kept branch
    - Error if the session is still active or closed, or was not isolated
*/
pub(crate) fn cleanup(id: &str) -> Result<String, String> {
    require_human("clean up session worktrees")?;
    let session = stored(id)?;
    if session.resumable() {
        return Err(format!(
            "contract session {id} is {}; cancel or finalize it first",
            session.lifecycle().name()
        ));
    }
    let crane = root()?;
    let worktrees = crane.join("runtime").join("worktrees");
    let path = session.root_path().clone();
    if !path.starts_with(&worktrees)
        && !path
            .to_string_lossy()
            .replace('\\', "/")
            .contains("/.crane/runtime/worktrees/")
    {
        return Err(format!(
            "contract session {id} did not run in an isolated worktree"
        ));
    }
    let repository = crane.parent().ok_or("invalid .crane location")?;
    let output = std::process::Command::new("git")
        .args(["worktree", "remove", "--force", &path.to_string_lossy()])
        .current_dir(repository)
        .output()
        .map_err(|error| format!("failed to execute git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot remove the worktree: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    session.record(json!({"event": "worktree_removed", "path": path.to_string_lossy()}))?;
    Ok(format!("crane/{id}"))
}

/** Build the session for a provider that sends no session id
 * Input
    - agent: AgentKind - provider profile
    - options: &SessionOptions - options
 * Output
    - Result<ContractSession, String>
*/
pub(crate) fn transient(
    agent: AgentKind,
    options: &SessionOptions,
) -> Result<ContractSession, String> {
    ContractSession::transient(agent, options.task.as_deref(), &|task| {
        options.governance(task)
    })
}

/** Start or resume a session from the agent host's session-start event: a closed session that can
 * still be resumed becomes active again, the start is journaled with the model and source the
 * host reported, and the model gets the concise context
 * Input
    - session: &ContractSession - session
    - model: Option<&str> - model the host reported
    - source: Option<&str> - startup, resume, clear, or compact as the host reported
 * Output
    - Result<String, String> the context for the model
*/
pub(crate) fn on_start(
    session: &ContractSession,
    model: Option<&str>,
    source: Option<&str>,
) -> Result<String, String> {
    let resumable = session.lifecycle() == Lifecycle::Closed && session.expired().is_none();
    if resumable {
        session.set_lifecycle(Lifecycle::Active)?;
    }
    session.record(
        json!({"event": "session_start", "model": model, "source": source, "resumed": resumable}),
    )?;
    Ok(context(session))
}

/** Render the concise context the model receives at session start: the contract listing, then
 * one short block with the task, autonomy mode and budget, timeouts, and zones; runtime authority
 * stays outside the model, so this only informs
 * Input
    - session: &ContractSession - session
 * Output
    - String
*/
pub(crate) fn context(session: &ContractSession) -> String {
    let mut out = render_context(session.contracts());
    let governance = session.governance();
    let activity = session.activity();
    out.push_str("\nSESSION (enforced by Crane outside the model; denied actions say why)\n");
    let describe = session.describe();
    out.push_str(&format!(
        "session: {}\n",
        describe["session_id"].as_str().unwrap_or_default()
    ));
    if let Some(task) = describe["task_id"].as_str() {
        out.push_str(&format!(
            "task: {task}{}\n",
            governance
                .task_title
                .as_ref()
                .map_or(String::new(), |title| format!(" - {title}"))
        ));
    }
    let mode = match governance.autonomy {
        Autonomy::Observe => "observe (read-only)",
        Autonomy::Assisted => "assisted (every change needs human approval)",
        Autonomy::Delegated if governance.scope.is_some() => {
            "delegated (changes outside the task scope need approval)"
        }
        Autonomy::Delegated => "delegated",
        Autonomy::Autonomous => "autonomous",
    };
    out.push_str(&format!(
        "autonomy: {mode}; budget: {} of {} mutating actions and {} of {} files left; safety: {}\n",
        activity.max_actions.saturating_sub(activity.actions),
        activity.max_actions,
        activity
            .max_files
            .saturating_sub(activity.files.len() as u64),
        activity.max_files,
        activity.safety.name()
    ));
    if !governance.scope_modules.is_empty() {
        out.push_str(&format!(
            "task scope: {}\n",
            governance.scope_modules.join(", ")
        ));
    }
    out.push_str(&format!(
        "expires at {} (authority lapses after {} s idle)\n",
        describe["expires_at"], governance.idle_timeout
    ));
    let zones = governance
        .zones
        .iter()
        .map(|zone| {
            format!(
                "{} ({}, {})",
                zone["zone_id"].as_str().unwrap_or_default(),
                zone["criticality"].as_str().unwrap_or_default(),
                zone["autonomy"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    if !zones.is_empty() {
        out.push_str(&format!("zones: {}\n", zones.join(", ")));
    }
    if !session.resumable() {
        out.push_str("This session has ended; every mutating tool call will be denied.\n");
    }
    out
}

/** Authorize one tool call through the common policy engine: first let an idle session's
 * authority lapse (journaled), then decide with the session's contract, zones, autonomy, budget,
 * and safety state, quarantine the session when its budget is exhausted, and journal the decision
 * Input
    - session: &ContractSession - session
    - protected: &[&str] - provider-specific protected settings files
    - action: &AgentAction - normalized tool call
    - event: &str - journal event name (pre_tool_use or permission_request)
 * Output
    - Result<Verdict, String>
*/
pub(crate) fn authorize(
    session: &ContractSession,
    protected: &[&str],
    action: &AgentAction,
    event: &str,
) -> Result<Verdict, String> {
    session.lapse_if_idle()?;
    let verdict = session.runtime(protected).decide(action);
    let exhausted = verdict.decision == Decision::Deny
        && verdict
            .reasons
            .iter()
            .any(|reason| reason.starts_with(BUDGET_EXHAUSTED));
    if exhausted && session.activity().safety != SafetyState::Quarantined {
        session.record(json!({"event": "safety", "state": "quarantined", "reason": "autonomy budget exhausted"}))?;
    }
    session.record(json!({
        "event": event,
        "tool": action.tool,
        "operation": action.operation.name(),
        "resources": verdict.resources,
        "decision": verdict.decision.name(),
        "reasons": verdict.reasons,
        "arguments_digest": action.digest,
        "result": match verdict.decision {
            Decision::Allow => "authorized",
            Decision::ApprovalRequired => "approval_required",
            Decision::Deny => "blocked",
        },
    }))?;
    Ok(verdict)
}

/** Note what an executed tool call means for the session's safety: when the contract on disk no
 * longer matches the bound one, the session becomes degraded (autonomy capped at assisted)
 * Input
    - session: &ContractSession - session
 * Output
    - Result<(), String>
*/
pub(crate) fn after_tool(session: &ContractSession) -> Result<(), String> {
    let drift = session.describe()["drift"].as_array().map_or(0, Vec::len);
    if drift > 0 && session.activity().safety == SafetyState::Active {
        session.record(json!({"event": "safety", "state": "degraded", "reason": "the contract changed on disk during the session"}))?;
    }
    Ok(())
}

/** Load a stored session for a management command
 * Input
    - id: &str - Crane session id
 * Output
    - Result<ContractSession, String>
*/
fn stored(id: &str) -> Result<ContractSession, String> {
    ContractSession::load(id)?.ok_or_else(|| format!("no contract session '{id}'"))
}

/** Start a session on behalf of an orchestrator or a human (the agent host then uses the same
 * provider session id): create it with the options and record the start
 * Input
    - agent: AgentKind - provider profile
    - provider_session: &str - provider session id the agent will use
    - options: &SessionOptions - options
 * Output
    - Result<(ContractSession, String), String> the session and its context
*/
pub(crate) fn start(
    agent: AgentKind,
    provider_session: &str,
    options: &SessionOptions,
) -> Result<(ContractSession, String), String> {
    require_human("start sessions")?;
    let (session, created) = open(agent, provider_session, options)?;
    if created {
        session.record(
            json!({"event": "session_start", "source": "manager", "model": null, "resumed": false}),
        )?;
    }
    let context = context(&session);
    Ok((session, context))
}

/** Resume a session as a human: reactivate a closed session, restore lapsed authority, and lift a
 * quarantine; cancelled, finalized, and expired sessions cannot be resumed
 * Input
    - id: &str - Crane session id
 * Output
    - Result<(), String>
*/
pub(crate) fn resume(id: &str) -> Result<(), String> {
    require_human("resume sessions")?;
    let session = stored(id)?;
    if !session.resumable() {
        return Err(format!(
            "contract session {id} is {}; it cannot be resumed",
            session.lifecycle().name()
        ));
    }
    if let Some(expired) = session.expired() {
        return Err(expired);
    }
    session.set_lifecycle(Lifecycle::Active)?;
    session.record(json!({"event": "session_resumed", "by": "human"}))?;
    if session.activity().safety == SafetyState::Quarantined {
        session.record(
            json!({"event": "safety", "state": "active", "reason": "resumed by a human"}),
        )?;
    }
    Ok(())
}

/** Cancel a session: it can never act or be resumed again; the worktree is not touched
 * Input
    - id: &str - Crane session id
    - reason: &str - why
 * Output
    - Result<bool, String> whether the session was still open
*/
pub(crate) fn cancel(id: &str, reason: &str) -> Result<bool, String> {
    let session = stored(id)?;
    if !session.resumable() {
        return Ok(false);
    }
    session.record(json!({"event": "session_cancelled", "reason": reason}))?;
    session.set_lifecycle(Lifecycle::Cancelled)?;
    Ok(true)
}

/** Finalize a session: reconcile it one last time, write the final attestation, and end it for
 * good
 * Input
    - id: &str - Crane session id
 * Output
    - Result<Value, String> the final attestation
*/
pub(crate) fn finalize(id: &str) -> Result<Value, String> {
    let session = stored(id)?;
    if session.lifecycle() == Lifecycle::Finalized {
        return crate::session::read_attestation(id)?
            .ok_or_else(|| format!("contract session {id} has no attestation"));
    }
    if session.lifecycle() == Lifecycle::Active {
        session.set_lifecycle(Lifecycle::Closed)?;
    }
    session.record(json!({"event": "session_finalizing"}))?;
    let (_, attestation) = crate::effects::validate(&session)?;
    session.write_attestation(&attestation)?;
    session.record(
        json!({"event": "session_finalized", "final_status": attestation["final_status"]}),
    )?;
    session.set_lifecycle(Lifecycle::Finalized)?;
    Ok(attestation)
}

/** Time out sessions: every active session that expired or sat idle past its idle timeout is
 * closed (and its lapse journaled), so interrupted agents never keep authority
 * Input
    - None
 * Output
    - Result<Vec<String>, String> sessions timed out
*/
pub(crate) fn sweep() -> Result<Vec<String>, String> {
    let mut timed_out = Vec::new();
    for id in session_ids()? {
        let Ok(Some(session)) = ContractSession::load(&id) else {
            continue;
        };
        if session.lifecycle() != Lifecycle::Active {
            continue;
        }
        let reason = session
            .expired()
            .or_else(|| session.lapse_if_idle().ok().flatten());
        if let Some(reason) = reason {
            session.record(json!({"event": "session_timed_out", "reason": reason}))?;
            session.set_lifecycle(Lifecycle::Closed)?;
            timed_out.push(id);
        }
    }
    Ok(timed_out)
}

/** Quarantine a session: every mutating tool call is denied until a human resumes it
 * Input
    - id: &str - Crane session id
    - reason: &str - why
 * Output
    - Result<(), String>
*/
pub(crate) fn quarantine(id: &str, reason: &str) -> Result<(), String> {
    let session = stored(id)?;
    session.record(json!({"event": "safety", "state": "quarantined", "reason": reason}))
}

/** Extend a session's autonomy budget as a human (journaled; the bound budget never changes)
 * Input
    - id: &str - Crane session id
    - actions: u64 - extra mutating actions
    - files: u64 - extra files
 * Output
    - Result<(), String>
*/
pub(crate) fn extend(id: &str, actions: u64, files: u64) -> Result<(), String> {
    require_human("extend session budgets")?;
    let session = stored(id)?;
    if !session.resumable() {
        return Err(format!(
            "contract session {id} is {}",
            session.lifecycle().name()
        ));
    }
    session.record(json!({"event": "budget_extended", "mutating_actions": actions, "files": files}))
}
