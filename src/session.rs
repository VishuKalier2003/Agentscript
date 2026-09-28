use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::adapter::AgentKind;
use crate::authority::{derive_grants, grant_from_json, grant_to_json, Grant, Runtime};
use crate::ir::{compile, ContractSet};
use crate::model::Violation;
use crate::repository::{git, root};
use crate::scope::ScopeContext;
use crate::util::{io_error, now_unix, sha256};
use crate::verify::{repair_owner, verify_contracts, Report};

/** Version of the session, journal, and attestation file layout */
const SESSION_FORMAT: u64 = 1;

/** Lifecycle state of a contract session, the only part of a session that changes after creation
 * Variants
    - Active - the agent is working under the session's contract
    - Closed - the agent session ended; a resumed agent session reactivates it with the same
      binding
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lifecycle {
    Active,
    Closed,
}

/** One agent session bound to one immutable contract: the policies, their versions, and their
 * checkpoint commits are fixed when the session is created and are used for runtime authority,
 * reconciliation, and final verification alike; nothing an agent does can refresh them
 * Fields
    - id: String - Crane session id, "<agent>-<provider session id>"
    - agent: AgentKind - agent profile
    - provider_session: String - the provider's own session id
    - created_at: u64 - Unix seconds
    - expires_at: Option<u64> - Unix seconds after which every mutating action is denied
    - root: PathBuf - repository root that action paths are resolved against
    - contracts: ContractSet - the bound contract IR
    - grants: Vec<Grant> - runtime authority derived from the contracts at creation
    - persisted: bool - false for a transient session used when the provider sends no session id
*/
pub(crate) struct ContractSession {
    pub(crate) id: String,
    pub(crate) agent: AgentKind,
    pub(crate) provider_session: String,
    pub(crate) created_at: u64,
    pub(crate) expires_at: Option<u64>,
    pub(crate) root: PathBuf,
    pub(crate) contracts: ContractSet,
    pub(crate) grants: Vec<Grant>,
    persisted: bool,
}

impl ContractSession {
    /** Load the session for a provider session id, or create it on first sight, by compiling the
     * policies on disk, deriving runtime authority from their checkpoints, and writing the
     * session file only if no other hook process created it first; an existing session is never
     * replaced or refreshed
     * Input
        - agent: AgentKind - agent profile
        - provider_session: &str - provider's session id
        - ttl: Option<u64> - seconds until the session expires, only used on creation
     * Output
        - Result<(ContractSession, bool), String> the session and whether it was just created
        - Error if the id is unusable, the policies cannot be read, or the files cannot be written
    */
    pub(crate) fn establish(
        agent: AgentKind,
        provider_session: &str,
        ttl: Option<u64>,
    ) -> Result<(Self, bool), String> {
        let id = session_id(agent, provider_session)?;
        if let Some(session) = Self::load(&id)? {
            return Ok((session, false));
        }
        let mut session = Self::transient(agent)?;
        session.id = id;
        session.provider_session = provider_session.into();
        session.expires_at = ttl.map(|seconds| session.created_at.saturating_add(seconds));
        session.persisted = true;
        if session.persist()? {
            Ok((session, true))
        } else {
            let existing = Self::load(&session.id)?.ok_or("contract session disappeared")?;
            Ok((existing, false))
        }
    }

    /** Build a session that is never written to disk, for providers that send no session id;
     * it compiles the policies and derives authority exactly like a stored session
     * Input
        - agent: AgentKind - agent profile
     * Output
        - Result<ContractSession, String>
        - Error if the policies or the repository root cannot be read
    */
    pub(crate) fn transient(agent: AgentKind) -> Result<Self, String> {
        let contracts = compile()?;
        let grants = derive_grants(&contracts);
        Ok(Self {
            id: "transient".into(),
            agent,
            provider_session: String::new(),
            created_at: now_unix(),
            expires_at: None,
            root: PathBuf::from(git(&["rev-parse", "--show-toplevel"])?),
            contracts,
            grants,
            persisted: false,
        })
    }

    /** Load a stored session by id, by reading its session.json and rebuilding the contract IR and
     * grants (the IR checks its own version, so an edited file is rejected)
     * Input
        - id: &str - Crane session id
     * Output
        - Result<Option<ContractSession>, String>, None if no such session exists
        - Error if the file is unreadable or invalid
    */
    pub(crate) fn load(id: &str) -> Result<Option<Self>, String> {
        let path = directory(id)?.join("session.json");
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
        };
        let invalid = |detail: String| format!("contract session {id} is invalid: {detail}");
        let value: Value =
            serde_json::from_str(&content).map_err(|error| invalid(error.to_string()))?;
        if value.get("session_format").and_then(Value::as_u64) != Some(SESSION_FORMAT) {
            return Err(invalid("unsupported session format".into()));
        }
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| invalid(format!("missing '{key}'")))
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
        Ok(Some(Self {
            id: text("session_id")?.into(),
            agent: AgentKind::parse(Some(text("agent")?))?,
            provider_session: text("provider_session")?.into(),
            created_at: value.get("created_at").and_then(Value::as_u64).unwrap_or(0),
            expires_at: value.get("expires_at").and_then(Value::as_u64),
            root: PathBuf::from(text("root")?),
            contracts,
            grants,
            persisted: true,
        }))
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
        let document = json!({
            "session_format": SESSION_FORMAT,
            "session_id": self.id,
            "agent": self.agent.name(),
            "provider_session": self.provider_session,
            "created_at": self.created_at,
            "expires_at": self.expires_at,
            "root": self.root.to_string_lossy(),
            "contracts": self.contracts.to_json(),
            "grants": self.grants.iter().map(grant_to_json).collect::<Vec<_>>(),
        });
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

    /** Build the runtime authority for this session, treating an expired or closed session as
     * unusable
     * Input
        - protected: &[&str] - provider-specific protected settings files
     * Output
        - Runtime borrowing this session's contracts and grants
    */
    pub(crate) fn runtime<'a>(&'a self, protected: &'a [&'a str]) -> Runtime<'a> {
        Runtime {
            contracts: &self.contracts,
            grants: &self.grants,
            root: self.root.clone(),
            protected,
            problem: self.expired().or_else(|| {
                (self.lifecycle() == Lifecycle::Closed).then(|| {
                    format!(
                        "contract session {} is closed; a new agent session is required",
                        self.id
                    )
                })
            }),
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
        let text = match state {
            Lifecycle::Active => "active\n",
            Lifecycle::Closed => "closed\n",
        };
        fs::write(directory(&self.id)?.join("state"), text).map_err(io_error)
    }

    /** Append an event to the session's journal, by stamping it with a sequence number, the time,
     * and the session binding (session, agent, contract version, checkpoints) and writing it as one
     * JSON line; transient sessions keep no journal
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

    /** Read the journal, by parsing every line and reporting (not skipping) any line that is not a
     * JSON object bound to this session and contract version
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
                        && event["contract_version"] == json!(self.contracts.version) =>
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

    /** Reconcile the session into one contract outcome, by verifying the bound contract against
     * the current worktree, failing closed when the repository's contract on disk no longer
     * matches the bound one or the journal cannot be trusted, and summarizing the journal's
     * authorized and denied actions into an attestation
     * Input
        - None (uses self)
     * Output
        - (Report, Value) verification report (including session findings) and the attestation
    */
    pub(crate) fn reconcile(&self) -> (Report, Value) {
        let mut report = verify_contracts(&self.contracts, &mut ScopeContext::new(), &|_| true);
        let disk_version = compile().map(|set| set.version);
        if disk_version.as_deref() != Ok(self.contracts.version.as_str()) {
            report.violations.push(session_violation(
                "contract_drift",
                format!(
                    "policies or checkpoints changed during contract session {} (bound {}, repository {}); runtime authority and verification keep using the bound contract, so a human must start a new agent session to adopt the change",
                    self.id,
                    self.contracts.version,
                    disk_version.as_deref().unwrap_or_else(|error| error)
                ),
            ));
        }
        let (events, journal_digest, journal_problem) = self.journal();
        if let Some(problem) = &journal_problem {
            report
                .violations
                .push(session_violation("journal_error", problem.clone()));
        }
        let attestation = self.attestation(&report, &events, &journal_digest, disk_version);
        (report, attestation)
    }

    /** Build the attestation object, by listing the bound contracts, the journal's allowed and
     * denied pre-execution decisions, each rule's result, other findings, answers to the
     * reconciliation questions, and evidence identifiers (journal digest, HEAD, time)
     * Input
        - report: &Report - verification report including session findings
        - events: &[Value] - journal events
        - journal_digest: &str - SHA-256 of the journal bytes
        - disk_version: Result<String, String> - contract version currently on disk
     * Output
        - Value attestation JSON
    */
    fn attestation(
        &self,
        report: &Report,
        events: &[Value],
        journal_digest: &str,
        disk_version: Result<String, String>,
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
            "contract_version": self.contracts.version,
            "verification_contract_version": self.contracts.version,
            "repository_contract_version": disk_version.clone().unwrap_or_else(|error| format!("unavailable: {error}")),
            "contracts": self.contracts.contracts.iter().map(|contract| json!({
                "contract_id": contract.policy_id,
                "contract_version": contract.version,
                "checkpoint": contract.checkpoint,
                "checkpoint_sha": contract.checkpoint_sha.clone().ok(),
            })).collect::<Vec<_>>(),
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

/** Build a session-level violation (contract drift or journal problems), owned by a human
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
