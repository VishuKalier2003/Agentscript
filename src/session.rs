use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::adapter::AgentKind;
use crate::agent_session::Governance;
use crate::authority::{derive_grants, grant_from_json, grant_to_json, Grant, Runtime, Usage};
use crate::autonomy::{step, Actor, State, Trigger};
use crate::ir::{compile, ContractSet};
use crate::model::Violation;
use crate::repository::{git, root};
use crate::scope::ScopeContext;
use crate::util::{io_error, now_unix, sha256};
use crate::verify::{repair_owner, verify_contracts, Report};
use crate::zones::model::SafetyState;

/** Version of the session, journal, and attestation file layout (2 added the binding digest,
 * repository and task identity, and the binding stamp on journal events; 3 added governance:
 * autonomy mode and budget, idle timeout, zone constraints, and task scope)
*/
const SESSION_FORMAT: u64 = 3;

/** Journal events that show a human or the agent host is present, restoring lapsed authority */
const REAUTHORIZING: &[&str] = &["session_start", "user_prompt_submit", "session_resumed"];

/** Repository identity recorded for a repository without commits, which has nothing to bind to */
const UNBORN_REPOSITORY: &str = "unborn";

/** Lifecycle state of a contract session, the only part of a session that changes after creation
 * Variants
    - Active - the agent is working under the session's contract
    - Closed - the agent session ended or timed out; a resumed agent session reactivates it with
      the same binding
    - Cancelled - the session was cancelled (for example its task was); it never acts again
    - Finalized - the session was reconciled for the last time; it never acts again
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lifecycle {
    Active,
    Closed,
    Cancelled,
    Finalized,
}

impl Lifecycle {
    /** Return the state's name, as stored in the state file
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Closed => "closed",
            Self::Cancelled => "cancelled",
            Self::Finalized => "finalized",
        }
    }
}

/** What a session has done so far, derived from its journal (so concurrent hook processes never
 * race on counters)
 * Fields
    - actions: u64 - mutating tool calls authorized (or sent for approval)
    - files: BTreeSet<String> - distinct files those calls wrote
    - max_actions: u64 - action budget including human extensions
    - max_files: u64 - file budget including human extensions
    - safety: SafetyState - current safety state
    - safety_reason: Option<String> - why it is not active
    - state: State - autonomy and safety, replayed through the autonomy state machine
    - started_at: Option<u64> - first start
    - last_at: Option<u64> - last journaled event
    - ended_at: Option<u64> - cancellation or finalization
    - model: Option<String> - model the agent host last reported
    - lapsed: bool - authority lapsed for inactivity and was not restored yet
*/
pub(crate) struct Activity {
    pub(crate) actions: u64,
    pub(crate) files: BTreeSet<String>,
    pub(crate) max_actions: u64,
    pub(crate) max_files: u64,
    pub(crate) safety: SafetyState,
    pub(crate) safety_reason: Option<String>,
    pub(crate) state: State,
    pub(crate) started_at: Option<u64>,
    pub(crate) last_at: Option<u64>,
    pub(crate) ended_at: Option<u64>,
    pub(crate) model: Option<String>,
    pub(crate) lapsed: bool,
}

/** One agent session bound to one immutable contract: the policies, their versions, their
 * contract hashes, and their checkpoint commits are fixed when the session is created, together
 * with the repository, agent, and task, and are used for runtime authority, reconciliation, and
 * final verification alike; every field is private and set only by the constructor, the binding
 * digest over all of them is checked whenever the session is loaded, and nothing an agent does
 * can refresh them (only the lifecycle state changes after creation)
 * Fields
    - id: String - Crane session id, "<agent>-<provider session id>"
    - agent: AgentKind - agent profile
    - provider_session: String - the provider's own session id
    - task: Option<String> - task identity supplied when the session was created, if any
    - created_at: u64 - Unix seconds
    - expires_at: Option<u64> - Unix seconds after which every mutating action is denied
    - root: PathBuf - repository root that action paths are resolved against
    - repository: String - repository identity (digest of the root commits, or "unborn")
    - contracts: ContractSet - the bound contract IR
    - grants: Vec<Grant> - runtime authority derived from the contracts at creation
    - governance: Governance - autonomy mode and budget, idle timeout, zones, and task scope
    - binding: String - SHA-256 over every field above, stamped on journal events and attestations
    - persisted: bool - false for a transient session used when the provider sends no session id
*/
pub(crate) struct ContractSession {
    id: String,
    agent: AgentKind,
    provider_session: String,
    task: Option<String>,
    created_at: u64,
    expires_at: Option<u64>,
    root: PathBuf,
    repository: String,
    contracts: ContractSet,
    grants: Vec<Grant>,
    governance: Governance,
    binding: String,
    persisted: bool,
}

/** Computes a new session's governance from its validated task id, called only on creation */
pub(crate) type GovernanceSource<'a> = &'a dyn Fn(Option<&str>) -> Result<Governance, String>;

/** Everything that identifies a new session before its contract is compiled
 * Fields
    - id: String - Crane session id
    - agent: AgentKind - agent profile
    - provider_session: String - provider's session id, empty for a transient session
    - task: Option<String> - validated task identity
    - ttl: Option<u64> - seconds until the session expires
    - persisted: bool - whether the session is written to disk
    - governance: Governance - autonomy, budget, zones, and scope
*/
struct Identity {
    id: String,
    agent: AgentKind,
    provider_session: String,
    task: Option<String>,
    ttl: Option<u64>,
    persisted: bool,
    governance: Governance,
}

impl ContractSession {
    /** Load the session for a provider session id, or create it on first sight, by first loading
     * any stored session, otherwise moving aside files a failed earlier attempt left behind,
     * compiling the policies on disk, deriving runtime authority from their checkpoints, and
     * writing the session file only if no other hook process created it first; an existing
     * session is never replaced or refreshed, and a task identity that differs from the bound
     * one is rejected
     * Input
        - agent: AgentKind - agent profile
        - provider_session: &str - provider's session id
        - ttl: Option<u64> - seconds until the session expires, only used on creation
        - task: Option<&str> - task identity from the hook configuration, if any
        - governance: GovernanceSource - computes the governance, only on creation
     * Output
        - Result<(ContractSession, bool), String> the session and whether it was just created
        - Error if the id or task is unusable or conflicts with the binding, the policies cannot
          be read, or the files cannot be written
    */
    pub(crate) fn establish(
        agent: AgentKind,
        provider_session: &str,
        ttl: Option<u64>,
        task: Option<&str>,
        governance: GovernanceSource,
    ) -> Result<(Self, bool), String> {
        let id = session_id(agent, provider_session)?;
        let task = task.map(task_id).transpose()?;
        let (session, created) = match Self::load(&id)? {
            Some(session) => (session, false),
            None => {
                recover_partial(&id)?;
                let session = Self::bind(Identity {
                    id: id.clone(),
                    agent,
                    provider_session: provider_session.into(),
                    governance: governance(task.as_deref())?,
                    task: task.clone(),
                    ttl,
                    persisted: true,
                })?;
                if session.persist()? {
                    (session, true)
                } else {
                    let existing = Self::load(&id)?.ok_or("contract session disappeared")?;
                    (existing, false)
                }
            }
        };
        if task.is_some() && task != session.task {
            return Err(format!(
                "contract session {id} is bound to task {}, not {}; a task cannot be replaced, so a new agent session is required",
                session.task.as_deref().unwrap_or("(none)"),
                task.as_deref().unwrap_or_default()
            ));
        }
        Ok((session, created))
    }

    /** Build a session that is never written to disk, for providers that send no session id;
     * it compiles the policies and derives authority exactly like a stored session
     * Input
        - agent: AgentKind - agent profile
        - task: Option<&str> - task identity from the hook configuration, if any
        - governance: GovernanceSource - computes the governance
     * Output
        - Result<ContractSession, String>
        - Error if the task is unusable or the policies or repository cannot be read
    */
    pub(crate) fn transient(
        agent: AgentKind,
        task: Option<&str>,
        governance: GovernanceSource,
    ) -> Result<Self, String> {
        let task = task.map(task_id).transpose()?;
        Self::bind(Identity {
            id: "transient".into(),
            agent,
            provider_session: String::new(),
            governance: governance(task.as_deref())?,
            task,
            ttl: None,
            persisted: false,
        })
    }

    /** Create a session, the only constructor of new sessions, by compiling the policies,
     * deriving the grants from the same compiled contracts, identifying the repository, and
     * finally computing the binding digest over all of it
     * Input
        - identity: Identity - session, agent, task, and lifetime
     * Output
        - Result<ContractSession, String>
        - Error if the policies or the repository cannot be read
    */
    fn bind(identity: Identity) -> Result<Self, String> {
        let contracts = compile()?;
        let grants = derive_grants(&contracts);
        let (root, repository) = repository_identity()?;
        let created_at = now_unix();
        let mut session = Self {
            id: identity.id,
            agent: identity.agent,
            provider_session: identity.provider_session,
            task: identity.task,
            created_at,
            expires_at: identity
                .ttl
                .map(|seconds| created_at.saturating_add(seconds)),
            root,
            repository,
            contracts,
            grants,
            governance: identity.governance,
            binding: String::new(),
            persisted: identity.persisted,
        };
        session.binding = sha256(session.binding_json().to_string().as_bytes());
        Ok(session)
    }

    /** Load a stored session by id, by reading its session.json, rebuilding the contract IR (which
     * checks its own hashes and version) and grants, and then rejecting the file unless it
     * belongs to this id and its recomputed binding digest equals the stored one, so no
     * authority-defining field (agent, task, expiry, root, repository, contract, checkpoint,
     * grants) can be changed after creation
     * Input
        - id: &str - Crane session id
     * Output
        - Result<Option<ContractSession>, String>, None if no such session exists
        - Error if the file is unreadable, partial, of another format, or tampered with
    */
    pub(crate) fn load(id: &str) -> Result<Option<Self>, String> {
        let path = directory(id)?.join("session.json");
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
        };
        let invalid = |detail: String| {
            format!(
                "contract session {id} is invalid: {detail}; mutating tools stay denied until a human removes .crane/runtime/sessions/{id} so a new session can bind the current contract"
            )
        };
        let value: Value =
            serde_json::from_str(&content).map_err(|error| invalid(error.to_string()))?;
        if value.get("session_format").and_then(Value::as_u64) != Some(SESSION_FORMAT) {
            return Err(invalid(format!(
                "unsupported session format; expected {SESSION_FORMAT}"
            )));
        }
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| invalid(format!("missing '{key}'")))
        };
        let number = |key: &str| match value.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(number) => number
                .as_u64()
                .map(Some)
                .ok_or_else(|| invalid(format!("'{key}' must be a number"))),
        };
        let contracts = ContractSet::from_json(value.get("contracts").unwrap_or(&Value::Null))
            .map_err(invalid)?;
        let grants = value
            .get("grants")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing 'grants'".into()))?
            .iter()
            .map(grant_from_json)
            .collect::<Result<Vec<_>, _>>()
            .map_err(invalid)?;
        if grants.len() != contracts.clauses().count() {
            return Err(invalid("grants do not match the contract clauses".into()));
        }
        let task = match value.get("task_id") {
            None | Some(Value::Null) => None,
            Some(task) => Some(
                task.as_str()
                    .ok_or_else(|| invalid("'task_id' must be text".into()))?
                    .to_string(),
            ),
        };
        let governance = Governance::from_json(value.get("governance").unwrap_or(&Value::Null))
            .map_err(invalid)?;
        let session = Self {
            id: text("session_id")?.into(),
            agent: AgentKind::parse(Some(text("agent")?)).map_err(invalid)?,
            provider_session: text("provider_session")?.into(),
            task,
            created_at: number("created_at")?
                .ok_or_else(|| invalid("missing 'created_at'".into()))?,
            expires_at: number("expires_at")?,
            root: PathBuf::from(text("root")?),
            repository: text("repository_id")?.into(),
            contracts,
            grants,
            governance,
            binding: text("binding_digest")?.into(),
            persisted: true,
        };
        if session.id != id
            || session_id(session.agent, &session.provider_session).as_deref() != Ok(id)
        {
            return Err(invalid(
                "the session file belongs to another agent session".into(),
            ));
        }
        if sha256(session.binding_json().to_string().as_bytes()) != session.binding {
            return Err(invalid(
                "binding digest does not match the session contents".into(),
            ));
        }
        Ok(Some(session))
    }

    /** Return the canonical binding of the session (every authority-defining field, without the
     * digest and lifecycle); serde_json sorts object keys, so its text is stable and is what the
     * binding digest hashes
     * Input
        - None (uses self)
     * Output
        - Value JSON object
    */
    fn binding_json(&self) -> Value {
        json!({
            "session_format": SESSION_FORMAT,
            "session_id": self.id,
            "agent": self.agent.name(),
            "provider_session": self.provider_session,
            "task_id": self.task,
            "created_at": self.created_at,
            "expires_at": self.expires_at,
            "root": self.root.to_string_lossy(),
            "repository_id": self.repository,
            "contracts": self.contracts.to_json(),
            "grants": self.grants.iter().map(grant_to_json).collect::<Vec<_>>(),
            "governance": self.governance.to_json(),
        })
    }

    /** Return the repository root the session is bound to (its worktree when isolated)
     * Input
        - None (uses self)
     * Output
        - &PathBuf
    */
    pub(crate) fn root_path(&self) -> &PathBuf {
        &self.root
    }

    /** Return the session's runtime directory, None for a transient session
     * Input
        - None (uses self)
     * Output
        - Option<PathBuf>
    */
    pub(crate) fn directory(&self) -> Option<PathBuf> {
        self.persisted.then(|| directory(&self.id).ok()).flatten()
    }

    /** Return the session's governance (autonomy, budget, timeouts, zones, scope)
     * Input
        - None (uses self)
     * Output
        - &Governance
    */
    pub(crate) fn governance(&self) -> &Governance {
        &self.governance
    }

    /** Check whether the session can still act or be resumed (active or closed)
     * Input
        - None (uses self)
     * Output
        - bool
    */
    pub(crate) fn resumable(&self) -> bool {
        matches!(self.lifecycle(), Lifecycle::Active | Lifecycle::Closed)
    }

    /** Derive what the session has done from its journal: authorized mutating actions and the
     * files they wrote, budget extensions, the autonomy and safety state (by replaying every
     * autonomy event through the state machine under the bound autonomy policy), start, last
     * activity, end, the reported model, and whether authority lapsed after the last
     * reauthorizing event
     * Input
        - None (uses self)
     * Output
        - Activity
    */
    pub(crate) fn activity(&self) -> Activity {
        let mut activity = Activity {
            actions: 0,
            files: BTreeSet::new(),
            max_actions: self.governance.max_actions,
            max_files: self.governance.max_files,
            safety: SafetyState::Active,
            safety_reason: None,
            state: State::initial(self.governance.autonomy),
            started_at: None,
            last_at: None,
            ended_at: None,
            model: None,
            lapsed: false,
        };
        let policy = self.governance.autonomy_policy.clone().unwrap_or_default();
        for event in self.journal().0 {
            let at = event["at"].as_u64();
            activity.last_at = at.or(activity.last_at);
            let kind = event["event"].as_str().unwrap_or_default();
            if REAUTHORIZING.contains(&kind) {
                activity.lapsed = false;
            }
            match kind {
                "session_start" => {
                    activity.started_at = activity.started_at.or(at);
                    if let Some(model) = event["model"].as_str() {
                        activity.model = Some(model.into());
                    }
                }
                "pre_tool_use"
                    if event["operation"] != "read"
                        && matches!(
                            event["decision"].as_str(),
                            Some("allow" | "approval_required")
                        ) =>
                {
                    activity.actions += 1;
                    for resource in event["resources"].as_array().into_iter().flatten() {
                        if let Some(path) = resource
                            .as_str()
                            .and_then(|text| text.strip_prefix("file:"))
                        {
                            if path != "(outside repository)" {
                                activity.files.insert(path.to_string());
                            }
                        }
                    }
                }
                "budget_extended" => {
                    activity.max_actions += event["mutating_actions"].as_u64().unwrap_or(0);
                    activity.max_files += event["files"].as_u64().unwrap_or(0);
                }
                "autonomy" => {
                    let replayed = Trigger::from_json(&event).and_then(|trigger| {
                        let actor = Actor::parse(event["actor"].as_str().unwrap_or_default())?;
                        step(&policy, &activity.state, &trigger, actor)
                    });
                    // A journaled transition that no longer replays is tampering: fail closed
                    activity.state = match replayed {
                        Ok((state, _)) => state,
                        Err(error) => State {
                            safety: SafetyState::Quarantined,
                            reason: Some(format!(
                                "autonomy event {} does not replay: {error}",
                                event["seq"]
                            )),
                            ..activity.state.clone()
                        },
                    };
                }
                // Sessions written before the state machine journaled safety states directly
                "safety" => {
                    activity.state.safety =
                        SafetyState::parse(event["state"].as_str().unwrap_or("active"))
                            .unwrap_or(SafetyState::Quarantined);
                    activity.state.reason = event["reason"]
                        .as_str()
                        .map(String::from)
                        .filter(|_| activity.state.safety != SafetyState::Active);
                }
                "authority_lapsed" => activity.lapsed = true,
                "session_cancelled" | "session_finalized" => activity.ended_at = at,
                _ => {}
            }
        }
        activity.safety = activity.state.safety;
        activity.safety_reason = activity.state.reason.clone();
        activity
    }

    /** Return who and what the session belongs to, for state inherited across sessions
     * Input
        - None (uses self)
     * Output
        - (AgentKind, Option<&str>, u64) agent profile, task, and creation time
    */
    pub(crate) fn identity(&self) -> (AgentKind, Option<&str>, u64) {
        (self.agent, self.task.as_deref(), self.created_at)
    }

    /** Return the session id
     * Input
        - None (uses self)
     * Output
        - &str
    */
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /** Let the session's authority lapse when it sat idle past its idle timeout: journal the lapse
     * once; it holds until a reauthorizing event (session start, user prompt, or a human resume)
     * Input
        - None (uses self)
     * Output
        - Result<Option<String>, String> why authority is lapsed, None while it holds
    */
    pub(crate) fn lapse_if_idle(&self) -> Result<Option<String>, String> {
        if !self.persisted {
            return Ok(None);
        }
        let activity = self.activity();
        let reason = format!(
            "contract session {} lapsed after {} s without activity; the agent host must resume the session, or a human runs 'crane agent session resume {}'",
            self.id, self.governance.idle_timeout, self.id
        );
        if activity.lapsed {
            return Ok(Some(reason));
        }
        let last = activity.last_at.unwrap_or(self.created_at);
        if now_unix().saturating_sub(last) > self.governance.idle_timeout {
            self.record(json!({"event": "authority_lapsed", "idle_seconds": now_unix().saturating_sub(last)}))?;
            return Ok(Some(reason));
        }
        Ok(None)
    }

    /** Return the bound contract set, the only one runtime authority and verification may use
     * Input
        - None (uses self)
     * Output
        - &ContractSet
    */
    pub(crate) fn contracts(&self) -> &ContractSet {
        &self.contracts
    }

    /** Describe the session for humans and CI: its binding (without the grants), the binding
     * digest, the lifecycle state, and the drift between the bound contract and the one on disk
     * Input
        - None (uses self)
     * Output
        - Value JSON object
    */
    pub(crate) fn describe(&self) -> Value {
        let mut value = self.binding_json();
        if let Some(object) = value.as_object_mut() {
            object.remove("grants");
        }
        value["binding_digest"] = json!(self.binding);
        value["lifecycle"] = json!(self.lifecycle().name());
        value["drift"] = json!(self.drift().1);
        if let Some(files) = value["governance"].as_object_mut() {
            let constrained = files
                .remove("files")
                .and_then(|files| files.as_object().map(|files| files.len()))
                .unwrap_or(0);
            files.insert("zoned_files".into(), json!(constrained));
        }
        value["activity"] = self.activity_json();
        value
    }

    /** Summarize the session's activity for descriptions and attestations
     * Input
        - None (uses self)
     * Output
        - Value JSON object
    */
    fn activity_json(&self) -> Value {
        let activity = self.activity();
        json!({
            "started_at": activity.started_at,
            "last_activity_at": activity.last_at,
            "ended_at": activity.ended_at,
            "model": activity.model,
            "autonomy": activity.state.autonomy.name(),
            "initial_autonomy": self.governance.autonomy.name(),
            "safety_state": activity.safety.name(),
            "safety_reason": activity.safety_reason,
            "authority_lapsed": activity.lapsed,
            "budget": {
                "mutating_actions": {"used": activity.actions, "limit": activity.max_actions},
                "files": {"used": activity.files.len(), "limit": activity.max_files},
            },
        })
    }

    /** Write session.json exactly once, by writing a temporary file and hard-linking it into place,
     * which fails instead of overwriting when another process won the race; the runtime directory
     * also gets a .gitignore so sessions are never committed
     * Input
        - None (uses self)
     * Output
        - Result<bool, String>, false if the session already existed
        - Error if the files cannot be written
    */
    fn persist(&self) -> Result<bool, String> {
        let runtime = root()?.join("runtime");
        let directory = directory(&self.id)?;
        fs::create_dir_all(&directory).map_err(io_error)?;
        let ignore = runtime.join(".gitignore");
        if !ignore.exists() {
            fs::write(ignore, "*\n").map_err(io_error)?;
        }
        let mut document = self.binding_json();
        document["binding_digest"] = json!(self.binding);
        let temporary = directory.join(format!("session.json.{}", std::process::id()));
        fs::write(
            &temporary,
            serde_json::to_string_pretty(&document).map_err(io_error)?,
        )
        .map_err(io_error)?;
        let linked = fs::hard_link(&temporary, directory.join("session.json"));
        let _ = fs::remove_file(&temporary);
        match linked {
            Ok(()) => {
                self.set_lifecycle(Lifecycle::Active)?;
                Ok(true)
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(io_error(error)),
        }
    }

    /** Build the runtime authority for this session from exactly the contracts and grants it was
     * bound to, treating an expired, closed, or displaced session (used in another repository)
     * as unusable
     * Input
        - protected: &[&str] - provider-specific protected settings files
     * Output
        - Runtime borrowing this session's contracts and grants
    */
    pub(crate) fn runtime<'a>(&'a self, protected: &'a [&'a str]) -> Runtime<'a> {
        let activity = self.activity();
        let lifecycle = match self.lifecycle() {
            Lifecycle::Active => None,
            Lifecycle::Closed => Some(format!(
                "contract session {} is closed; a new agent session is required, or the agent host resumes this one",
                self.id
            )),
            Lifecycle::Cancelled => Some(format!(
                "contract session {} was cancelled; it can never act again",
                self.id
            )),
            Lifecycle::Finalized => Some(format!(
                "contract session {} was finalized; start a new agent session",
                self.id
            )),
        };
        let lapsed = activity.lapsed.then(|| {
            format!(
                "contract session {} lapsed for inactivity; the agent host must resume the session, or a human runs 'crane agent session resume {}'",
                self.id, self.id
            )
        });
        Runtime {
            contracts: &self.contracts,
            grants: &self.grants,
            root: self.root.clone(),
            protected,
            problem: self
                .expired()
                .or(lifecycle)
                .or(lapsed)
                .or_else(|| self.displaced()),
            governance: Some(&self.governance),
            usage: Usage {
                actions: activity.actions,
                files: activity.files,
                max_actions: activity.max_actions,
                max_files: activity.max_files,
            },
            safety: activity.safety,
            safety_reason: activity.safety_reason,
            autonomy: activity.state.autonomy,
        }
    }

    /** Report whether the session is being used outside the repository it was bound to, by
     * comparing the current repository root and identity with the bound ones; a repository that
     * had no commits when the session was created has no identity to compare; transient
     * sessions are built in place and are never displaced
     * Input
        - None (uses self)
     * Output
        - Option<String> reason when displaced, None otherwise
    */
    fn displaced(&self) -> Option<String> {
        if !self.persisted {
            return None;
        }
        match repository_identity() {
            Err(error) => Some(format!(
                "contract session {} cannot identify its repository: {error}",
                self.id
            )),
            Ok((root, repository)) => {
                let moved = root != self.root;
                let replaced =
                    self.repository != UNBORN_REPOSITORY && repository != self.repository;
                (moved || replaced).then(|| {
                    format!(
                        "contract session {} is bound to repository {} at {}, not to this one; a new agent session is required",
                        self.id,
                        self.repository,
                        self.root.display()
                    )
                })
            }
        }
    }

    /** Compare the bound contract with the one on disk now
     * Input
        - None (uses self)
     * Output
        - (Result<String, String>, Vec<String>) the version on disk (or why it cannot be compiled)
          and one line per difference, empty when nothing drifted
    */
    fn drift(&self) -> (Result<String, String>, Vec<String>) {
        match compile() {
            Ok(disk) => {
                let lines = self.contracts.drift(&disk);
                (Ok(disk.version), lines)
            }
            Err(error) => (
                Err(error.clone()),
                vec![format!("the policies on disk cannot be compiled: {error}")],
            ),
        }
    }

    /** Report whether the session has expired
     * Input
        - None (uses self)
     * Output
        - Option<String> reason when expired, None otherwise
    */
    pub(crate) fn expired(&self) -> Option<String> {
        self.expires_at
            .filter(|expires_at| now_unix() >= *expires_at)
            .map(|expires_at| {
                format!(
                    "contract session {} expired at {expires_at}; a human must start a new agent session",
                    self.id
                )
            })
    }

    /** Read the lifecycle state, treating a missing state file as active
     * Input
        - None (uses self)
     * Output
        - Lifecycle
    */
    pub(crate) fn lifecycle(&self) -> Lifecycle {
        let state = directory(&self.id)
            .and_then(|directory| fs::read_to_string(directory.join("state")).map_err(io_error));
        match state.as_deref().map(str::trim) {
            Ok("closed") => Lifecycle::Closed,
            Ok("cancelled") => Lifecycle::Cancelled,
            Ok("finalized") => Lifecycle::Finalized,
            _ => Lifecycle::Active,
        }
    }

    /** Change the lifecycle state, the only mutation a session allows after creation
     * Input
        - state: Lifecycle - new state
     * Output
        - Result<(), String>
        - Error if the state file cannot be written
    */
    pub(crate) fn set_lifecycle(&self, state: Lifecycle) -> Result<(), String> {
        if !self.persisted {
            return Ok(());
        }
        fs::write(
            directory(&self.id)?.join("state"),
            format!("{}\n", state.name()),
        )
        .map_err(io_error)
    }

    /** Append an event to the session's journal, by stamping it with a sequence number, the time,
     * and the session binding (session, agent, contract version, checkpoints, binding digest) and
     * writing it as one JSON line; transient sessions keep no journal
     * Input
        - event: Value - JSON object with the event's own fields
     * Output
        - Result<(), String>
        - Error if the journal cannot be written
    */
    pub(crate) fn record(&self, mut event: Value) -> Result<(), String> {
        if !self.persisted {
            return Ok(());
        }
        let path = directory(&self.id)?.join("journal.jsonl");
        let sequence = fs::read_to_string(&path)
            .map(|content| content.lines().count())
            .unwrap_or(0)
            + 1;
        event["seq"] = json!(sequence);
        event["at"] = json!(now_unix());
        event["session_id"] = json!(self.id);
        event["agent"] = json!(self.agent.name());
        event["contract_version"] = json!(self.contracts.version);
        event["checkpoints"] = json!(self.checkpoints());
        event["binding"] = json!(self.binding);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(io_error)?;
        writeln!(file, "{event}").map_err(io_error)
    }

    /** List the bound checkpoints as "name@sha", once each
     * Input
        - None (uses self)
     * Output
        - Vec<String>
    */
    fn checkpoints(&self) -> Vec<String> {
        let mut checkpoints = self
            .contracts
            .contracts
            .iter()
            .map(|contract| {
                format!(
                    "{}@{}",
                    contract.checkpoint,
                    contract.checkpoint_sha.as_deref().unwrap_or("unavailable")
                )
            })
            .collect::<Vec<_>>();
        checkpoints.sort();
        checkpoints.dedup();
        checkpoints
    }

    /** Return the session's journal events (lines that do not belong to the session are left out)
     * Input
        - None (uses self)
     * Output
        - Vec<Value>
    */
    pub(crate) fn events(&self) -> Vec<Value> {
        self.journal().0
    }

    /** Read the journal, by parsing every line and reporting (not skipping) any line that is not a
     * JSON object bound to this session, contract version, and binding digest (a line cut short
     * by a crash is reported the same way)
     * Input
        - None (uses self)
     * Output
        - (Vec<Value>, String, Option<String>) events, SHA-256 of the journal bytes, and the first
          problem found
    */
    fn journal(&self) -> (Vec<Value>, String, Option<String>) {
        if !self.persisted {
            return (Vec::new(), sha256(b""), None);
        }
        let bytes =
            match directory(&self.id).map(|directory| fs::read(directory.join("journal.jsonl"))) {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(error)) if error.kind() == ErrorKind::NotFound => Vec::new(),
                Ok(Err(error)) => return (Vec::new(), sha256(b""), Some(io_error(error))),
                Err(error) => return (Vec::new(), sha256(b""), Some(error)),
            };
        let digest = sha256(&bytes);
        let mut events = Vec::new();
        let mut problem = None;
        for (number, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            match serde_json::from_str::<Value>(line) {
                Ok(event)
                    if event["session_id"] == json!(self.id)
                        && event["contract_version"] == json!(self.contracts.version)
                        && event["binding"] == json!(self.binding) =>
                {
                    events.push(event)
                }
                _ => {
                    problem.get_or_insert_with(|| {
                        format!(
                            "journal line {} is not an event of this session and contract",
                            number + 1
                        )
                    });
                }
            }
        }
        (events, digest, problem)
    }

    /** Reconcile the session into one contract outcome, by verifying the bound contract (the same
     * set and checkpoint commits runtime authority used) against the current worktree, failing
     * closed when the session is displaced, when the contract on disk no longer matches the bound
     * one (naming each changed policy and moved checkpoint), or when the journal cannot be
     * trusted, and summarizing the journal's authorized and denied actions into an attestation
     * Input
        - None (uses self)
     * Output
        - (Report, Value) verification report (including session findings) and the attestation
    */
    pub(crate) fn reconcile(&self) -> (Report, Value) {
        let mut report = verify_contracts(&self.contracts, &mut ScopeContext::new(), &|_| true);
        if let Some(problem) = self.displaced() {
            report
                .violations
                .push(session_violation("repository_mismatch", problem));
        }
        let (disk_version, drift) = self.drift();
        if !drift.is_empty() {
            report.violations.push(session_violation(
                "contract_drift",
                format!(
                    "policies or checkpoints changed during contract session {} (bound {}, repository {}): {}; runtime authority and verification keep using the bound contract, so a human must start a new agent session to adopt the change",
                    self.id,
                    self.contracts.version,
                    disk_version.as_deref().unwrap_or("unavailable"),
                    drift.join("; ")
                ),
            ));
        }
        let (events, journal_digest, journal_problem) = self.journal();
        if let Some(problem) = &journal_problem {
            report
                .violations
                .push(session_violation("journal_error", problem.clone()));
        }
        let attestation = self.attestation(&report, &events, &journal_digest, disk_version, &drift);
        (report, attestation)
    }

    /** Build the attestation object, by listing the bound contracts, the journal's allowed and
     * denied pre-execution decisions, each rule's result, other findings, answers to the
     * reconciliation questions, the session binding (repository, task, binding digest, contract
     * hashes, the contract version runtime authority and verification both used), the drift
     * found, and evidence identifiers (journal digest, HEAD, time)
     * Input
        - report: &Report - verification report including session findings
        - events: &[Value] - journal events
        - journal_digest: &str - SHA-256 of the journal bytes
        - disk_version: Result<String, String> - contract version currently on disk
        - drift: &[String] - differences between the bound contract and the one on disk
     * Output
        - Value attestation JSON
    */
    fn attestation(
        &self,
        report: &Report,
        events: &[Value],
        journal_digest: &str,
        disk_version: Result<String, String>,
        drift: &[String],
    ) -> Value {
        let sha_of = |checkpoint: &str| {
            self.contracts
                .contracts
                .iter()
                .find(|contract| contract.checkpoint == checkpoint)
                .and_then(|contract| contract.checkpoint_sha.clone().ok())
        };
        let actions = |decision: &str| {
            events
                .iter()
                .filter(|event| event["event"] == "pre_tool_use" && event["decision"] == decision)
                .map(|event| {
                    json!({
                        "seq": event["seq"],
                        "tool": event["tool"],
                        "operation": event["operation"],
                        "resources": event["resources"],
                        "reasons": event["reasons"],
                    })
                })
                .collect::<Vec<_>>()
        };
        let results = |rule: &str| {
            let passes = report
                .passes
                .iter()
                .filter(|pass| pass.rule == rule)
                .map(|pass| {
                    json!({
                        "policy_id": pass.policy_id,
                        "rule": pass.description,
                        "target": pass.target,
                        "checkpoint": pass.checkpoint,
                        "checkpoint_sha": sha_of(&pass.checkpoint),
                        "status": "pass",
                    })
                });
            let failures = report
                .violations
                .iter()
                .filter(|violation| violation.rule == rule)
                .map(|violation| violation_json(violation, sha_of(&violation.checkpoint)));
            passes.chain(failures).collect::<Vec<_>>()
        };
        let violated = |types: &[&str]| {
            report
                .violations
                .iter()
                .any(|violation| types.contains(&violation.violation_type.as_str()))
        };
        let denied = actions("deny");
        let findings = report
            .violations
            .iter()
            .filter(|violation| violation.rule != "preserve" && violation.rule != "target")
            .map(|violation| violation_json(violation, None))
            .collect::<Vec<_>>();
        let unreconciled = report.violations.iter().any(|violation| {
            !matches!(
                violation.violation_type.as_str(),
                "source_changed" | "target_unchanged" | "change_type_mismatch"
            )
        });
        json!({
            "attestation_format": SESSION_FORMAT,
            "session_id": self.id,
            "agent_id": self.agent.name(),
            "provider_session": self.provider_session,
            "task_id": self.task,
            "repository_id": self.repository,
            "binding_digest": self.binding,
            "session": {
                "lifecycle": self.lifecycle().name(),
                "governance": {
                    "autonomy": self.governance.autonomy.name(),
                    "idle_timeout": self.governance.idle_timeout,
                    "zone_set_version": self.governance.zone_set_version,
                    "zones": self.governance.zones,
                    "task_title": self.governance.task_title,
                    "scope_modules": self.governance.scope_modules,
                },
                "expires_at": self.expires_at,
                "activity": self.activity_json(),
            },
            "contract_version": self.contracts.version,
            "runtime_contract_version": self.contracts.version,
            "verification_contract_version": self.contracts.version,
            "repository_contract_version": disk_version.clone().unwrap_or_else(|error| format!("unavailable: {error}")),
            "contracts": self.contracts.contracts.iter().map(|contract| json!({
                "contract_id": contract.policy_id,
                "contract_hash": contract.hash(),
                "contract_version": contract.version,
                "policy_version": contract.version,
                "checkpoint": contract.checkpoint,
                "checkpoint_sha": contract.checkpoint_sha.clone().ok(),
            })).collect::<Vec<_>>(),
            "drift": drift,
            "authorized_actions": actions("allow"),
            "denied_actions": denied,
            "preserve_results": results("preserve"),
            "target_results": results("target"),
            "findings": findings,
            "reconciliation": {
                "forbidden_mutations_attempted": !denied.is_empty(),
                "required_targets_changed": !violated(&["target_unchanged"]),
                "preserve_invariants_satisfied": !report.violations.iter().any(|violation| violation.rule == "preserve"),
                "change_types_satisfied": !violated(&["change_type_mismatch"]),
                "same_contract_version": disk_version.as_deref() == Ok(self.contracts.version.as_str()),
                "same_repository": !violated(&["repository_mismatch"]),
                "fully_reconciled": !unreconciled,
            },
            "final_status": if report.violations.is_empty() { "PASS" } else { "FAIL" },
            "evidence": {
                "journal_events": events.len(),
                "journal_sha256": journal_digest,
                "head": git(&["rev-parse", "HEAD"]).unwrap_or_else(|_| "unborn".into()),
                "verified_at": now_unix(),
            },
        })
    }

    /** Store the latest attestation next to the session, replacing the previous one
     * Input
        - attestation: &Value - attestation JSON
     * Output
        - Result<(), String>
        - Error if the file cannot be written
    */
    pub(crate) fn write_attestation(&self, attestation: &Value) -> Result<(), String> {
        if !self.persisted {
            return Ok(());
        }
        fs::write(
            directory(&self.id)?.join("attestation.json"),
            serde_json::to_string_pretty(attestation).map_err(io_error)?,
        )
        .map_err(io_error)
    }
}

/** Derive the Crane session id from the provider's id, keeping only characters that are safe in
 * a directory name
 * Input
    - agent: AgentKind - agent profile
    - provider_session: &str - provider's session id
 * Output
    - Result<String, String> such as "claude-3f2a..."
    - Error if nothing usable is left
*/
fn session_id(agent: AgentKind, provider_session: &str) -> Result<String, String> {
    let cleaned = provider_session
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || "-_".contains(*character))
        .take(96)
        .collect::<String>();
    if cleaned.is_empty() {
        return Err(format!("unusable agent session id '{provider_session}'"));
    }
    Ok(format!("{}-{cleaned}", agent.name()))
}

/** Validate a task identity, which must be 1 to 128 ASCII letters, digits, or "-_.:#/"
 * Input
    - value: &str - task identity from the hook configuration
 * Output
    - Result<String, String> the task identity
    - Error if it is empty, too long, or has other characters
*/
fn task_id(value: &str) -> Result<String, String> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.:#/".contains(character));
    if valid {
        Ok(value.into())
    } else {
        Err(format!(
            "unusable task id '{value}'; use 1 to 128 letters, digits, or -_.:#/"
        ))
    }
}

/** Identify the current repository, by reading its top-level directory and hashing its root
 * commits (sorted, so the result does not depend on Git's walk order); a repository without
 * commits is "unborn"
 * Input
    - None
 * Output
    - Result<(PathBuf, String), String> repository root and identity
    - Error if the current directory is not inside a Git repository
*/
fn repository_identity() -> Result<(PathBuf, String), String> {
    let root = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?);
    let identity = match git(&["rev-list", "--max-parents=0", "HEAD"]) {
        Ok(commits) if !commits.trim().is_empty() => {
            let mut commits = commits.split_whitespace().collect::<Vec<_>>();
            commits.sort_unstable();
            sha256(format!("crane-repository\n{}\n", commits.join("\n")).as_bytes())
        }
        _ => UNBORN_REPOSITORY.into(),
    };
    Ok((root, identity))
}

/** Move aside the files a failed earlier attempt left for a session id that has no session.json
 * (a journal, lifecycle state, or attestation written for a session that no longer exists), into
 * recovered-<time>-<pid>, so a new session neither inherits a closed state nor mixes its journal
 * with foreign events; the evidence is kept, and stale temporary session files are left alone
 * because another process may still be linking one
 * Input
    - id: &str - Crane session id
 * Output
    - Result<(), String>
    - Error if the leftover files cannot be moved
*/
fn recover_partial(id: &str) -> Result<(), String> {
    let directory = directory(id)?;
    let leftovers = ["journal.jsonl", "state", "attestation.json"]
        .into_iter()
        .filter(|name| directory.join(name).is_file())
        .collect::<Vec<_>>();
    if leftovers.is_empty() || directory.join("session.json").exists() {
        return Ok(());
    }
    let recovered = directory.join(format!("recovered-{}-{}", now_unix(), std::process::id()));
    fs::create_dir_all(&recovered).map_err(io_error)?;
    for name in leftovers {
        match fs::rename(directory.join(name), recovered.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {} // another process moved it
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(())
}

/** Return the directory that holds one session's files, .crane/runtime/sessions/ID
 * Input
    - id: &str - Crane session id
 * Output
    - Result<PathBuf, String>
    - Error if .crane cannot be found
*/
fn directory(id: &str) -> Result<PathBuf, String> {
    Ok(root()?.join("runtime").join("sessions").join(id))
}

/** List stored session ids, sorted
 * Input
    - None
 * Output
    - Result<Vec<String>, String>, empty if no session was ever created
    - Error if .crane cannot be found or the directory cannot be read
*/
pub(crate) fn session_ids() -> Result<Vec<String>, String> {
    let directory = root()?.join("runtime").join("sessions");
    let mut ids = match fs::read_dir(directory) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().join("session.json").is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(io_error(error)),
    };
    ids.sort();
    Ok(ids)
}

/** Read a stored session's latest attestation, if one was written
 * Input
    - id: &str - Crane session id
 * Output
    - Result<Option<Value>, String>
    - Error if the file exists but cannot be read or parsed
*/
pub(crate) fn read_attestation(id: &str) -> Result<Option<Value>, String> {
    match fs::read_to_string(directory(id)?.join("attestation.json")) {
        Ok(content) => serde_json::from_str(&content).map(Some).map_err(io_error),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}

/** Build a session-level violation (contract drift, repository mismatch, or journal problems),
 * owned by a human
 * Input
    - violation_type: &str - machine-readable type
    - message: String - explanation
 * Output
    - Violation with policy_id crane and rule session
*/
fn session_violation(violation_type: &str, message: String) -> Violation {
    Violation {
        policy_id: "crane".into(),
        rule: "session".into(),
        target: String::new(),
        checkpoint: String::new(),
        violation_type: violation_type.into(),
        message,
    }
}

/** Serialize a violation for the attestation, with the same fields as the JSON contract plus the
 * checkpoint commit
 * Input
    - violation: &Violation - violation
    - checkpoint_sha: Option<String> - bound checkpoint commit, if any
 * Output
    - Value JSON object
*/
fn violation_json(violation: &Violation, checkpoint_sha: Option<String>) -> Value {
    json!({
        "policy_id": violation.policy_id,
        "rule": violation.rule,
        "target": violation.target,
        "checkpoint": violation.checkpoint,
        "checkpoint_sha": checkpoint_sha,
        "status": "fail",
        "violation_type": violation.violation_type,
        "repair_owner": repair_owner(violation),
        "message": violation.message,
    })
}
