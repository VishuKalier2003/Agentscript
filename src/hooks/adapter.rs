// Agent adapters. An adapter only translates its provider's hook payloads into provider-neutral
// actions and renders Crane's answers in the provider's protocol; every decision is made by the
// provider-neutral enforcement engine. Claude Code and Codex are supported; the generic profile
// reads Crane's neutral action JSON.

use std::path::Path;

use serde_json::{json, Value};

use super::action::{AgentAction, FileChange, Operation, Proposed, TextEdit};
use crate::telemetry::model::Decision;
use crate::trust::crypto::sha512_hex;

/** The agent profile selected with --profile
 * Variants
    - Generic - any agent host, using Crane's neutral action format
    - Claude - Claude Code
    - Codex - Codex
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentKind {
    Generic,
    Claude,
    Codex,
}

impl AgentKind {
    /** Parse a profile name (case-insensitive; claude-code is accepted for claude)
     * Input
        - value: &str - profile name
     * Output
        - Result<AgentKind, String>
        - Error if the profile is unknown
    */
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "generic" | "universal" => Ok(Self::Generic),
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            other => Err(format!(
                "unsupported agent profile '{other}'; expected claude or codex"
            )),
        }
    }

    /** Return the profile name
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /** Return the provider's display name
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn display(self) -> &'static str {
        match self {
            Self::Generic => "generic agent",
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

/** A lifecycle event reported by an agent host
 * Variants
    - SessionStart - a session starts or resumes
    - UserPromptSubmit - the user sent a prompt (a new task)
    - PreToolUse - a tool is about to run: authorize it
    - PostToolUse - a tool ran: verify its effects
    - PermissionRequest - the host is about to ask the user to approve a tool
    - Stop - the agent wants to finish: reconcile
    - SessionEnd - the session ended
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PermissionRequest,
    Stop,
    SessionEnd,
}

impl HookEvent {
    /** Every event */
    pub(crate) const ALL: [HookEvent; 7] = [
        Self::SessionStart,
        Self::UserPromptSubmit,
        Self::PreToolUse,
        Self::PostToolUse,
        Self::PermissionRequest,
        Self::Stop,
        Self::SessionEnd,
    ];

    /** Parse an event name: the command-line name (pre-tool-use) or the host's (PreToolUse)
     * Input
        - value: &str - event name
     * Output
        - Result<HookEvent, String>
        - Error listing the valid names
    */
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|event| event.name() == value || event.host_name() == value)
            .ok_or_else(|| format!("unknown hook event '{value}'"))
    }

    /** Return the command-line name
     * Input
        - None (uses self)
     * Output
        - &'static str such as "pre-tool-use"
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::UserPromptSubmit => "user-prompt-submit",
            Self::PreToolUse => "pre-tool-use",
            Self::PostToolUse => "post-tool-use",
            Self::PermissionRequest => "permission-request",
            Self::Stop => "stop",
            Self::SessionEnd => "session-end",
        }
    }

    /** Return the host's event name
     * Input
        - None (uses self)
     * Output
        - &'static str such as "PreToolUse"
    */
    pub(crate) fn host_name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::Stop => "Stop",
            Self::SessionEnd => "SessionEnd",
        }
    }

    /** Report whether the event carries a tool call
     * Input
        - None (uses self)
     * Output
        - bool
    */
    pub(crate) fn has_action(self) -> bool {
        matches!(
            self,
            Self::PreToolUse | Self::PostToolUse | Self::PermissionRequest
        )
    }
}

/** A provider event translated into Crane's terms
 * Fields
    - session: Option<String> - provider session id, None when not reported
    - turn: Option<String> - provider turn id
    - task: Option<String> - provider task id
    - tool_call_id: Option<String> - provider tool_use_id
    - action: Option<AgentAction> - the tool call, for tool events
    - stop_hook_active: bool - the agent continues because a stop was blocked
    - model: Option<String> - model reported
    - source: Option<String> - why the session started
    - cwd: Option<String> - agent working directory
    - prompt: Option<String> - prompt text (digested, never stored)
    - tool_failed: Option<bool> - whether the tool reported a failure (post-tool-use)
    - end_reason: Option<String> - why the session ended
*/
#[derive(Debug, Clone, Default)]
pub(crate) struct ProviderEvent {
    pub(crate) session: Option<String>,
    pub(crate) turn: Option<String>,
    pub(crate) task: Option<String>,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) action: Option<AgentAction>,
    pub(crate) stop_hook_active: bool,
    pub(crate) model: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) prompt: Option<String>,
    pub(crate) tool_failed: Option<bool>,
    pub(crate) end_reason: Option<String>,
}

/** Build a provider event from the fields the hook payloads share
 * Input
    - payload: &Value - hook JSON
    - action: Option<AgentAction> - translated tool call
 * Output
    - ProviderEvent
*/
fn provider_event(payload: &Value, action: Option<AgentAction>) -> ProviderEvent {
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(String::from)
    };
    let response = payload.get("tool_response");
    let tool_failed = response.map(|response| {
        response.get("success") == Some(&Value::Bool(false))
            || response.get("is_error") == Some(&Value::Bool(true))
            || response.get("interrupted") == Some(&Value::Bool(true))
            || response
                .get("exit_code")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0)
    });
    ProviderEvent {
        session: text("session_id"),
        turn: text("turn_id"),
        task: text("task_id"),
        tool_call_id: text("tool_use_id").or_else(|| text("call_id")),
        action,
        stop_hook_active: payload
            .get("stop_hook_active")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        model: text("model").or_else(|| {
            payload
                .get("model")
                .and_then(|model| model.get("id"))
                .and_then(Value::as_str)
                .map(String::from)
        }),
        source: text("source"),
        cwd: text("cwd"),
        prompt: text("prompt"),
        tool_failed,
        end_reason: text("reason"),
    }
}

/** A decision with its explanation
 * Fields
    - decision: Decision - authority decision
    - reasons: Vec<String> - why
    - bypass: bool - the action tried to bypass Crane (metadata, trust, markers, commands)
    - cost: u64 - autonomy credits the action costs
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) decision: Decision,
    pub(crate) reasons: Vec<String>,
    pub(crate) bypass: bool,
    pub(crate) cost: u64,
}

/** What Crane tells the agent after verification
 * Fields
    - block: bool - the agent must repair before continuing (or before finishing)
    - summary: String - one-line summary
    - detail: String - full explanation (JSON report or instructions)
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Feedback {
    pub(crate) block: bool,
    pub(crate) summary: String,
    pub(crate) detail: String,
}

/** Where a provider keeps its hook configuration and which events Crane registers there
 * Fields
    - file: &'static str - repository-relative configuration file
    - events: &'static [(HookEvent, bool)] - registered events, and whether each needs a matcher
    - deny: &'static [&'static str] - provider permission rules Crane adds
    - timeout: u64 - seconds the provider waits for a hook
*/
pub(crate) struct HookConfig {
    pub(crate) file: &'static str,
    pub(crate) events: &'static [(HookEvent, bool)],
    pub(crate) deny: &'static [&'static str],
    pub(crate) timeout: u64,
}

/** The interface of an agent integration */
pub(crate) trait AgentAdapter {
    /** Report the profile this adapter serves
     * Input
        - None (uses self)
     * Output
        - AgentKind
    */
    fn kind(&self) -> AgentKind;

    /** List the provider files that configure Crane's hooks (protected like .crane)
     * Input
        - None (uses self)
     * Output
        - &'static [&'static str] lowercase paths with "/" separators
    */
    fn protected_paths(&self) -> &'static [&'static str] {
        &[]
    }

    /** Translate a hook payload; the default reads Crane's neutral action JSON
     * Input
        - event: HookEvent - lifecycle event
        - payload: &Value - hook payload (Null when empty)
     * Output
        - ProviderEvent
    */
    fn translate(&self, event: HookEvent, payload: &Value) -> ProviderEvent {
        neutral_event(event, payload)
    }

    /** Answer a pre-tool-use decision; by default print it as JSON and block unless allowed
     * Input
        - verdict: &Verdict - decision
     * Output
        - Result<(), String>
        - HOOK_BLOCK error unless the decision is Allow
    */
    fn respond_action(&self, verdict: &Verdict) -> Result<(), String> {
        println!(
            "{}",
            json!({"decision": verdict.decision, "reasons": verdict.reasons})
        );
        match verdict.decision {
            Decision::Allow => Ok(()),
            _ => Err(format!("HOOK_BLOCK:Crane: {}", verdict.reasons.join("; "))),
        }
    }

    /** Answer a permission request; by default the same as respond_action
     * Input
        - verdict: &Verdict - decision
     * Output
        - Result<(), String>
    */
    fn respond_permission(&self, verdict: &Verdict) -> Result<(), String> {
        self.respond_action(verdict)
    }

    /** Answer after verification; by default print the detail and block when required
     * Input
        - event: HookEvent - lifecycle event
        - feedback: &Feedback - what to tell the agent
     * Output
        - Result<(), String>
        - HOOK_BLOCK error when the agent must repair
    */
    fn respond_feedback(&self, _event: HookEvent, feedback: &Feedback) -> Result<(), String> {
        if !feedback.detail.is_empty() {
            println!("{}", feedback.detail);
        }
        if feedback.block {
            Err(format!("HOOK_BLOCK:{}", feedback.summary))
        } else {
            Ok(())
        }
    }

    /** Supply context to the agent at session start; by default print it
     * Input
        - context: &str - policies, selections, and policy context
     * Output
        - Result<(), String>
    */
    fn respond_context(&self, context: &str) -> Result<(), String> {
        println!("{context}");
        Ok(())
    }

    /** Answer an event for which no decision or verification could be obtained: never as an
     * allow or a clean result (fail closed); a held stop is let through the second time so the
     * agent cannot loop forever
     * Input
        - event: HookEvent - lifecycle event
        - error: &str - why
        - stop_hook_active: bool - the agent continues after a held stop
     * Output
        - Result<(), String>
        - HOOK_BLOCK error when the provider must block
    */
    fn respond_failure(
        &self,
        event: HookEvent,
        error: &str,
        stop_hook_active: bool,
    ) -> Result<(), String> {
        let error = error.strip_prefix("HOOK_BLOCK:").unwrap_or(error);
        match event {
            HookEvent::PreToolUse | HookEvent::PermissionRequest => Err(format!(
                "HOOK_BLOCK:Crane could not authorize this tool call, so it is denied: {error}"
            )),
            HookEvent::PostToolUse => Err(format!(
                "HOOK_BLOCK:Crane could not verify this tool call, so it is not known to be compliant: {error}"
            )),
            HookEvent::Stop if !stop_hook_active => Err(format!(
                "HOOK_BLOCK:Crane could not reconcile the session, so the work is not verified: {error}"
            )),
            HookEvent::SessionStart => Err(format!("Crane could not start the session: {error}")),
            _ => {
                eprintln!("Crane: {error}");
                Ok(())
            }
        }
    }

    /** Describe the provider's hook configuration, None when Crane cannot install into it
     * Input
        - None (uses self)
     * Output
        - Option<HookConfig>
    */
    fn hook_config(&self) -> Option<HookConfig> {
        None
    }

    /** Return the command the provider runs for an event
     * Input
        - event: HookEvent - lifecycle event
     * Output
        - String
    */
    fn hook_command(&self, event: HookEvent) -> String {
        format!(
            "crane agent hook --event {} --profile {}",
            event.name(),
            self.kind().name()
        )
    }
}

/** Adapter for the generic profile (Crane's neutral action JSON)
 * Fields
    - None
*/
pub(crate) struct GenericAdapter;

/** Adapter for Claude Code
 * Fields
    - None
*/
pub(crate) struct ClaudeAdapter;

/** Adapter for Codex
 * Fields
    - None
*/
pub(crate) struct CodexAdapter;

impl AgentAdapter for GenericAdapter {
    /** Report the generic profile
     * Input
        - None (uses self)
     * Output
        - AgentKind::Generic
    */
    fn kind(&self) -> AgentKind {
        AgentKind::Generic
    }
}

impl AgentAdapter for ClaudeAdapter {
    /** Report the Claude profile
     * Input
        - None (uses self)
     * Output
        - AgentKind::Claude
    */
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    /** Protect the Claude Code settings files that register Crane's hooks
     * Input
        - None (uses self)
     * Output
        - &'static [&'static str]
    */
    fn protected_paths(&self) -> &'static [&'static str] {
        &[".claude/settings.json", ".claude/settings.local.json"]
    }

    /** Translate a Claude Code hook payload
     * Input
        - event: HookEvent - lifecycle event
        - payload: &Value - Claude Code hook JSON
     * Output
        - ProviderEvent
    */
    fn translate(&self, event: HookEvent, payload: &Value) -> ProviderEvent {
        provider_event(payload, event.has_action().then(|| claude_action(payload)))
    }

    /** Describe Claude Code's project-local hook configuration
     * Input
        - None (uses self)
     * Output
        - Option<HookConfig>
    */
    fn hook_config(&self) -> Option<HookConfig> {
        Some(HookConfig {
            file: ".claude/settings.local.json",
            events: &[
                (HookEvent::SessionStart, false),
                (HookEvent::UserPromptSubmit, false),
                (HookEvent::PreToolUse, true),
                (HookEvent::PostToolUse, true),
                (HookEvent::Stop, false),
                (HookEvent::SessionEnd, false),
            ],
            deny: &[
                "Edit(./.crane/**)",
                "Write(./.crane/**)",
                "Edit(./.claude/settings.json)",
                "Edit(./.claude/settings.local.json)",
            ],
            timeout: 120,
        })
    }

    /** Answer PreToolUse the Claude Code way: silence to allow, exit code 2 with the reason to
     * deny (a denial is final; "ask" is never offered for a denied action), and permissionDecision
     * "ask" when approval is required
     * Input
        - verdict: &Verdict - decision
     * Output
        - Result<(), String>
    */
    fn respond_action(&self, verdict: &Verdict) -> Result<(), String> {
        let reasons = verdict.reasons.join("; ");
        match verdict.decision {
            Decision::Allow => Ok(()),
            Decision::RequireApproval => {
                println!(
                    "{}",
                    json!({"hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "ask",
                        "permissionDecisionReason": format!("Crane requires approval: {reasons}"),
                    }})
                );
                Ok(())
            }
            _ => Err(format!("HOOK_BLOCK:Crane denied this tool call: {reasons}")),
        }
    }

    /** Answer after verification the Claude Code way: exit code 2 with the detail to block, a
     * systemMessage (and additionalContext where supported) otherwise
     * Input
        - event: HookEvent - lifecycle event
        - feedback: &Feedback - what to tell the agent
     * Output
        - Result<(), String>
    */
    fn respond_feedback(&self, event: HookEvent, feedback: &Feedback) -> Result<(), String> {
        if feedback.block {
            return Err(format!(
                "HOOK_BLOCK:{}\n{}",
                feedback.summary, feedback.detail
            ));
        }
        if feedback.summary.is_empty() {
            return Ok(());
        }
        let mut output = json!({"systemMessage": format!("Crane: {}", feedback.summary)});
        if matches!(event, HookEvent::PostToolUse | HookEvent::UserPromptSubmit)
            && !feedback.detail.is_empty()
        {
            output["hookSpecificOutput"] =
                json!({"hookEventName": event.host_name(), "additionalContext": feedback.detail});
        }
        println!("{output}");
        Ok(())
    }

    /** Supply session context as SessionStart additionalContext
     * Input
        - context: &str - context text
     * Output
        - Result<(), String>
    */
    fn respond_context(&self, context: &str) -> Result<(), String> {
        println!(
            "{}",
            json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": context}})
        );
        Ok(())
    }
}

impl AgentAdapter for CodexAdapter {
    /** Report the Codex profile
     * Input
        - None (uses self)
     * Output
        - AgentKind::Codex
    */
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    /** Protect the Codex files that register Crane's hooks
     * Input
        - None (uses self)
     * Output
        - &'static [&'static str]
    */
    fn protected_paths(&self) -> &'static [&'static str] {
        &[".codex/hooks.json", ".codex/config.toml"]
    }

    /** Translate a Codex hook payload
     * Input
        - event: HookEvent - lifecycle event
        - payload: &Value - Codex hook JSON
     * Output
        - ProviderEvent
    */
    fn translate(&self, event: HookEvent, payload: &Value) -> ProviderEvent {
        provider_event(payload, event.has_action().then(|| codex_action(payload)))
    }

    /** Describe Codex's project hook configuration
     * Input
        - None (uses self)
     * Output
        - Option<HookConfig>
    */
    fn hook_config(&self) -> Option<HookConfig> {
        Some(HookConfig {
            file: ".codex/hooks.json",
            events: &[
                (HookEvent::SessionStart, false),
                (HookEvent::UserPromptSubmit, false),
                (HookEvent::PreToolUse, true),
                (HookEvent::PermissionRequest, true),
                (HookEvent::PostToolUse, true),
                (HookEvent::Stop, false),
                (HookEvent::SessionEnd, false),
            ],
            deny: &[],
            timeout: 120,
        })
    }

    /** Return the command Codex runs (Codex spells events as PreToolUse)
     * Input
        - event: HookEvent - lifecycle event
     * Output
        - String
    */
    fn hook_command(&self, event: HookEvent) -> String {
        format!(
            "crane agent hook --event {} --profile codex",
            event.host_name()
        )
    }

    /** Answer PreToolUse the Codex way: silence to allow, exit code 2 otherwise (Codex has no
     * "ask" in PreToolUse, and an approval must never become a silent allow)
     * Input
        - verdict: &Verdict - decision
     * Output
        - Result<(), String>
    */
    fn respond_action(&self, verdict: &Verdict) -> Result<(), String> {
        let reasons = verdict.reasons.join("; ");
        match verdict.decision {
            Decision::Allow => Ok(()),
            Decision::RequireApproval => Err(format!(
                "HOOK_BLOCK:Crane requires human approval for this tool call: {reasons}"
            )),
            _ => Err(format!("HOOK_BLOCK:Crane denied this tool call: {reasons}")),
        }
    }

    /** Answer PermissionRequest the Codex way: deny with a decision object when Crane denies;
     * otherwise print nothing so the user decides (Crane never approves on the user's behalf)
     * Input
        - verdict: &Verdict - decision
     * Output
        - Result<(), String>
    */
    fn respond_permission(&self, verdict: &Verdict) -> Result<(), String> {
        if !matches!(
            verdict.decision,
            Decision::Allow | Decision::RequireApproval
        ) {
            println!(
                "{}",
                json!({"hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {"behavior": "deny", "message": format!("Crane denied this tool call: {}", verdict.reasons.join("; "))},
                }})
            );
        }
        Ok(())
    }

    /** Answer after verification the Codex way: decision "block" with the reason when the agent
     * must repair, a systemMessage otherwise
     * Input
        - event: HookEvent - lifecycle event
        - feedback: &Feedback - what to tell the agent
     * Output
        - Result<(), String>
    */
    fn respond_feedback(&self, event: HookEvent, feedback: &Feedback) -> Result<(), String> {
        let output = if feedback.block {
            match event {
                HookEvent::PostToolUse => json!({
                    "decision": "block",
                    "reason": feedback.summary,
                    "hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": feedback.detail},
                }),
                _ => {
                    json!({"decision": "block", "reason": format!("{}\n{}", feedback.summary, feedback.detail)})
                }
            }
        } else if feedback.summary.is_empty() {
            return Ok(());
        } else {
            json!({"systemMessage": format!("Crane: {}", feedback.summary)})
        };
        println!("{output}");
        Ok(())
    }

    /** Answer an event without a decision the Codex way (fail closed)
     * Input
        - event: HookEvent - lifecycle event
        - error: &str - why
        - stop_hook_active: bool - the agent continues after a held stop
     * Output
        - Result<(), String>
    */
    fn respond_failure(
        &self,
        event: HookEvent,
        error: &str,
        stop_hook_active: bool,
    ) -> Result<(), String> {
        let error = error.strip_prefix("HOOK_BLOCK:").unwrap_or(error);
        let output = match event {
            HookEvent::PreToolUse => {
                return Err(format!(
                    "HOOK_BLOCK:Crane could not authorize this tool call, so it is denied: {error}"
                ))
            }
            HookEvent::PermissionRequest => json!({"hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "deny", "message": format!("Crane could not authorize this tool call: {error}")},
            }}),
            HookEvent::PostToolUse => {
                json!({"decision": "block", "reason": format!("Crane could not verify this tool call: {error}")})
            }
            HookEvent::SessionStart => {
                return Err(format!("Crane could not start the session: {error}"))
            }
            HookEvent::Stop if !stop_hook_active => {
                json!({"decision": "block", "reason": format!("Crane could not reconcile the session: {error}")})
            }
            _ => json!({"systemMessage": format!("Crane: {error}")}),
        };
        println!("{output}");
        Ok(())
    }
}

/** Construct the adapter of a profile
 * Input
    - kind: AgentKind - profile
 * Output
    - Box<dyn AgentAdapter>
*/
pub(crate) fn adapter(kind: AgentKind) -> Box<dyn AgentAdapter> {
    match kind {
        AgentKind::Claude => Box::new(ClaudeAdapter),
        AgentKind::Codex => Box::new(CodexAdapter),
        AgentKind::Generic => Box::new(GenericAdapter),
    }
}

/** Build an action skeleton with the digest of the raw input
 * Input
    - tool: &str - tool name
    - input: &Value - raw tool input
 * Output
    - AgentAction
*/
fn skeleton(tool: &str, input: &Value) -> AgentAction {
    AgentAction {
        tool: tool.to_string(),
        operation: Operation::Other,
        files: Vec::new(),
        reads: Vec::new(),
        command: None,
        arguments: Vec::new(),
        digest: sha512_hex(input.to_string().as_bytes()),
    }
}

/** Map a Claude Code tool call to a neutral action: read tools to read (keeping the path or URL
 * read), Write, Edit, MultiEdit, and NotebookEdit to write with the proposed content or edits (a
 * write without a path becomes other), Bash and PowerShell to execute, anything else to other
 * Input
    - payload: &Value - Claude Code tool event JSON
 * Output
    - AgentAction
*/
fn claude_action(payload: &Value) -> AgentAction {
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(String::from);
    let edit = |value: &Value| {
        Some(TextEdit {
            old: text(value, "old_string")?,
            new: text(value, "new_string")?,
            all: value
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    };
    let path = ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|key| text(input, key));
    let mut action = skeleton(tool, input);
    action.arguments = strings(input);
    match (tool, path) {
        ("Read" | "NotebookRead" | "Glob" | "Grep" | "LS", path) => {
            action.operation = Operation::Read;
            action.reads.extend(path);
            action
                .reads
                .extend(text(input, "pattern").filter(|_| tool == "Glob"));
        }
        ("WebFetch" | "WebSearch", _) => {
            action.operation = Operation::Read;
            action
                .reads
                .extend(text(input, "url").or_else(|| text(input, "query")));
        }
        ("TodoWrite" | "Task" | "Agent" | "ExitPlanMode", _) => action.operation = Operation::Read,
        ("Write" | "Edit" | "MultiEdit" | "NotebookEdit", Some(path)) => {
            let proposed = match tool {
                "Write" => text(input, "content").map_or(Proposed::Unknown, Proposed::Content),
                "Edit" => edit(input).map_or(Proposed::Unknown, |edit| Proposed::Edits(vec![edit])),
                "MultiEdit" => input
                    .get("edits")
                    .and_then(Value::as_array)
                    .and_then(|edits| edits.iter().map(edit).collect::<Option<Vec<_>>>())
                    .map_or(Proposed::Unknown, Proposed::Edits),
                _ => Proposed::Unknown,
            };
            action.operation = Operation::Write;
            action.files.push(FileChange { path, proposed });
        }
        ("Bash" | "PowerShell", _) => {
            action.operation = Operation::Execute;
            action.command = text(input, "command");
        }
        _ => {}
    }
    action
}

/** Translate Crane's neutral action JSON (generic profile): {"session_id", "tool", "operation":
 * "read|write|execute|other", "path", "content" | "edits": [{"old","new","all"}] | "delete": true,
 * "command", "arguments": [..]}; a write without a path becomes other
 * Input
    - event: HookEvent - lifecycle event
    - payload: &Value - neutral JSON
 * Output
    - ProviderEvent
*/
fn neutral_event(event: HookEvent, payload: &Value) -> ProviderEvent {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(String::from);
    let action = event.has_action().then(|| {
        let mut operation = text(payload, "operation")
            .and_then(|value| Operation::parse(&value))
            .unwrap_or(Operation::Other);
        let path = text(payload, "path");
        if operation == Operation::Write && path.is_none() {
            operation = Operation::Other;
        }
        let proposed = if let Some(content) = text(payload, "content") {
            Proposed::Content(content)
        } else if payload.get("delete").and_then(Value::as_bool) == Some(true) {
            Proposed::Delete
        } else {
            payload
                .get("edits")
                .and_then(Value::as_array)
                .and_then(|edits| {
                    edits
                        .iter()
                        .map(|edit| {
                            Some(TextEdit {
                                old: text(edit, "old")?,
                                new: text(edit, "new")?,
                                all: edit.get("all").and_then(Value::as_bool).unwrap_or(false),
                            })
                        })
                        .collect::<Option<Vec<_>>>()
                })
                .map_or(Proposed::Unknown, Proposed::Edits)
        };
        let mut action = skeleton(
            &text(payload, "tool").unwrap_or_else(|| operation.name().into()),
            payload,
        );
        action.operation = operation;
        match (operation, path) {
            (Operation::Write, Some(path)) => action.files.push(FileChange { path, proposed }),
            (Operation::Read, Some(path)) => action.reads.push(path),
            _ => {}
        }
        action.command = text(payload, "command");
        action.arguments = strings(payload);
        action
    });
    provider_event(payload, action)
}

/** Map a Codex tool call to a neutral action: apply_patch (directly or through the shell) to
 * write with the files its patch changes, shell tools to execute, read-only tools to read,
 * anything else to other
 * Input
    - payload: &Value - Codex tool event JSON
 * Output
    - AgentAction
*/
fn codex_action(payload: &Value) -> AgentAction {
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    let joined = match input.get("command") {
        Some(Value::Array(parts)) => {
            let parts = parts.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            match parts.as_slice() {
                [shell, flag, script] if shell.ends_with("sh") && flag.starts_with('-') => {
                    Some(script.to_string())
                }
                _ => Some(parts.join(" ")),
            }
        }
        Some(Value::String(text)) => Some(text.clone()),
        _ => ["patch", "input", "cmd"]
            .iter()
            .find_map(|key| input.get(*key).and_then(Value::as_str).map(String::from)),
    };
    let command = joined.as_deref();
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .or_else(|| input.get("workdir").and_then(Value::as_str));
    let mut action = skeleton(tool, input);
    action.arguments = strings(input);
    let shell_patch = command
        .filter(|command| command.trim_start().starts_with("apply_patch"))
        .and_then(|command| {
            let start = command.find("*** Begin Patch")?;
            let end = command.find("*** End Patch")? + "*** End Patch".len();
            Some(command[start..end].to_string())
        });
    let shell = matches!(
        tool,
        "Bash" | "shell" | "local_shell" | "exec_command" | "container.exec" | "unified_exec"
    );
    match tool {
        "apply_patch" => {
            action.operation = Operation::Write;
            action.files = command
                .and_then(|patch| parse_patch(patch, cwd))
                .unwrap_or_default();
        }
        _ if shell && shell_patch.is_some() => {
            action.operation = Operation::Write;
            action.files = shell_patch
                .and_then(|patch| parse_patch(&patch, cwd))
                .unwrap_or_default();
        }
        _ if shell => {
            action.operation = Operation::Execute;
            action.command = command.map(String::from);
        }
        "update_plan" | "read_file" | "list_dir" | "grep_files" | "view_image" | "web_search" => {
            action.operation = Operation::Read;
            action
                .reads
                .extend(input.get("path").and_then(Value::as_str).map(String::from));
        }
        _ => {}
    }
    action
}

/** Parse a Codex apply_patch patch into the file changes it proposes
 * Input
    - patch: &str - text from "*** Begin Patch" to "*** End Patch"
    - cwd: Option<&str> - directory relative paths are resolved against
 * Output
    - Option<Vec<FileChange>>, None if the patch is malformed or changes nothing
*/
pub(crate) fn parse_patch(patch: &str, cwd: Option<&str>) -> Option<Vec<FileChange>> {
    let resolve = |path: &str| match cwd {
        Some(cwd) if Path::new(path).is_relative() => {
            Path::new(cwd).join(path).to_string_lossy().into_owned()
        }
        _ => path.to_string(),
    };
    let mut lines = patch
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .peekable();
    if lines.next()?.trim() != "*** Begin Patch" {
        return None;
    }
    let mut changes = Vec::new();
    loop {
        let line = lines.next()?;
        if line.trim() == "*** End Patch" {
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let mut content = String::new();
            while let Some(added) = lines.peek().and_then(|next| next.strip_prefix('+')) {
                content.push_str(added);
                content.push('\n');
                lines.next();
            }
            changes.push(FileChange {
                path: resolve(path.trim()),
                proposed: Proposed::Content(content),
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            changes.push(FileChange {
                path: resolve(path.trim()),
                proposed: Proposed::Delete,
            });
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            let moved = lines
                .peek()
                .and_then(|next| next.strip_prefix("*** Move to: "))
                .map(|target| resolve(target.trim()));
            if moved.is_some() {
                lines.next();
            }
            let mut edits = Vec::new();
            let mut unknown = false;
            let (mut old, mut new) = (Vec::new(), Vec::new());
            let mut flush = |old: &mut Vec<&str>, new: &mut Vec<&str>| {
                if old.is_empty() && !new.is_empty() {
                    unknown = true;
                } else if !old.is_empty() {
                    edits.push(TextEdit {
                        old: old.join("\n"),
                        new: new.join("\n"),
                        all: false,
                    });
                }
                old.clear();
                new.clear();
            };
            while let Some(next) = lines.peek().copied() {
                if next.starts_with("*** ") && next.trim() != "*** End of File" {
                    break;
                }
                lines.next();
                if next.starts_with("@@") {
                    flush(&mut old, &mut new);
                } else if next.trim() == "*** End of File" {
                    continue;
                } else if let Some(removed) = next.strip_prefix('-') {
                    old.push(removed);
                } else if let Some(added) = next.strip_prefix('+') {
                    new.push(added);
                } else {
                    if !next.is_empty() && !next.starts_with(' ') {
                        return None;
                    }
                    let context = next.strip_prefix(' ').unwrap_or(next);
                    old.push(context);
                    new.push(context);
                }
            }
            flush(&mut old, &mut new);
            let proposed = if unknown || edits.is_empty() {
                Proposed::Unknown
            } else {
                Proposed::Edits(edits)
            };
            match moved {
                Some(target) => {
                    changes.push(FileChange {
                        path: resolve(path.trim()),
                        proposed: Proposed::Delete,
                    });
                    changes.push(FileChange {
                        path: target,
                        proposed: Proposed::Unknown,
                    });
                }
                None => changes.push(FileChange {
                    path: resolve(path.trim()),
                    proposed,
                }),
            }
        } else if !line.trim().is_empty() {
            return None;
        }
    }
    (!changes.is_empty()).then_some(changes)
}

/** Collect every string inside a JSON value
 * Input
    - value: &Value - JSON value
 * Output
    - Vec<String>
*/
fn strings(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(values) => values.iter().flat_map(strings).collect(),
        Value::Object(values) => values.values().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /** Check Claude and Codex translation of writes, edits, shells, and patches
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn translates_provider_payloads() {
        let claude = ClaudeAdapter.translate(
            HookEvent::PreToolUse,
            &json!({"session_id": "s", "tool_use_id": "t1", "tool_name": "Edit", "tool_input": {"file_path": "a.py", "old_string": "x", "new_string": "y"}}),
        );
        let action = claude.action.unwrap();
        assert_eq!(action.operation, Operation::Write);
        assert_eq!(
            action.files[0].proposed,
            Proposed::Edits(vec![TextEdit {
                old: "x".into(),
                new: "y".into(),
                all: false
            }])
        );
        assert_eq!(claude.tool_call_id.as_deref(), Some("t1"));
        let bash = ClaudeAdapter.translate(
            HookEvent::PreToolUse,
            &json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
        );
        assert_eq!(bash.action.unwrap().command.as_deref(), Some("ls"));
        let patch = "*** Begin Patch\n*** Update File: a.py\n@@\n-x = 1\n+x = 2\n*** End Patch";
        let codex = CodexAdapter.translate(
            HookEvent::PreToolUse,
            &json!({"tool_name": "apply_patch", "tool_input": {"command": patch}}),
        );
        let files = codex.action.unwrap().files;
        assert_eq!(files[0].path, "a.py");
        assert_eq!(
            files[0].proposed,
            Proposed::Edits(vec![TextEdit {
                old: "x = 1".into(),
                new: "x = 2".into(),
                all: false
            }])
        );
        assert!(parse_patch("garbage", None).is_none());
        let failed = ClaudeAdapter.translate(HookEvent::PostToolUse, &json!({"tool_name": "Bash", "tool_input": {"command": "x"}, "tool_response": {"exit_code": 1}}));
        assert_eq!(failed.tool_failed, Some(true));
    }
}
