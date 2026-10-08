// Session orchestrator: one owner for the governed execution lifecycle of an approved task,
// TASK_READY -> SESSION_CREATED -> AGENT_CONNECTED -> RUNNING (-> DEGRADED / QUARANTINED) ->
// STOPPING -> RECONCILING -> VERIFIED -> DELIVERY_READY, or FAILED. It only coordinates: the task
// intake launches the session, the session manager and the authority engine decide every action,
// effect verification observes what actually changed, the autonomy state machine and the budget
// keep their own state, and finalization reconciles the repository and runs the contract tests.
// The orchestrator adds the immutable lifecycle binding, the phase, the freeze at termination, the
// repository tests of the testing policy, and the verdict that alone lets a session be delivered.

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::adapter::{adapter, AgentKind, HookEvent};
use crate::agent_session;
use crate::authority::{AgentAction, Decision, Operation, Proposed, Verdict};
use crate::effects;
use crate::proposals::store::agent_environment;
use crate::repository::root;
use crate::session::{ContractSession, Lifecycle};
use crate::util::{io_error, now_unix, sha256};
use crate::verify::Report;
use crate::zones::model::{Autonomy, SafetyState};

/** Version of the orchestrator record layout */
const RECORD_FORMAT: u64 = 1;

/** Seconds a command run by the built-in executor may take */
const COMMAND_TIMEOUT: u64 = 120;

/** The phases of a governed session
 * Variants
    - TaskReady - the task's contract is approved; nothing runs yet
    - SessionCreated - the contract session exists and is bound
    - AgentConnected - an agent attached to the session
    - Running - the agent acts
    - Degraded - safety is degraded: changes need human approval
    - Quarantined - safety is quarantined: nothing may change
    - Stopping - execution is frozen for termination
    - Reconciling - the repository is reconciled, tests run, the attestation is written
    - Verified - the repository satisfies the contract, its tests, and the testing policy
    - DeliveryReady - verified, with changes to deliver
    - Failed - the session can never be delivered
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Phase {
    TaskReady,
    SessionCreated,
    AgentConnected,
    Running,
    Degraded,
    Quarantined,
    Stopping,
    Reconciling,
    Verified,
    DeliveryReady,
    Failed,
}

impl Phase {
    /** Every phase, in lifecycle order */
    pub(crate) const ALL: [Phase; 11] = [
        Self::TaskReady,
        Self::SessionCreated,
        Self::AgentConnected,
        Self::Running,
        Self::Degraded,
        Self::Quarantined,
        Self::Stopping,
        Self::Reconciling,
        Self::Verified,
        Self::DeliveryReady,
        Self::Failed,
    ];

    /** Return the phase's name
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::TaskReady => "TASK_READY",
            Self::SessionCreated => "SESSION_CREATED",
            Self::AgentConnected => "AGENT_CONNECTED",
            Self::Running => "RUNNING",
            Self::Degraded => "DEGRADED",
            Self::Quarantined => "QUARANTINED",
            Self::Stopping => "STOPPING",
            Self::Reconciling => "RECONCILING",
            Self::Verified => "VERIFIED",
            Self::DeliveryReady => "DELIVERY_READY",
            Self::Failed => "FAILED",
        }
    }

    /** Parse a phase name
     * Input
        - value: &str - name
     * Output
        - Result<Phase, String>
    */
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|phase| phase.name() == value)
            .ok_or_else(|| format!("unknown session phase '{value}'"))
    }

    /** Check whether the phase is final (nothing moves it again)
     * Input
        - None (uses self)
     * Output
        - bool
    */
    pub(crate) fn terminal(self) -> bool {
        matches!(self, Self::DeliveryReady | Self::Failed)
    }

    /** Check whether execution is frozen in this phase (termination started or done)
     * Input
        - None (uses self)
     * Output
        - bool
    */
    pub(crate) fn frozen(self) -> bool {
        matches!(
            self,
            Self::Stopping
                | Self::Reconciling
                | Self::Verified
                | Self::DeliveryReady
                | Self::Failed
        )
    }

    /** Check whether the state machine allows a transition: the lifecycle only moves forward,
     * the safety phases move among themselves and back to running while the agent acts, every
     * phase before VERIFIED can fail, and only reconciliation can verify
     * Input
        - to: Phase - next phase
     * Output
        - bool
    */
    pub(crate) fn allows(self, to: Phase) -> bool {
        use Phase::*;
        if to == Failed {
            return !matches!(self, Verified | DeliveryReady | Failed);
        }
        matches!(
            (self, to),
            (TaskReady, SessionCreated)
                | (SessionCreated, AgentConnected | Stopping)
                | (AgentConnected, Running | Degraded | Quarantined | Stopping)
                | (Running, Degraded | Quarantined | Stopping)
                | (Degraded, Running | Quarantined | Stopping)
                | (Quarantined, Running | Degraded | Stopping)
                | (Stopping, Reconciling)
                | (Reconciling, Verified)
                | (Verified, DeliveryReady)
        )
    }
}

/** The orchestrator's answer to one action
 * Variants
    - Allow - the action may run
    - Deny - the action must not run
    - Approval - a human must approve it first
    - Quarantine - denied, and the session is quarantined
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Allow,
    Deny,
    Approval,
    Quarantine,
}

impl Outcome {
    /** Return the outcome's name
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Allow => "ALLOW",
            Self::Deny => "DENY",
            Self::Approval => "APPROVAL",
            Self::Quarantine => "QUARANTINE",
        }
    }
}

/** Refuse an orchestrator action on behalf of an agent
 * Input
    - action: &str - run or finish
 * Output
    - Result<(), String>
*/
fn require_human(action: &str) -> Result<(), String> {
    match agent_environment() {
        Some(marker) => Err(format!(
            "crane session {action} refuses to run in an agent environment ({marker} is set); a human runs and finishes governed sessions"
        )),
        None => Ok(()),
    }
}

/** Return a session's orchestrator record path, .crane/runtime/orchestrator/ID.json
 * Input
    - id: &str - session id
 * Output
    - Result<PathBuf, String>
*/
fn record_path(id: &str) -> Result<PathBuf, String> {
    Ok(root()?
        .join("runtime")
        .join("orchestrator")
        .join(format!("{id}.json")))
}

/** Load a session's orchestrator record
 * Input
    - id: &str - session id
 * Output
    - Result<Option<Value>, String> None for a session the orchestrator does not govern
*/
pub(crate) fn load(id: &str) -> Result<Option<Value>, String> {
    match fs::read_to_string(record_path(id)?) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| format!("orchestrator record {id} is invalid: {error}")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}

/** Write a record through a temporary file
 * Input
    - record: &Value - record
 * Output
    - Result<(), String>
*/
fn save(record: &Value) -> Result<(), String> {
    let path = record_path(record["session_id"].as_str().unwrap_or_default())?;
    fs::create_dir_all(path.parent().ok_or("invalid record path")?).map_err(io_error)?;
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_string_pretty(record).map_err(io_error)? + "\n",
    )
    .map_err(io_error)?;
    fs::rename(&temporary, &path).map_err(io_error)
}

/** Return a record's phase
 * Input
    - record: &Value - record
 * Output
    - Phase (FAILED for an unreadable phase, so nothing proceeds)
*/
pub(crate) fn phase(record: &Value) -> Phase {
    Phase::parse(record["phase"].as_str().unwrap_or_default()).unwrap_or(Phase::Failed)
}

/** Move a record to a phase when the state machine allows it (the same phase is a no-op)
 * Input
    - record: &mut Value - record
    - to: Phase - next phase
    - cause: &str - what caused it
    - reason: &str - why
 * Output
    - Result<(), String>
    - Error for a transition the state machine refuses
*/
fn transition(record: &mut Value, to: Phase, cause: &str, reason: &str) -> Result<(), String> {
    let from = phase(record);
    if from == to {
        return Ok(());
    }
    if !from.allows(to) {
        return Err(format!(
            "session {} cannot move from {} to {}",
            record["session_id"].as_str().unwrap_or_default(),
            from.name(),
            to.name()
        ));
    }
    record["phase"] = json!(to.name());
    if let Some(history) = record["history"].as_array_mut() {
        history.push(json!({"from": from.name(), "to": to.name(), "at": now_unix(), "cause": cause, "reason": reason}));
    }
    Ok(())
}

/** Build the immutable lifecycle binding of a session: repository, task, contract and its digest,
 * policy version, zone version, checkpoint, agent, autonomy mode, initial safety state, budget, and
 * organization configuration, from the session's own digest-checked binding and the stored
 * contract version it was bound to
 * Input
    - session: &ContractSession - session
 * Output
    - Result<Value, String>
    - Error for a session without an approved task contract
*/
pub(crate) fn binding(session: &ContractSession) -> Result<Value, String> {
    let described = session.describe();
    let governance = &described["governance"];
    let bound = &governance["task_contract"];
    let task = described["task_id"]
        .as_str()
        .ok_or_else(|| format!("session {} is not bound to a task", session.id()))?;
    if bound["status"] != "approved" {
        return Err(format!(
            "session {} is not bound to an approved task contract",
            session.id()
        ));
    }
    let contract = crate::task_contracts::stored(task, bound["version"].as_u64().unwrap_or(0))?;
    let bindings = &contract["bindings"];
    Ok(json!({
        "repository": {"identity": described["repository_id"], "owner": bindings["repository"]["owner"], "name": bindings["repository"]["name"], "root": described["root"]},
        "task": task,
        "contract": {"contract_id": contract["contract_id"], "version": contract["version"]},
        "contract_digest": contract["digest"],
        "policy_version": bindings["policy_version"],
        "zone_version": bindings["zone_set_version"],
        "checkpoint": bindings["checkpoint"],
        "agent": described["agent"],
        "autonomy_mode": governance["autonomy"],
        "safety_state": SafetyState::Active.name(),
        "budget": {
            "mutating_actions": governance["budget"]["mutating_actions"],
            "files": governance["budget"]["files"],
            "model_version": bindings["budget"]["model_version"],
            "risk_budget_max": bindings["budget"]["max"],
        },
        "organization": bindings["organization"],
        "session": {"session_id": described["session_id"], "binding_digest": described["binding_digest"]},
    }))
}

/** Check that a session still matches its recorded lifecycle binding
 * Input
    - record: &Value - record
    - session: &ContractSession - session (its own binding digest was checked on load)
 * Output
    - Result<(), String> why the binding no longer holds
*/
fn verify_binding(record: &Value, session: &ContractSession) -> Result<(), String> {
    let current = binding(session)?;
    let digest = sha256(current.to_string().as_bytes());
    let recorded = sha256(record["binding"].to_string().as_bytes());
    if record["binding_digest"] != digest.as_str() || recorded != digest {
        return Err(format!(
            "the lifecycle binding of session {} no longer matches what it was created with",
            session.id()
        ));
    }
    Ok(())
}

/** Start governing a launched session: record its binding and move TASK_READY -> SESSION_CREATED;
 * an existing record is returned unchanged
 * Input
    - session: &ContractSession - launched session
    - by: &str - who runs it
 * Output
    - Result<Value, String> the record
*/
pub(crate) fn create(session: &ContractSession, by: &str) -> Result<Value, String> {
    if let Some(record) = load(session.id())? {
        return Ok(record);
    }
    let binding = binding(session)?;
    let mut record = json!({
        "orchestrator_format": RECORD_FORMAT,
        "session_id": session.id(),
        "task_id": binding["task"],
        "phase": Phase::TaskReady.name(),
        "binding_digest": sha256(binding.to_string().as_bytes()),
        "binding": binding,
        "created_at": now_unix(),
        "by": by,
        "actions": {"ALLOW": 0, "DENY": 0, "APPROVAL": 0, "QUARANTINE": 0, "executed": 0, "not_executed": 0},
        "history": [],
        "termination": null,
    });
    transition(
        &mut record,
        Phase::SessionCreated,
        by,
        "the contract session was created and bound",
    )?;
    save(&record)?;
    session.record(json!({"event": "orchestrated", "phase": Phase::SessionCreated.name(), "binding_digest": record["binding_digest"]}))?;
    Ok(record)
}

/** Note that an agent attached to a governed session (SESSION_CREATED -> AGENT_CONNECTED)
 * Input
    - session: &ContractSession - session
    - cause: &str - what connected
 * Output
    - Result<(), String>
*/
pub(crate) fn connected(session: &ContractSession, cause: &str) -> Result<(), String> {
    let Some(mut record) = load(session.id())? else {
        return Ok(());
    };
    if phase(&record) == Phase::SessionCreated {
        transition(
            &mut record,
            Phase::AgentConnected,
            cause,
            "an agent attached to the session",
        )?;
        save(&record)?;
    }
    Ok(())
}

/** Bring the phase in line with the session's safety while the agent acts: degraded and
 * quarantined safety have their phases, and recovered safety returns to RUNNING
 * Input
    - record: &mut Value - record
    - session: &ContractSession - session
    - cause: &str - what happened
 * Output
    - Result<(), String>
*/
fn follow_safety(record: &mut Value, session: &ContractSession, cause: &str) -> Result<(), String> {
    let current = phase(record);
    if current.frozen() {
        return Ok(());
    }
    if matches!(current, Phase::SessionCreated | Phase::TaskReady) {
        transition(record, Phase::AgentConnected, cause, "the agent acted")?;
    }
    let activity = session.activity();
    let reason = activity.safety_reason.clone().unwrap_or_default();
    match activity.state.safety {
        SafetyState::Quarantined => transition(record, Phase::Quarantined, cause, &reason),
        SafetyState::Degraded => transition(record, Phase::Degraded, cause, &reason),
        SafetyState::Active => transition(record, Phase::Running, cause, "the agent acts"),
    }
}

/** Decide one normalized action of a session (steps 1 to 5 of every action): refuse it when
 * execution is frozen or the lifecycle binding no longer holds, otherwise let the session manager
 * and the authority engine decide it (contract, zones, autonomy, budget, safety; evidence is
 * journaled there), then follow the session's safety; sessions the orchestrator does not govern
 * are decided the same way without a phase
 * Input
    - session: &ContractSession - bound session
    - protected: &[&str] - provider configuration files guarded like .crane
    - action: &AgentAction - normalized action
    - event: &str - pre_tool_use or permission_request
 * Output
    - Result<(Outcome, Verdict), String>
*/
pub(crate) fn decide(
    session: &ContractSession,
    protected: &[&str],
    action: &AgentAction,
    event: &str,
) -> Result<(Outcome, Verdict), String> {
    let mut record = load(session.id())?;
    if let Some(record) = record.as_mut() {
        let current = phase(record);
        let refusal = if current.frozen() {
            Some(format!(
                "execution is frozen: the session is {}",
                current.name()
            ))
        } else {
            verify_binding(record, session).err()
        };
        if let Some(reason) = refusal {
            if action.operation != Operation::Read || current == Phase::Failed {
                if !current.frozen() {
                    transition(record, Phase::Failed, event, &reason)?;
                }
                tally(record, Outcome::Deny);
                save(record)?;
                session.record(json!({
                    "event": event,
                    "tool": action.tool,
                    "operation": action.operation.name(),
                    "decision": Decision::Deny.name(),
                    "reasons": [reason],
                    "arguments_digest": action.digest,
                    "result": "blocked",
                    "summary": agent_session::summary(action),
                }))?;
                return Ok((
                    Outcome::Deny,
                    Verdict {
                        decision: Decision::Deny,
                        reasons: vec![reason],
                        resources: Vec::new(),
                        policies: Vec::new(),
                        zones: Vec::new(),
                    },
                ));
            }
        }
    }
    let verdict = agent_session::authorize(session, protected, action, event)?;
    let quarantined = session.activity().state.safety == SafetyState::Quarantined;
    let outcome = match verdict.decision {
        Decision::Allow => Outcome::Allow,
        Decision::ApprovalRequired => Outcome::Approval,
        Decision::Deny if quarantined => Outcome::Quarantine,
        Decision::Deny => Outcome::Deny,
    };
    if let Some(mut record) = record {
        tally(&mut record, outcome);
        follow_safety(&mut record, session, event)?;
        save(&record)?;
    }
    Ok((outcome, verdict))
}

/** Count an outcome in the record
 * Input
    - record: &mut Value - record
    - outcome: Outcome - outcome
 * Output
    - None
*/
fn tally(record: &mut Value, outcome: Outcome) {
    let count = record["actions"][outcome.name()].as_u64().unwrap_or(0) + 1;
    record["actions"][outcome.name()] = json!(count);
}

/** Observe an executed action (steps 7 to 9): what actually changed in the repository is
 * verified incrementally against the bound contract, journaled with its budget consumption, and
 * fed to the autonomy state machine and the budget; then the phase follows the session's safety
 * Input
    - session: &ContractSession - bound session
    - action: &AgentAction - the action that ran
    - protected: &[&str] - provider configuration files guarded like .crane
 * Output
    - Result<Report, String> the incremental verification
*/
pub(crate) fn observe(
    session: &ContractSession,
    action: &AgentAction,
    protected: &[&str],
) -> Result<Report, String> {
    // Reads change nothing; everything else is judged by what actually changed
    let (report, effect) = if action.operation == Operation::Read {
        (
            Report {
                passes: Vec::new(),
                violations: Vec::new(),
            },
            Value::Null,
        )
    } else {
        effects::observe(session, action, protected)?
    };
    let consumption = crate::budget::manage::consumption(session, action);
    let mut entry = json!({
        "event": "post_tool_use",
        "summary": agent_session::summary(action),
        "tool": action.tool,
        "operation": action.operation.name(),
        "resources": action.files.iter().map(|change| format!("file:{}", change.path)).collect::<Vec<_>>(),
        "arguments_digest": action.digest,
        "result": "executed",
        "verified_clauses": effect["clauses_checked"].as_u64().unwrap_or(0),
        "verification": if report.violations.is_empty() { "pass" } else { "fail" },
        "effect": effect,
    });
    if let Some(amount) = consumption {
        entry["budget_consume"] =
            json!({"amount": amount, "compliant": agent_session::behaviour(&report).is_empty()});
    }
    session.record(entry)?;
    agent_session::after_tool(session)?;
    agent_session::after_effect(session, &report)?;
    crate::budget::manage::reward_compliance(session)?;
    if let Some(mut record) = load(session.id())? {
        let executed = record["actions"]["executed"].as_u64().unwrap_or(0) + 1;
        record["actions"]["executed"] = json!(executed);
        follow_safety(&mut record, session, "post_tool_use")?;
        save(&record)?;
    }
    Ok(report)
}

/** Resolve a path an action names against the session root, refusing one outside it
 * Input
    - root: &Path - session root
    - path: &str - absolute or relative path
 * Output
    - Result<PathBuf, String>
*/
fn inside(root: &Path, path: &str) -> Result<PathBuf, String> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        root.join(path)
    };
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let parent = candidate.parent().unwrap_or(root);
    let canonical_parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    if canonical_parent.starts_with(&canonical_root) || parent.starts_with(root) {
        Ok(candidate)
    } else {
        Err(format!("{path} is outside the session repository"))
    }
}

/** Execute a permitted action with the built-in executor (step 6, for agents driven by an action
 * script): writes are applied exactly as proposed, commands run in the session root with a
 * timeout, reads and other tools change nothing here
 * Input
    - session: &ContractSession - session
    - action: &AgentAction - permitted action
 * Output
    - Result<Value, String> what was done, or why it could not be
*/
fn execute(session: &ContractSession, action: &AgentAction) -> Result<Value, String> {
    let root = session.root_path();
    match action.operation {
        Operation::Write => {
            for change in &action.files {
                let path = inside(root, &change.path)?;
                match &change.proposed {
                    Proposed::Content(text) => {
                        if let Some(folder) = path.parent() {
                            fs::create_dir_all(folder).map_err(io_error)?;
                        }
                        fs::write(&path, text).map_err(io_error)?;
                    }
                    Proposed::Edits(edits) => {
                        let mut text = fs::read_to_string(&path).map_err(io_error)?;
                        for edit in edits {
                            if !text.contains(&edit.old) {
                                return Err(format!(
                                    "{}: the text to replace is not in the file",
                                    change.path
                                ));
                            }
                            text = if edit.all {
                                text.replace(&edit.old, &edit.new)
                            } else {
                                text.replacen(&edit.old, &edit.new, 1)
                            };
                        }
                        fs::write(&path, text).map_err(io_error)?;
                    }
                    Proposed::Delete => fs::remove_file(&path).map_err(io_error)?,
                    Proposed::Unknown => {
                        return Err(format!(
                            "{}: the proposed content is unknown, so it cannot be applied",
                            change.path
                        ))
                    }
                }
            }
            Ok(json!({"applied": action.files.len()}))
        }
        Operation::Execute => {
            let command = action.command.clone().unwrap_or_default();
            let shell = if cfg!(windows) {
                vec!["cmd".to_string(), "/C".to_string(), command]
            } else {
                vec!["sh".to_string(), "-c".to_string(), command]
            };
            Ok(effects::run(root, &shell, COMMAND_TIMEOUT))
        }
        _ => Ok(json!({"applied": 0})),
    }
}

/** How a governed run gets its agent's actions
 * Variants
    - Script(Vec<Value>) - actions in Crane's neutral format, executed by the built-in executor
    - Command(Vec<String>) - an agent process (Claude Code, Codex, ...) started with CRANE_SESSION
      and CRANE_TASK_ID, whose hooks bring every action through the orchestrator
    - Detach - the session is prepared; a human starts the agent and later finishes the session
*/
pub(crate) enum Driver {
    Script(Vec<Value>),
    Command(Vec<String>),
    Detach,
}

/** Drive scripted actions through the per-action pipeline: decide, execute where permitted (an
 * approval is executed only when the human running the command approved actions up front),
 * observe, and stop at a stop marker, a frozen or quarantined session, or the script's end;
 * agent claims are recorded and never trusted
 * Input
    - session: &ContractSession - session
    - actions: &[Value] - neutral actions
    - approve: bool - the human approves actions that need approval
 * Output
    - Result<Vec<Value>, String> one line per action
*/
fn drive_script(
    session: &ContractSession,
    actions: &[Value],
    approve: bool,
) -> Result<Vec<Value>, String> {
    let translator = adapter(AgentKind::Generic);
    let mut lines = Vec::new();
    connected(session, "crane session run")?;
    for (index, payload) in actions.iter().enumerate() {
        if matches!(payload["operation"].as_str(), Some("claim" | "stop")) {
            session.record(
                json!({"event": "agent_claim", "claim": payload["text"], "trusted": false}),
            )?;
            lines.push(json!({"index": index, "claim": payload["text"], "trusted": false}));
            if payload["operation"] == "stop" {
                break;
            }
            continue;
        }
        let action = translator
            .translate(HookEvent::PreToolUse, payload)
            .action
            .ok_or("an action script entry is not an action")?;
        let (outcome, verdict) = decide(session, &[], &action, "pre_tool_use")?;
        let permitted = outcome == Outcome::Allow || (outcome == Outcome::Approval && approve);
        let mut line = json!({"index": index, "tool": action.tool, "operation": action.operation.name(), "outcome": outcome.name(), "reasons": verdict.reasons});
        if permitted {
            match execute(session, &action) {
                Ok(result) => {
                    line["executed"] = json!(true);
                    line["result"] = result;
                    let report = observe(session, &action, &[])?;
                    line["verification"] = json!(if report.violations.is_empty() {
                        "pass"
                    } else {
                        "fail"
                    });
                }
                Err(error) => {
                    line["executed"] = json!(false);
                    line["error"] = json!(error);
                }
            }
        } else {
            line["executed"] = json!(false);
            if let Some(mut record) = load(session.id())? {
                let count = record["actions"]["not_executed"].as_u64().unwrap_or(0) + 1;
                record["actions"]["not_executed"] = json!(count);
                save(&record)?;
            }
        }
        lines.push(line);
        let current = load(session.id())?.map(|record| phase(&record));
        if matches!(current, Some(Phase::Quarantined | Phase::Failed)) || !session.resumable() {
            break;
        }
    }
    Ok(lines)
}

/** Start an agent process for a session and wait for it: its hooks bind to the session through
 * CRANE_SESSION and CRANE_TASK_ID, and every action goes through the orchestrator
 * Input
    - session: &ContractSession - session
    - command: &[String] - program and arguments
 * Output
    - Result<Value, String> {program, exit}
*/
fn drive_command(session: &ContractSession, command: &[String]) -> Result<Value, String> {
    let (program, arguments) = command.split_first().ok_or("no agent command given")?;
    let status = std::process::Command::new(program)
        .args(arguments)
        .current_dir(session.root_path())
        // The agent's own output goes to stderr, so Crane's report on stdout stays readable
        .stdout(std::io::stderr())
        .env("CRANE_SESSION", session.id())
        .env("CRANE_TASK_ID", session.identity().1.unwrap_or_default())
        .status()
        .map_err(|error| format!("could not start the agent '{program}': {error}"))?;
    Ok(json!({"program": program, "exit": status.code()}))
}

/** Terminate a governed session: freeze execution (STOPPING), reconcile the repository as it is
 * (not as the agent says), run the mandatory contract tests and write the attestation (session
 * finalization), run the repository tests the testing policy asks for, and decide VERIFIED (then
 * DELIVERY_READY when there are changes to deliver) or FAILED; a terminated session is returned
 * unchanged, and a FAILED one stays FAILED whatever happens to the repository afterwards
 * Input
    - id: &str - session id
    - by: &str - who finishes it
 * Output
    - Result<Value, String> the record
*/
pub(crate) fn terminate(id: &str, by: &str) -> Result<Value, String> {
    require_human("finish")?;
    let mut record = load(id)?.ok_or_else(|| {
        format!(
            "session {id} is not governed by the orchestrator; run tasks with 'crane session run'"
        )
    })?;
    let current = phase(&record);
    if current.terminal() || current == Phase::Verified {
        record["already_terminated"] = json!(true);
        return Ok(record);
    }
    let session =
        ContractSession::load(id)?.ok_or_else(|| format!("no contract session '{id}'"))?;
    let mut failures = Vec::new();
    if !current.frozen() {
        transition(&mut record, Phase::Stopping, by, "execution is frozen")?;
        save(&record)?;
    }
    let binding_error = verify_binding(&record, &session).err();
    transition(
        &mut record,
        Phase::Reconciling,
        by,
        "final reconciliation, contract tests, repository tests, attestation",
    )?;
    save(&record)?;
    let attestation = match agent_session::finalize(id) {
        Ok(attestation) => attestation,
        Err(error) => {
            failures.push(format!("finalization failed: {error}"));
            Value::Null
        }
    };
    let session =
        ContractSession::load(id)?.ok_or_else(|| format!("no contract session '{id}'"))?;
    let repository_tests = repository_tests(&session)?;
    let activity = session.activity();
    let final_decision = &attestation["final_attestation"]["final_decision"]["decision"];
    let mut checks = vec![
        json!({"check": "reconciliation", "passed": attestation["final_status"] == "PASS", "detail": attestation["final_status"]}),
        json!({"check": "contract_tests", "passed": attestation["contract_tests"]["failed"].as_u64().unwrap_or(0) == 0, "detail": attestation["contract_tests"]}),
        json!({"check": "repository_tests", "passed": repository_tests["status"] != "failed", "detail": repository_tests}),
        json!({"check": "attestation", "passed": final_decision == "PASS", "detail": final_decision}),
        json!({"check": "safety", "passed": activity.state.safety != SafetyState::Quarantined, "detail": activity.state.safety.name()}),
        json!({"check": "binding", "passed": binding_error.is_none(), "detail": binding_error}),
    ];
    for check in &checks {
        if check["passed"] != true {
            failures.push(format!(
                "{} did not pass ({})",
                check["check"].as_str().unwrap_or_default(),
                match &check["detail"] {
                    Value::String(text) => text.clone(),
                    Value::Null => "unavailable".into(),
                    other => other.to_string().chars().take(200).collect(),
                }
            ));
        }
    }
    failures.dedup();
    let changed = attestation["final_attestation"]["action_summary"]["files_changed"]
        .as_array()
        .map_or(0, Vec::len);
    checks.push(json!({"check": "changes", "passed": changed > 0, "detail": changed}));
    record["termination"] = json!({
        "by": by,
        "at": now_unix(),
        "final_status": attestation["final_status"],
        "attestation_digest": attestation["final_attestation"]["attestation_digest"],
        "contract_tests": attestation["contract_tests"],
        "repository_tests": repository_tests,
        "checks": checks,
        "failures": failures,
        "files_changed": changed,
    });
    if failures.is_empty() {
        transition(
            &mut record,
            Phase::Verified,
            by,
            "the repository satisfies the contract, its tests, and the testing policy",
        )?;
        if changed > 0 {
            transition(
                &mut record,
                Phase::DeliveryReady,
                by,
                "verified changes can be delivered",
            )?;
        }
    } else {
        transition(&mut record, Phase::Failed, by, &failures.join("; "))?;
    }
    save(&record)?;
    session
        .record(json!({"event": "orchestrated", "phase": record["phase"], "failures": failures}))?;
    Ok(record)
}

/** Run the repository tests the testing policy asks for at termination: .crane/testing.json
 * "session_tests" is "affected" (the default: the tests affected by the session, already part of
 * the reconciliation), "all" (every organizational and agent-authored test as well), or "none"
 * Input
    - session: &ContractSession - finalized session
 * Output
    - Result<Value, String> {policy, status, ...}
*/
fn repository_tests(session: &ContractSession) -> Result<Value, String> {
    let config = effects::testing_config()?;
    let policy = config["session_tests"].as_str().unwrap_or("affected");
    match policy {
        "none" => Ok(json!({"policy": "none", "status": "skipped"})),
        "affected" => Ok(json!({"policy": "affected", "status": "included in the reconciliation"})),
        "all" => {
            let checkpoints = session.describe()["contracts"]["contracts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|contract| contract["checkpoint_sha"].as_str().map(String::from))
                .collect::<BTreeSet<_>>();
            let agent = session.activity().files;
            let mut result = effects::within(session.root_path(), || {
                crate::contract_tests::ordinary_tests(&checkpoints, &agent)
            })?;
            result["policy"] = json!("all");
            Ok(result)
        }
        other => Ok(
            json!({"policy": other, "status": "failed", "error": format!("unknown session_tests policy '{other}'; use affected, all, or none")}),
        ),
    }
}

/** Note on a governed session that its verified change was delivered (merged); the phase stays
 * DELIVERY_READY, the record gains the delivery
 * Input
    - id: &str - session id
    - merge_sha: &str - merge commit
    - by: &str - who merged
 * Output
    - Result<(), String>
*/
pub(crate) fn delivered(id: &str, merge_sha: &str, by: &str) -> Result<(), String> {
    if let Some(mut record) = load(id)? {
        record["delivery"] =
            json!({"state": "COMPLETE", "merge_sha": merge_sha, "by": by, "at": now_unix()});
        save(&record)?;
    }
    Ok(())
}

/** Refuse to deliver a governed session that is not DELIVERY_READY (sessions the orchestrator
 * does not govern keep their own delivery checks)
 * Input
    - id: &str - session id
 * Output
    - Result<(), String>
*/
pub(crate) fn delivery_allowed(id: &str) -> Result<(), String> {
    match load(id)? {
        Some(record) if phase(&record) != Phase::DeliveryReady => Err(format!(
            "governed session {id} is {}; only a DELIVERY_READY session can be delivered",
            phase(&record).name()
        )),
        _ => Ok(()),
    }
}

/** Run an approved task through the whole governed lifecycle with one call: launch its session
 * with the chosen agent (task intake), start governing it, drive the agent (an action script, an
 * agent process, or nothing when detached), and terminate it into VERIFIED/DELIVERY_READY or
 * FAILED; refused on behalf of an agent
 * Input
    - task: &str - task id with an approved contract
    - agent: Option<String> - claude, codex, or generic
    - autonomy: Option<Autonomy> - autonomy mode
    - driver: Driver - how actions arrive
    - approve: bool - the human approves actions that need approval (scripts only)
    - by: &str - who runs it
 * Output
    - Result<Value, String> {record, actions, agent}
*/
pub(crate) fn run(
    task: &str,
    agent: Option<String>,
    autonomy: Option<Autonomy>,
    driver: Driver,
    approve: bool,
    by: &str,
) -> Result<Value, String> {
    require_human("run")?;
    let launched = crate::intake::launch(task, agent, autonomy, false, by)?;
    let id = launched["launch"]["session_id"]
        .as_str()
        .ok_or("the launch recorded no session")?
        .to_string();
    let session =
        ContractSession::load(&id)?.ok_or_else(|| format!("no contract session '{id}'"))?;
    let record = create(&session, by)?;
    if phase(&record).frozen() {
        return Err(format!(
            "session {id} already ran and is {}; plan and approve the task again for a new run",
            phase(&record).name()
        ));
    }
    let mut result = json!({"session_id": id, "actions": [], "agent_process": null});
    match driver {
        Driver::Detach => {
            result["record"] = record;
            result["detached"] = json!(true);
            return Ok(result);
        }
        Driver::Script(actions) => {
            result["actions"] = json!(drive_script(&session, &actions, approve)?);
        }
        Driver::Command(command) => {
            result["agent_process"] = drive_command(&session, &command)?;
        }
    }
    let session =
        ContractSession::load(&id)?.ok_or_else(|| format!("no contract session '{id}'"))?;
    if session.lifecycle() == Lifecycle::Cancelled {
        let mut record = load(&id)?.unwrap_or(record);
        transition(&mut record, Phase::Failed, by, "the session was cancelled")?;
        save(&record)?;
        result["record"] = record;
        return Ok(result);
    }
    result["record"] = terminate(&id, by)?;
    Ok(result)
}
