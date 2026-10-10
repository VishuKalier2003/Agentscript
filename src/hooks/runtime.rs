// The hook runtime: 'crane agent hook --event EVENT --profile PROFILE', run by the agent host for
// every lifecycle event. It establishes identity, decides tool calls before they run, verifies
// their actual effects afterwards, accounts autonomy credits, reconciles at stop, attests at the
// end, and records every step as a security event. Whenever a decision or verification cannot be
// obtained the adapter answers fail-closed, never as an allow or a clean result.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::action::{AgentAction, Operation};
use super::adapter::{
    adapter, AgentAdapter, AgentKind, Feedback, HookEvent, ProviderEvent, Verdict,
};
use super::enforce::{repository_path, Decided, Guard};
use crate::governance::policy::PolicySet;
use crate::governance::state::{integrity, State};
use crate::governance::verify::{baseline, evaluate, Options, Outcome, Report, ScanMode};
use crate::governance::workspace::Workspace;
use crate::platform::files::{write_atomic, Lock};
use crate::platform::{io_error, now_millis};
use crate::selection::registry::Operation as SelectionOperation;
use crate::telemetry::events::{Bindings, Event, Measurement, Scope, Source};
use crate::telemetry::identity::{Observed, SessionRecord, SuppliedContext, TaskRecord};
use crate::telemetry::ledger::{Kind, Ledger, Operation as LedgerOperation};
use crate::telemetry::model::{
    Assessment, Decision, Domain, Enforcement, Execution, Safety, Severity, Telemetry,
};
use crate::telemetry::Stores;
use crate::telemetry::{alerts, sync};
use crate::trust::crypto::{digest, sha512_hex};

/** Largest hook payload read from stdin, in bytes */
const MAX_PAYLOAD: u64 = 16 * 1024 * 1024;

/** Largest baseline excerpt included in repair instructions, in bytes */
const MAX_RESTORE_EXCERPT: usize = 4096;

/** Phrases in a prompt that suggest an instruction-integrity attack on the agent or on Crane */
const INJECTION_PHRASES: &[&str] = &[
    "ignore previous instructions",
    "ignore all previous",
    "disregard the above",
    "disregard previous",
    "disable crane",
    "bypass crane",
    "uninstall crane",
    "edit .crane",
    "modify .crane",
    "delete .crane",
    "remove the hook",
    "disable the hook",
    "jailbreak",
    "reveal your system prompt",
];

/** What the pre-tool hook decided for a tool call, kept for the post-tool hook
 * Fields
    - at: u64 - Unix milliseconds of the decision
    - decision: Decision - authority decision
    - cost: u64 - credits the action costs
    - reserved: u64 - credits reserved
    - operation: String - read, write, execute, or other
    - tool: String - tool name
    - selections: Vec<String> - selections touched
*/
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CallRecord {
    at: u64,
    decision: Decision,
    cost: u64,
    reserved: u64,
    operation: String,
    tool: String,
    selections: Vec<String>,
}

/** Per-session correlation state of tool calls
 * Fields
    - calls: BTreeMap<String, CallRecord> - pre-tool decisions by tool call
    - known_violations: BTreeSet<String> - violations already reported in this session
*/
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Calls {
    calls: BTreeMap<String, CallRecord>,
    known_violations: BTreeSet<String>,
}

/** Everything a hook invocation works with
 * Fields
    - adapter: Box<dyn AgentAdapter> - provider adapter
    - event: HookEvent - lifecycle event
    - provider: ProviderEvent - translated payload
    - workspace: Workspace - repository
    - state: State - governance state
    - stores: Stores - runtime stores
    - trust_home: Option<PathBuf> - the trust directory
    - started: Instant - when the hook started
    - hook_event_id: String - identifier of this hook invocation
*/
struct Hook {
    adapter: Box<dyn AgentAdapter>,
    event: HookEvent,
    provider: ProviderEvent,
    workspace: Workspace,
    state: State,
    stores: Stores,
    trust_home: Option<PathBuf>,
    started: Instant,
    hook_event_id: String,
}

/** Read the hook payload from stdin (empty input is Null); malformed JSON on a tool event is an
 * error, so the tool call is denied
 * Input
    - event: HookEvent - lifecycle event
 * Output
    - Result<Value, String>
*/
fn read_payload(event: HookEvent) -> Result<Value, String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(Value::Null);
    }
    let mut input = String::new();
    stdin
        .lock()
        .take(MAX_PAYLOAD)
        .read_to_string(&mut input)
        .map_err(|error| format!("could not read the hook payload: {error}"))?;
    if input.trim().is_empty() {
        return if event.has_action() {
            Err("the hook payload is empty, so the tool call cannot be checked".into())
        } else {
            Ok(Value::Null)
        };
    }
    serde_json::from_str(&input)
        .map_err(|error| format!("the hook payload is not valid JSON: {error}"))
}

/** Run one hook invocation: translate the payload and handle the event, answering fail-closed
 * through the adapter when anything goes wrong
 * Input
    - kind: AgentKind - provider profile
    - event: HookEvent - lifecycle event
 * Output
    - Result<(), String>
    - HOOK_BLOCK error when the provider must block
*/
pub(crate) fn run(kind: AgentKind, event: HookEvent) -> Result<(), String> {
    let started = Instant::now();
    let adapter = adapter(kind);
    let payload = match read_payload(event) {
        Ok(payload) => payload,
        Err(error) => return adapter.respond_failure(event, &error, false),
    };
    let provider = adapter.translate(event, &payload);
    let stop_hook_active = provider.stop_hook_active;
    let setup = (|| -> Result<(Workspace, State, Stores, Option<PathBuf>), String> {
        let workspace = Workspace::locate()?;
        let trust = workspace.trust()?;
        let state = State::load(&workspace)?;
        Ok((
            workspace,
            state,
            Stores::open(&trust.runtime()),
            Some(trust.home),
        ))
    })();
    let (workspace, state, stores, trust_home) = match setup {
        Ok(parts) => parts,
        Err(error) => return adapter.respond_failure(event, &error, stop_hook_active),
    };
    let hook_event_id = format!(
        "hk_{}",
        &digest(
            "hook/invocation/v1",
            &[
                event.name().as_bytes(),
                &now_millis().to_be_bytes(),
                &std::process::id().to_be_bytes()
            ]
        )[..24]
    );
    let hook = Hook {
        adapter,
        event,
        provider,
        workspace,
        state,
        stores,
        trust_home,
        started,
        hook_event_id,
    };
    match hook.handle() {
        Ok(()) => Ok(()),
        Err(error) if error.starts_with("HOOK_BLOCK:") => Err(error),
        Err(error) => {
            let _ = hook.record_failure(&error);
            hook.adapter
                .respond_failure(event, &error, stop_hook_active)
        }
    }
}

impl Hook {
    /** Handle the event
     * Input
        - None (uses self)
     * Output
        - Result<(), String>
    */
    fn handle(&self) -> Result<(), String> {
        let mut session = self.session()?;
        let result = match self.event {
            HookEvent::SessionStart => self.session_start(&mut session),
            HookEvent::UserPromptSubmit => self.prompt(&mut session),
            HookEvent::PreToolUse | HookEvent::PermissionRequest => self.pre_tool(&mut session),
            HookEvent::PostToolUse => self.post_tool(&mut session),
            HookEvent::Stop => self.stop(&mut session),
            HookEvent::SessionEnd => self.session_end(&mut session),
        };
        // A tool call is complete after post-tool use, a session after stop or session end
        match self.event {
            HookEvent::PostToolUse | HookEvent::Stop | HookEvent::SessionEnd => {
                sync::trigger(&self.workspace)
            }
            _ => sync::trigger_if_due(&self.workspace, &self.stores),
        }
        result
    }

    /** Find or create the session, and make sure it has a current task
     * Input
        - None (uses self)
     * Output
        - Result<SessionRecord, String>
    */
    fn session(&self) -> Result<SessionRecord, String> {
        let _lock = self.stores.identity.lock()?;
        let observed = Observed {
            provider: self.adapter.kind().name().into(),
            provider_session_id: self.provider.session.clone(),
            model: self.provider.model.clone(),
            cwd: self.provider.cwd.clone(),
            autonomy_mode: self.state.config.autonomy.mode.clone(),
            registry_generation: self.state.generation(),
        };
        let (mut session, created) = self.stores.identity.resolve_session(
            &self.workspace.repository_id,
            &observed,
            now_millis(),
        );
        if session.current_task.is_none() {
            self.stores.identity.start_task(
                &mut session,
                self.provider.task.clone(),
                None,
                now_millis(),
            )?;
        }
        self.stores.identity.save_session(&session)?;
        if created {
            self.emit(
                &session,
                self.base_event(
                    "session.created",
                    Domain::IdentityProvenance,
                    &session,
                    "created",
                ),
                None,
            )?;
        }
        Ok(session)
    }

    /** Reload a session, change it, and save it under the identity lock, so concurrent hooks of
     * one session never lose an update
     * Input
        - session: &mut SessionRecord - session (refreshed with the saved result)
        - change: impl FnOnce(&mut SessionRecord) - the change
     * Output
        - Result<(), String>
    */
    fn update_session(
        &self,
        session: &mut SessionRecord,
        change: impl FnOnce(&mut SessionRecord),
    ) -> Result<(), String> {
        let _lock = self.stores.identity.lock()?;
        let mut fresh = self
            .stores
            .identity
            .session(&session.foxx_session_id)
            .unwrap_or_else(|| session.clone());
        change(&mut fresh);
        fresh.last_seen_at = now_millis();
        self.stores.identity.save_session(&fresh)?;
        *session = fresh;
        Ok(())
    }

    /** Return the current task of a session
     * Input
        - session: &SessionRecord - session
     * Output
        - Option<TaskRecord>
    */
    fn task(&self, session: &SessionRecord) -> Option<TaskRecord> {
        session
            .current_task
            .as_ref()
            .and_then(|id| self.stores.identity.task(id))
    }

    /** Return the tool call identifier: the provider's tool_use_id, or one derived from the
     * session and the action digest (marked inferred in the event)
     * Input
        - session: &SessionRecord - session
        - action: &AgentAction - tool call
     * Output
        - (String, bool) identifier and whether the provider supplied it
    */
    fn tool_call_id(&self, session: &SessionRecord, action: &AgentAction) -> (String, bool) {
        match &self.provider.tool_call_id {
            Some(id) => (id.clone(), true),
            None => (
                format!(
                    "call_{}",
                    &digest(
                        "hook/tool-call/v1",
                        &[session.foxx_session_id.as_bytes(), action.digest.as_bytes()]
                    )[..24]
                ),
                false,
            ),
        }
    }

    /** Build an event with the session's identity and governance bindings
     * Input
        - event_type: &str - typed name
        - domain: Domain - security domain
        - session: &SessionRecord - session
        - key: &str - idempotency suffix
     * Output
        - Event
    */
    fn base_event(
        &self,
        event_type: &str,
        domain: Domain,
        session: &SessionRecord,
        key: &str,
    ) -> Event {
        let mut event = Event::new(
            event_type,
            domain,
            &self.workspace.repository_id,
            &format!("{}:{event_type}:{key}", session.foxx_session_id),
        );
        event.source = Source::ProviderAdapter;
        let task = self.task(session);
        event.scope = Scope {
            foxx_session_id: Some(session.foxx_session_id.clone()),
            foxx_task_id: task.as_ref().map(|task| task.foxx_task_id.clone()),
            external_task_id: task.as_ref().and_then(|task| task.external_task_id.clone()),
            provider: Some(session.provider.clone()),
            provider_session_id: session.provider_session_id.clone(),
            provider_turn_id: self.provider.turn.clone(),
            provider_task_id: task.as_ref().and_then(|task| task.provider_task_id.clone()),
            agent_instance_id: Some(session.agent_instance_id.clone()),
            model: session.model.clone(),
            tool_call_id: None,
            hook_event_id: Some(self.hook_event_id.clone()),
            trace_id: Some(session.foxx_session_id.clone()),
            span_id: Some(self.hook_event_id.clone()),
            parent_event_id: None,
        };
        event.bindings = Bindings {
            registry_generation: self.state.generation(),
            registry_head: self
                .state
                .registry
                .manifest
                .as_ref()
                .map(|manifest| manifest.head.clone()),
            policies: Vec::new(),
            selections: Vec::new(),
            checkpoint: self.state.config.default_checkpoint.clone(),
            autonomy_mode: Some(session.autonomy_mode.clone()),
            safety: Some(format!("{:?}", session.safety).to_uppercase()),
            zones: Vec::new(),
        };
        event.measurements.push(Measurement::observed(
            "hook_latency_ms",
            self.started.elapsed().as_millis() as f64,
            "ms",
            "hook process wall clock",
        ));
        event.payload =
            json!({"hook_event": self.event.name(), "identity_provenance": session.provenance});
        event
    }

    /** Store an event and raise an alert for it when it is serious enough
     * Input
        - session: &SessionRecord - session
        - event: Event - event to store
        - alert: Option<&str> - alert kind to raise
     * Output
        - Result<(), String>
    */
    fn emit(
        &self,
        session: &SessionRecord,
        event: Event,
        alert: Option<&str>,
    ) -> Result<(), String> {
        let stored = self.stores.events.append(event)?;
        if let (Some(kind), Some(event)) = (alert, stored) {
            alerts::raise(&self.workspace, &self.stores, kind, session, &event);
        }
        Ok(())
    }

    /** Record an internal failure of the hook as a coverage gap
     * Input
        - error: &str - what failed
     * Output
        - Result<(), String>
    */
    fn record_failure(&self, error: &str) -> Result<(), String> {
        let mut event = Event::new(
            "hook.failed",
            Domain::AuditCompliance,
            &self.workspace.repository_id,
            &format!("{}:failed", self.hook_event_id),
        );
        event.severity = Severity::High;
        event.decision = Some(Decision::Error);
        event.enforcement = Enforcement::CoverageGap;
        event.telemetry = Telemetry::Lost;
        event.reasons.push(error.to_string());
        event.scope.hook_event_id = Some(self.hook_event_id.clone());
        event.scope.provider_session_id = self.provider.session.clone();
        event.scope.provider = Some(self.adapter.kind().name().into());
        event.payload = json!({"hook_event": self.event.name(), "fail_closed": true});
        self.stores.events.append(event).map(|_| ())
    }

    /** Path of the per-session tool call correlation file
     * Input
        - session: &SessionRecord - session
     * Output
        - PathBuf
    */
    fn calls_path(&self, session: &SessionRecord) -> PathBuf {
        self.stores
            .directory
            .join("calls")
            .join(format!("{}.json", session.foxx_session_id))
    }

    /** Load, change, and save the correlation state under its lock
     * Input
        - session: &SessionRecord - session
        - change: impl FnOnce(&mut Calls) -> T - the change
     * Output
        - Result<T, String>
    */
    fn with_calls<T>(
        &self,
        session: &SessionRecord,
        change: impl FnOnce(&mut Calls) -> T,
    ) -> Result<T, String> {
        let path = self.calls_path(session);
        let _lock = Lock::acquire(&path.with_extension("lock"))?;
        let mut calls: Calls = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let result = change(&mut calls);
        write_atomic(
            &path,
            serde_json::to_string(&calls).map_err(io_error)?.as_bytes(),
        )?;
        Ok(result)
    }

    /** Available credits of a session
     * Input
        - session: &SessionRecord - session
     * Output
        - Result<u64, String>
    */
    fn available(&self, session: &SessionRecord) -> Result<u64, String> {
        Ok(Ledger::balance_of(&self.stores.ledger.entries()?, &session.foxx_session_id).available)
    }

    /** Apply a ledger operation for the session
     * Input
        - session: &SessionRecord - session
        - tool_call_id: Option<&str> - tool call
        - kind: Kind - operation
        - amount: u64 - amount
        - reason: &str - why
        - key: &str - idempotency key suffix
     * Output
        - Result<u64, String> amount applied
    */
    fn ledger(
        &self,
        session: &SessionRecord,
        tool_call_id: Option<&str>,
        kind: Kind,
        amount: u64,
        reason: &str,
        key: &str,
    ) -> Result<u64, String> {
        let task = session.current_task.clone();
        let (entry, _) = self.stores.ledger.apply(LedgerOperation {
            session: &session.foxx_session_id,
            task: task.as_deref(),
            tool_call_id,
            kind,
            amount,
            reason,
            registry_generation: self.state.generation(),
            idempotency_key: &format!("{}:{key}", session.foxx_session_id),
        })?;
        Ok(entry.amount)
    }

    /** SessionStart: grant credits to a new session, supply policies, selections, and policy
     * context, and record the start or resumption
     * Input
        - session: &mut SessionRecord - session
     * Output
        - Result<(), String>
    */
    fn session_start(&self, session: &mut SessionRecord) -> Result<(), String> {
        let autonomy = &self.state.config.autonomy;
        let granted = self.ledger(
            session,
            None,
            Kind::Grant,
            autonomy.session_credits,
            "session start grant",
            "grant",
        )?;
        let (context, supplied) = self.context_text(session)?;
        self.update_session(session, |session| {
            session.context = supplied.clone();
            if session.ended_at.is_some() {
                session.ended_at = None;
                session.end_reason = None;
            }
        })?;
        let mut event = self.base_event(
            "session.started",
            Domain::IdentityProvenance,
            session,
            &format!("start:{}", self.provider.source.clone().unwrap_or_default()),
        );
        event.measurements.push(Measurement::observed(
            "credits_granted",
            granted as f64,
            "credits",
            "autonomy ledger",
        ));
        event.measurements.push(Measurement::observed(
            "context_files_supplied",
            supplied.len() as f64,
            "count",
            "policy context bindings",
        ));
        event.telemetry = if session.provider_session_id.is_some() {
            Telemetry::Observed
        } else {
            Telemetry::Inferred
        };
        event.payload["source"] = json!(self.provider.source);
        event.payload["context"] = json!(supplied);
        event.evidence = supplied
            .iter()
            .map(|context| format!("context:{}:{}", context.path, &context.digest[..16]))
            .collect();
        self.emit(session, event, None)?;
        self.adapter.respond_context(&context)
    }

    /** Build the session context: the rules of the boundary, every policy with its commands and
     * selections, the policy context files, and the session's autonomy
     * Input
        - session: &SessionRecord - session
     * Output
        - Result<(String, Vec<SuppliedContext>), String> text and the context files supplied
    */
    fn context_text(
        &self,
        session: &SessionRecord,
    ) -> Result<(String, Vec<SuppliedContext>), String> {
        let policies = PolicySet::load(&self.workspace)?;
        let mut text = String::from(
            "Crane governs this repository. Agent changes to .crane (including .crane/map), Crane's hook settings, and Crane's trust directory are denied, even with approval; governance commands (crane protect, target, checkpoint, init, policy changes) are for humans. Do not add, move, or alter '@crane:selection' marker comments.\n",
        );
        let mut supplied = Vec::new();
        for (_, block) in policies.blocks() {
            if block.statements.is_empty() {
                continue;
            }
            text.push_str(&format!("Policy {}:\n", block.name));
            for statement in &block.statements {
                let location = self
                    .state
                    .record(&statement.id)
                    .map(|record| {
                        format!(
                            "{} near lines {}-{}",
                            record.current.span.path,
                            record.current.span.start_line,
                            record.current.span.end_line
                        )
                    })
                    .unwrap_or_else(|| "unregistered".into());
                let meaning = match statement.operation {
                    SelectionOperation::Preserve => "must stay exactly as it is".to_string(),
                    SelectionOperation::Target => format!(
                        "must be changed{}",
                        statement
                            .change_type
                            .map(|change_type| format!(" ({})", change_type.name()))
                            .unwrap_or_default()
                    ),
                };
                text.push_str(&format!(
                    "  - {} {} ({location}): {meaning}\n",
                    statement.operation.name(),
                    statement.id
                ));
            }
            for binding in self.state.context.of(&block.name) {
                if let Ok(bytes) = fs::read(self.workspace.root.join(&binding.path)) {
                    let current = sha512_hex(&bytes);
                    text.push_str(&format!(
                        "  Context ({}{}), guidance only:\n{}\n",
                        binding.path,
                        if current == binding.digest {
                            ""
                        } else {
                            ", changed since it was bound"
                        },
                        String::from_utf8_lossy(&bytes).trim_end()
                    ));
                    supplied.push(SuppliedContext {
                        policy: block.name.clone(),
                        path: binding.path.clone(),
                        digest: current,
                    });
                }
            }
        }
        if let Ok(governance) = crate::governance::zones::load(&self.workspace) {
            use crate::governance::zones::{compose_context, Lifecycle};
            let packs = compose_context(&self.workspace, &governance);
            if !packs.is_empty() {
                text.push_str("Context packs (guidance only, never authorization):\n");
            }
            for (id, version, body) in packs {
                text.push_str(&format!("  [{id} v{version}]\n{body}\n"));
                supplied.push(SuppliedContext {
                    policy: "context-pack".into(),
                    path: format!("pack:{id}@v{version}"),
                    digest: sha512_hex(body.as_bytes()),
                });
            }
            for zone in governance
                .zones
                .iter()
                .filter(|zone| zone.status == Lifecycle::Active)
            {
                text.push_str(&format!(
                    "Zone {} ({:?}, autonomy ceiling {}): {}\n",
                    zone.id,
                    zone.criticality,
                    zone.autonomy_ceiling.name(),
                    zone.description
                ));
            }
            for flow in governance
                .flows
                .iter()
                .filter(|flow| flow.status == Lifecycle::Active)
            {
                text.push_str(&format!(
                    "Flow {} ({:?}, autonomy ceiling {}): {}\n",
                    flow.id,
                    flow.criticality,
                    flow.autonomy_ceiling.name(),
                    flow.description
                ));
            }
        }
        let available = self.available(session)?;
        text.push_str(&format!(
            "Autonomy: mode {}, safety {:?}, {available} credits available. Credits measure risk-bearing authority; when they run out, changes need human approval.\n",
            session.autonomy_mode, session.safety
        ));
        Ok((text, supplied))
    }

    /** UserPromptSubmit: open a new task and flag prompts that look like instruction attacks
     * Input
        - session: &mut SessionRecord - session
     * Output
        - Result<(), String>
    */
    fn prompt(&self, session: &mut SessionRecord) -> Result<(), String> {
        let prompt = self.provider.prompt.clone();
        let task = {
            let _lock = self.stores.identity.lock()?;
            let mut fresh = self
                .stores
                .identity
                .session(&session.foxx_session_id)
                .unwrap_or_else(|| session.clone());
            let task = self.stores.identity.start_task(
                &mut fresh,
                self.provider.task.clone(),
                Some(prompt.as_deref().unwrap_or_default()),
                now_millis(),
            )?;
            self.stores.identity.save_session(&fresh)?;
            *session = fresh;
            task
        };
        let mut event = self.base_event(
            "task.started",
            Domain::IdentityProvenance,
            session,
            &task.foxx_task_id,
        );
        event.payload["prompt_chars"] = json!(task.prompt_chars);
        event.payload["prompt_digest"] = json!(task.prompt_digest);
        event.payload["task_provenance"] = json!(task.provenance);
        event.evidence.push(format!(
            "prompt-sha512:{}",
            task.prompt_digest.clone().unwrap_or_default()
        ));
        self.emit(session, event, None)?;
        let lowered = prompt.unwrap_or_default().to_ascii_lowercase();
        let matched = INJECTION_PHRASES
            .iter()
            .filter(|phrase| lowered.contains(*phrase))
            .copied()
            .collect::<Vec<_>>();
        if !matched.is_empty() {
            let mut event = self.base_event(
                "prompt.suspicious",
                Domain::PromptInjection,
                session,
                &format!("{}:injection", task.foxx_task_id),
            );
            event.severity = Severity::Medium;
            event.assessment = Some(Assessment::Suspicious);
            event.telemetry = Telemetry::Inferred;
            event.reasons.push(format!(
                "the prompt contains instruction-integrity phrases: {}",
                matched.join(", ")
            ));
            self.emit(session, event, Some("prompt_injection"))?;
        }
        Ok(())
    }

    /** PreToolUse and PermissionRequest: decide the tool call, reserve its credits, count bypass
     * attempts (quarantining the session past the threshold), record the decision, and answer
     * Input
        - session: &mut SessionRecord - session
     * Output
        - Result<(), String>
    */
    fn pre_tool(&self, session: &mut SessionRecord) -> Result<(), String> {
        let action = self
            .provider
            .action
            .clone()
            .ok_or("the hook payload has no tool call")?;
        let (call, provided) = self.tool_call_id(session, &action);
        let trust = self.workspace.trust()?;
        let integrity_ok = !integrity(&self.workspace, &self.state, &trust)
            .iter()
            .any(super::super::governance::Finding::blocking);
        let available = self.available(session)?;
        let (limits, zone_error) = match crate::governance::zones::cached_limits(
            &self.workspace,
            &self.state,
            &self.stores.directory,
        ) {
            Ok(limits) => (limits, None),
            Err(error) => (Default::default(), Some(error)),
        };
        let guard = Guard {
            workspace: &self.workspace,
            state: &self.state,
            integrity_ok,
            protected_paths: self.adapter.protected_paths(),
            trust_home: self.trust_home.clone(),
            cwd: self.provider.cwd.clone().map(PathBuf::from),
            mode: session.autonomy_mode.clone(),
            safety: session.safety,
            available: Some(available),
            costs: self.state.config.autonomy.costs.clone(),
            limits: Some(&limits),
            zone_error,
        };
        let mut decided: Decided = guard.decide(&action);
        let mut reserved = 0;
        if decided.verdict.decision == Decision::Allow && decided.verdict.cost > 0 {
            reserved = self.ledger(
                session,
                Some(&call),
                Kind::Reserve,
                decided.verdict.cost,
                &format!("{} {}", action.operation.name(), action.tool),
                &format!("{call}:reserve"),
            )?;
            if reserved < decided.verdict.cost {
                self.ledger(
                    session,
                    Some(&call),
                    Kind::Release,
                    reserved,
                    "insufficient credits",
                    &format!("{call}:release-short"),
                )?;
                reserved = 0;
                decided.verdict.decision = Decision::RequireApproval;
                decided.verdict.reasons.push(
                    "autonomy credits were exhausted by concurrent actions; a human must approve"
                        .into(),
                );
            }
        }
        let threshold = self.state.config.autonomy.quarantine_after_bypass_attempts;
        let mut quarantined_now = false;
        if decided.verdict.bypass {
            self.update_session(session, |session| {
                session.bypass_attempts += 1;
                if session.bypass_attempts >= threshold && session.safety != Safety::Quarantined {
                    session.safety = Safety::Quarantined;
                    session.safety_reason = Some(format!(
                        "{} attempts to bypass Crane",
                        session.bypass_attempts
                    ));
                    quarantined_now = true;
                }
            })?;
        }
        self.with_calls(session, |calls| {
            calls.calls.insert(
                call.clone(),
                CallRecord {
                    at: now_millis(),
                    decision: decided.verdict.decision,
                    cost: decided.verdict.cost,
                    reserved,
                    operation: action.operation.name().into(),
                    tool: action.tool.clone(),
                    selections: decided.selections.clone(),
                },
            );
        })?;
        let domain = if decided.verdict.bypass {
            Domain::PolicyIntegrity
        } else if !decided.network.is_empty() {
            Domain::NetworkExfiltration
        } else {
            Domain::Authorization
        };
        let mut event = self.base_event(
            "tool_call.decided",
            domain,
            session,
            &format!("{call}:{}", self.event.name()),
        );
        if !provided {
            event
                .idempotency_key
                .push_str(&format!(":{}", now_millis()));
        }
        event.scope.tool_call_id = Some(call.clone());
        event.scope.span_id = Some(call.clone());
        event.decision = Some(decided.verdict.decision);
        event.execution =
            (decided.verdict.decision != Decision::Allow).then_some(Execution::NotExecuted);
        event.assessment =
            (decided.verdict.decision != Decision::Allow).then_some(if decided.verdict.bypass {
                Assessment::Suspicious
            } else {
                Assessment::Violation
            });
        event.enforcement = if matches!(
            decided.verdict.decision,
            Decision::Deny | Decision::Quarantine
        ) {
            Enforcement::Prevented
        } else {
            Enforcement::NotApplicable
        };
        event.severity = match decided.verdict.decision {
            Decision::Allow => Severity::Info,
            Decision::RequireApproval => Severity::Low,
            _ if decided.verdict.bypass => Severity::High,
            _ => Severity::Medium,
        };
        event.telemetry = if provided {
            Telemetry::Observed
        } else {
            Telemetry::Inferred
        };
        event.bindings.policies = decided.policies.clone();
        event.bindings.selections = decided.selections.clone();
        event.bindings.zones = decided.zones.clone();
        event.resources = decided.resources.clone();
        event.reasons = decided.verdict.reasons.clone();
        event.measurements.push(Measurement::observed(
            "credits_cost",
            decided.verdict.cost as f64,
            "credits",
            "cost model",
        ));
        event.measurements.push(Measurement::observed(
            "credits_reserved",
            reserved as f64,
            "credits",
            "autonomy ledger",
        ));
        event.measurements.push(Measurement::observed(
            "credits_available_before",
            available as f64,
            "credits",
            "autonomy ledger",
        ));
        event.measurements.push(Measurement::observed(
            "bypass_attempt",
            if decided.verdict.bypass { 1.0 } else { 0.0 },
            "count",
            "pre-action decision",
        ));
        if !decided.network.is_empty() {
            event.measurements.push(Measurement {
                name: "network_destinations".into(),
                value: Some(decided.network.len() as f64),
                unit: "count".into(),
                status: Telemetry::Inferred,
                method: "parsed from the command or tool arguments; not observed on the network"
                    .into(),
            });
        }
        event.measurements.push(Measurement::missing(
            "network_bytes_sent",
            "bytes",
            Telemetry::Unsupported,
            "Crane does not observe the network stack",
        ));
        event.payload["tool"] = json!(action.tool);
        event.payload["operation"] = json!(action.operation.name());
        event.payload["input_digest"] = json!(action.digest);
        event.payload["network"] = json!(decided.network);
        event.payload["tool_call_id_provenance"] =
            json!(if provided { "provider" } else { "generated" });
        let alert = if decided.verdict.bypass {
            Some("bypass_attempt")
        } else {
            None
        };
        self.emit(session, event, alert)?;
        if quarantined_now {
            let mut event = self.base_event(
                "session.quarantined",
                Domain::RecoveryOversight,
                session,
                "quarantined",
            );
            event.severity = Severity::Critical;
            event.decision = Some(Decision::Quarantine);
            event
                .reasons
                .push(session.safety_reason.clone().unwrap_or_default());
            self.emit(session, event, Some("quarantine"))?;
        }
        if decided.verdict.decision == Decision::Allow {
            let remaining = available.saturating_sub(reserved);
            if remaining <= self.state.config.autonomy.low_credit_threshold
                && available > self.state.config.autonomy.low_credit_threshold
            {
                let mut event = self.base_event(
                    "credits.low",
                    Domain::ResourceAbuse,
                    session,
                    &format!("{call}:low"),
                );
                event.severity = Severity::Medium;
                event.measurements.push(Measurement::observed(
                    "credits_available",
                    remaining as f64,
                    "credits",
                    "autonomy ledger",
                ));
                self.emit(session, event, Some("low_credits"))?;
            }
        }
        let verdict = Verdict { ..decided.verdict };
        if self.event == HookEvent::PermissionRequest {
            self.adapter.respond_permission(&verdict)
        } else {
            self.adapter.respond_action(&verdict)
        }
    }

    /** PostToolUse: settle the reservation, verify the actual effects (written files, or the
     * whole work tree after a shell or other tool), attribute new violations to this tool call
     * (a protected change that did not pass a write check is a confirmed bypass), degrade or
     * quarantine the session, and tell the agent how to repair
     * Input
        - session: &mut SessionRecord - session
     * Output
        - Result<(), String>
    */
    fn post_tool(&self, session: &mut SessionRecord) -> Result<(), String> {
        let action = self
            .provider
            .action
            .clone()
            .ok_or("the hook payload has no tool call")?;
        let (call, provided) = self.tool_call_id(session, &action);
        let record = self.with_calls(session, |calls| calls.calls.get(&call).cloned())?;
        let failed = self.provider.tool_failed;
        if let Some(record) = &record {
            if record.reserved > 0 {
                if failed == Some(true) && action.operation == Operation::Write {
                    self.ledger(
                        session,
                        Some(&call),
                        Kind::Release,
                        record.reserved,
                        "the write failed",
                        &format!("{call}:release"),
                    )?;
                } else {
                    self.ledger(
                        session,
                        Some(&call),
                        Kind::Consume,
                        record.reserved,
                        "the action ran",
                        &format!("{call}:consume"),
                    )?;
                }
            }
        } else {
            let mut event = self.base_event(
                "hook.missing_decision",
                Domain::AuditCompliance,
                session,
                &format!("{call}:missing"),
            );
            event.scope.tool_call_id = Some(call.clone());
            event.severity = Severity::High;
            event.enforcement = Enforcement::CoverageGap;
            event.telemetry = Telemetry::Unobserved;
            event.reasons.push(format!("{} ran without a recorded pre-tool authorization (hook missing, failed, or the tool bypassed it)", action.tool));
            self.emit(session, event, Some("coverage_gap"))?;
        }
        let scan = match action.operation {
            Operation::Write => ScanMode::Paths(
                action
                    .files
                    .iter()
                    .filter_map(|change| {
                        repository_path(
                            &self.workspace,
                            self.provider.cwd.as_deref().map(std::path::Path::new),
                            &change.path,
                        )
                    })
                    .collect(),
            ),
            Operation::Read => ScanMode::Paths(Vec::new()),
            _ => ScanMode::Full,
        };
        let (report, state) = evaluate(
            &self.workspace,
            &Options {
                scan,
                targets_pending: true,
                candidates: false,
                governance: false,
            },
        )?;
        let violations = violations(&report);
        let new = self.with_calls(session, |calls| {
            let new = violations
                .keys()
                .filter(|key| !calls.known_violations.contains(*key))
                .cloned()
                .collect::<Vec<_>>();
            calls.known_violations = violations.keys().cloned().collect();
            new
        })?;
        let mut event = self.base_event(
            "tool_call.verified",
            Domain::PolicyIntegrity,
            session,
            &format!("{call}:post"),
        );
        if !provided {
            event
                .idempotency_key
                .push_str(&format!(":{}", now_millis()));
        }
        event.scope.tool_call_id = Some(call.clone());
        event.scope.span_id = Some(call.clone());
        event.decision = record.as_ref().map(|record| record.decision);
        event.execution = Some(match failed {
            Some(true) => Execution::Failure,
            Some(false) => Execution::Success,
            None => Execution::Unknown,
        });
        if let Some(record) = &record {
            event.measurements.push(Measurement::observed(
                "tool_duration_ms",
                now_millis().saturating_sub(record.at) as f64,
                "ms",
                "pre-tool to post-tool hook interval",
            ));
        } else {
            event.measurements.push(Measurement::missing(
                "tool_duration_ms",
                "ms",
                Telemetry::Unobserved,
                "no pre-tool decision was recorded",
            ));
        }
        event.measurements.push(Measurement::observed(
            "selections_verified",
            report.resolutions.len() as f64,
            "count",
            "post-action reconciliation",
        ));
        event.measurements.push(Measurement::observed(
            "files_scanned",
            report.files_scanned as f64,
            "count",
            "post-action reconciliation",
        ));
        event.measurements.push(Measurement::missing(
            "cpu_ms",
            "ms",
            Telemetry::Unsupported,
            "process resource usage is not instrumented",
        ));
        event.measurements.push(Measurement::missing(
            "memory_bytes",
            "bytes",
            Telemetry::Unsupported,
            "process resource usage is not instrumented",
        ));
        event.payload["tool"] = json!(action.tool);
        event.payload["operation"] = json!(action.operation.name());
        if new.is_empty() {
            event.assessment = Some(if violations.is_empty() {
                Assessment::Compliant
            } else {
                Assessment::Unresolved
            });
            if !violations.is_empty() {
                event
                    .reasons
                    .push("violations that existed before this tool call remain".into());
            }
            self.emit(session, event, None)?;
            let summary = if violations.is_empty() {
                String::new()
            } else {
                format!(
                    "{} pre-existing Crane violation(s) remain; a human must repair them",
                    violations.len()
                )
            };
            return self.adapter.respond_feedback(
                HookEvent::PostToolUse,
                &Feedback {
                    block: false,
                    summary,
                    detail: String::new(),
                },
            );
        }
        let bypass = action.operation != Operation::Write;
        event.assessment = Some(Assessment::Violation);
        event.enforcement = if bypass {
            Enforcement::BypassConfirmed
        } else {
            Enforcement::DetectedAfterEffect
        };
        event.severity = if bypass {
            Severity::Critical
        } else {
            Severity::High
        };
        event.reasons = new.iter().map(|key| violations[key].clone()).collect();
        event.bindings.selections = new
            .iter()
            .filter_map(|key| key.strip_prefix("selection:").map(String::from))
            .collect();
        let penalty = self.state.config.autonomy.costs.violation_penalty;
        let revoked = self.ledger(
            session,
            Some(&call),
            Kind::Revoke,
            penalty,
            "violation detected after the effect",
            &format!("{call}:revoke"),
        )?;
        event.measurements.push(Measurement::observed(
            "credits_revoked",
            revoked as f64,
            "credits",
            "autonomy ledger",
        ));
        self.update_session(session, |session| {
            session.violations += 1;
            let target = if bypass {
                Safety::Quarantined
            } else {
                Safety::Degraded
            };
            if target > session.safety {
                session.safety = target;
                session.safety_reason = Some(event.reasons.join("; "));
            }
        })?;
        self.emit(
            session,
            event,
            Some(if bypass {
                "bypass_confirmed"
            } else {
                "violation"
            }),
        )?;
        let detail = repair_instructions(&self.workspace, &state, &report, &new, &violations);
        self.adapter.respond_feedback(
            HookEvent::PostToolUse,
            &Feedback {
                block: true,
                summary: format!(
                    "Crane detected {} violation(s) caused by this {} call{}; restore the protected code",
                    new.len(),
                    action.tool,
                    if bypass { " (protected code changed outside the edit tools: confirmed bypass, session quarantined)" } else { "" }
                ),
                detail,
            },
        )
    }

    /** Stop: reconcile completely; a failing policy or unresolved selection blocks the first stop
     * so the agent repairs or completes its targets, and is let through (recorded unverified)
     * the second time so the agent cannot loop
     * Input
        - session: &mut SessionRecord - session
     * Output
        - Result<(), String>
    */
    fn stop(&self, session: &mut SessionRecord) -> Result<(), String> {
        let (report, state) = evaluate(
            &self.workspace,
            &Options {
                governance: false,
                ..Options::default()
            },
        )?;
        let failures = report.failures();
        let blocking = report
            .findings
            .iter()
            .filter(|finding| finding.blocking())
            .collect::<Vec<_>>();
        let task = self.task(session);
        let passed = failures.is_empty() && blocking.is_empty();
        let status = if passed {
            "COMPLETED"
        } else if self.provider.stop_hook_active {
            "ENDED"
        } else {
            "BLOCKED"
        };
        if let Some(mut task) = task.clone() {
            task.status = status.into();
            if status != "BLOCKED" {
                task.ended_at = Some(now_millis());
            }
            self.stores.identity.save_task(&task)?;
        }
        let mut event = self.base_event(
            if passed {
                "task.verified"
            } else {
                "task.verification_failed"
            },
            Domain::PolicyIntegrity,
            session,
            &format!(
                "{}:stop:{}",
                task.as_ref()
                    .map(|task| task.foxx_task_id.clone())
                    .unwrap_or_default(),
                now_millis()
            ),
        );
        event.assessment = Some(if passed {
            Assessment::Compliant
        } else if failures
            .iter()
            .any(|failure| failure.outcome == Outcome::Unresolved)
        {
            Assessment::Unresolved
        } else {
            Assessment::Violation
        });
        event.severity = if passed {
            Severity::Info
        } else {
            Severity::High
        };
        event.reasons = failures
            .iter()
            .map(|failure| format!("{}: {}", failure.policy, failure.message))
            .chain(blocking.iter().map(|finding| finding.message.clone()))
            .collect();
        event.measurements.push(Measurement::observed(
            "policies_passed",
            report
                .policies
                .iter()
                .filter(|policy| policy.passed)
                .count() as f64,
            "count",
            "full reconciliation",
        ));
        event.measurements.push(Measurement::observed(
            "policies_failed",
            report
                .policies
                .iter()
                .filter(|policy| !policy.passed)
                .count() as f64,
            "count",
            "full reconciliation",
        ));
        event.payload["task_status"] = json!(status);
        self.emit(session, event, (!passed).then_some("verification_failed"))?;
        if passed {
            return self.adapter.respond_feedback(
                HookEvent::Stop,
                &Feedback {
                    block: false,
                    summary: String::new(),
                    detail: String::new(),
                },
            );
        }
        let mut detail = String::new();
        for failure in &failures {
            detail.push_str(&format!(
                "- {} {} ({}): {}\n",
                failure.operation.name(),
                failure.id,
                failure.policy,
                failure.message
            ));
        }
        for finding in &blocking {
            detail.push_str(&format!("- {}\n", finding.message));
        }
        let violation_keys = violations(&report);
        let keys = violation_keys.keys().cloned().collect::<Vec<_>>();
        detail.push_str(&repair_instructions(
            &self.workspace,
            &state,
            &report,
            &keys,
            &violation_keys,
        ));
        self.adapter.respond_feedback(
            HookEvent::Stop,
            &Feedback {
                block: !self.provider.stop_hook_active,
                summary: "Crane policies are not satisfied; repair preserved code and complete the targets before finishing".into(),
                detail,
            },
        )
    }

    /** SessionEnd: release reservations without a post-tool settlement (recorded as coverage
     * gaps), expire unused credits, close the session and task, and record a signed attestation
     * over the session's events
     * Input
        - session: &mut SessionRecord - session
     * Output
        - Result<(), String>
    */
    fn session_end(&self, session: &mut SessionRecord) -> Result<(), String> {
        let open = self.with_calls(session, |calls| calls.calls.clone())?;
        let entries = self.stores.ledger.entries()?;
        for (call, record) in open.iter().filter(|(_, record)| record.reserved > 0) {
            let settled = entries.iter().any(|entry| {
                entry.tool_call_id.as_deref() == Some(call.as_str())
                    && matches!(entry.kind, Kind::Consume | Kind::Release)
            });
            if !settled {
                self.ledger(
                    session,
                    Some(call),
                    Kind::Release,
                    record.reserved,
                    "no post-tool settlement before the session ended",
                    &format!("{call}:release-end"),
                )?;
                let mut event = self.base_event(
                    "tool_call.unsettled",
                    Domain::AuditCompliance,
                    session,
                    &format!("{call}:unsettled"),
                );
                event.scope.tool_call_id = Some(call.clone());
                event.enforcement = Enforcement::CoverageGap;
                event.telemetry = Telemetry::Unobserved;
                event.reasons.push(format!(
                    "{} was authorized but its effect was never reported",
                    record.tool
                ));
                self.emit(session, event, None)?;
            }
        }
        let available = self.available(session)?;
        let expired = self.ledger(
            session,
            None,
            Kind::Expire,
            available,
            "session ended",
            "expire",
        )?;
        if let Some(mut task) = self.task(session) {
            if task.status == "RUNNING" {
                task.status = "ENDED".into();
                task.ended_at = Some(now_millis());
                self.stores.identity.save_task(&task)?;
            }
        }
        self.update_session(session, |session| {
            session.ended_at = Some(now_millis());
            session.end_reason = self.provider.end_reason.clone();
        })?;
        let events = self
            .stores
            .events
            .read_all()?
            .into_iter()
            .filter(|event| {
                event.scope.foxx_session_id.as_deref() == Some(session.foxx_session_id.as_str())
            })
            .collect::<Vec<_>>();
        let digests = events
            .iter()
            .map(|event| event.digest.clone())
            .collect::<Vec<_>>();
        let attestation = digest(
            "telemetry/session-attestation/v1",
            &[
                session.foxx_session_id.as_bytes(),
                digests.join("\n").as_bytes(),
            ],
        );
        let signature = self.workspace.trust()?.signing_key()?.map(|key| {
            (
                key.public_hex(),
                key.sign("telemetry/session-attestation/v1", attestation.as_bytes()),
            )
        });
        let mut event = self.base_event(
            "session.attested",
            Domain::AuditCompliance,
            session,
            "attested",
        );
        event.measurements.push(Measurement::observed(
            "credits_expired",
            expired as f64,
            "credits",
            "autonomy ledger",
        ));
        event.measurements.push(Measurement::observed(
            "events_attested",
            digests.len() as f64,
            "count",
            "session event chain",
        ));
        event.telemetry = if signature.is_some() {
            Telemetry::Observed
        } else {
            Telemetry::Unsupported
        };
        event
            .evidence
            .push(format!("attestation-sha512:{attestation}"));
        event.payload["attestation"] = json!({
            "digest": attestation,
            "events": digests.len(),
            "public_key": signature.as_ref().map(|(key, _)| key.clone()),
            "signature": signature.as_ref().map(|(_, signature)| signature.clone()),
            "signed": signature.is_some(),
        });
        self.emit(session, event, None)
    }
}

/** Collect the violations in a report, keyed so the same violation is recognized across tool
 * calls: failing or unresolved commands by selection, and blocking findings by code and subject
 * Input
    - report: &Report - verification report
 * Output
    - BTreeMap<String, String> key to explanation
*/
fn violations(report: &Report) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    for policy in &report.policies {
        for command in &policy.commands {
            let failing = match command.outcome {
                Outcome::Fail => command.operation == SelectionOperation::Preserve,
                Outcome::Unresolved => true,
                Outcome::Pass | Outcome::Pending => false,
            };
            if failing {
                found.insert(
                    format!("selection:{}", command.id),
                    format!(
                        "{} {} ({}): {}",
                        command.operation.name(),
                        command.id,
                        policy.name,
                        command.message
                    ),
                );
            }
        }
    }
    for finding in report.findings.iter().filter(|finding| finding.blocking()) {
        let subject = finding
            .selection
            .clone()
            .or(finding.path.clone())
            .unwrap_or_default();
        found
            .entry(format!("finding:{}:{subject}", finding.code))
            .or_insert_with(|| finding.message.clone());
    }
    found
}

/** Explain how to repair violations: for a changed preserved selection, the baseline content to
 * restore between its markers (from the checkpoint); for marker problems, how to restore markers
 * Input
    - workspace: &Workspace - repository
    - state: &State - governance state
    - report: &Report - verification report
    - keys: &[String] - violation keys to explain
    - violations: &BTreeMap<String, String> - all violations
 * Output
    - String
*/
fn repair_instructions(
    workspace: &Workspace,
    state: &State,
    report: &Report,
    keys: &[String],
    violations: &BTreeMap<String, String>,
) -> String {
    let mut text = String::new();
    for key in keys {
        text.push_str(&format!(
            "- {}\n",
            violations.get(key).cloned().unwrap_or_default()
        ));
        let Some(id) = key.strip_prefix("selection:") else {
            continue;
        };
        let Some(record) = state.record(id) else {
            continue;
        };
        let resolved = report.resolutions.get(id).cloned().flatten();
        if record.operation == SelectionOperation::Preserve {
            if let (Ok(content), Some(span)) = (baseline(workspace, record), resolved) {
                let excerpt: String = content.chars().take(MAX_RESTORE_EXCERPT).collect();
                text.push_str(&format!(
                    "  Restore the content between the markers of {id} in {} to exactly this (from checkpoint {}):\n{excerpt}{}\n",
                    span.path,
                    record.origin.checkpoint,
                    if excerpt.len() < content.len() { "\n  [truncated]" } else { "" }
                ));
                continue;
            }
        }
        text.push_str(&format!(
            "  Selection {id} must be enclosed again by its exact markers in {} (do not create, copy, or move markers); a human resolves it with crane validate if it cannot be restored.\n",
            record.current.span.path
        ));
    }
    text.push_str(&format!(
        "Verification report:\n{}\n",
        serde_json::to_string(&json!({"findings": report.findings, "policies": report.policies}))
            .unwrap_or_default()
    ));
    text
}
