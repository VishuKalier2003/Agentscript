use std::path::Path;

use serde_json::{json, Value};

use crate::authority::{AgentAction, Decision, FileChange, Operation, Proposed, TextEdit, Verdict};
use crate::commands::{initialize_for_agent, verify_for_agent};
use crate::util::sha256;
use crate::verify::{is_pending_target, render_json, repair_owner, Assessment, Report};

/** The agent profile selected with --profile or --agent
 * Variants
    - Generic - any agent host, using Crane's neutral action format
    - Claude - Claude Code, which also supports hook installation
    - Codex - Codex, using Crane's neutral action format
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentKind {
    Generic,
    Claude,
    Codex,
}

impl AgentKind {
    /** Parse an agent profile name, by defaulting to generic when none is given, lowercasing the
     * value, and mapping known aliases (universal, claude-code) to their AgentKind
     * Input
        - value: Option<&str> - profile name from --profile or --agent
     * Output
        - Result<AgentKind, String>
        - Error if the profile is not generic, claude, or codex
    */
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("generic").to_ascii_lowercase().as_str() {
            "generic" | "universal" => Ok(Self::Generic),
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            value => Err(format!(
                "unsupported agent profile '{value}'; expected generic, claude, or codex"
            )),
        }
    }

    /** Return the stable profile name used in activation messages, by matching the variant to its
     * lowercase string
     * Input
        - None (uses self)
     * Output
        - &'static str profile name
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/** A lifecycle event reported by an agent host
 * Variants
    - SessionStart - a session starts or resumes: bind the contract and give the model context
    - UserPromptSubmit - the user sent a prompt: verify
    - PreToolUse - a tool is about to run: authorize it
    - PostToolUse - a tool ran: record it and verify incrementally
    - PermissionRequest - the host is about to ask the user to approve a tool: deny it when the
      contract forbids it, otherwise leave the decision to the user
    - Stop - the agent wants to finish: reconcile completely
    - SessionEnd - the session ended: reconcile and close
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
    /** Every event, in the order used by messages */
    const ALL: [HookEvent; 7] = [
        Self::PreToolUse,
        Self::SessionStart,
        Self::UserPromptSubmit,
        Self::PostToolUse,
        Self::PermissionRequest,
        Self::Stop,
        Self::SessionEnd,
    ];

    /** Parse the --event value, accepting the command-line name ("pre-tool-use") or the hook
     * host's own event name ("PreToolUse", as Claude Code and Codex spell it)
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
            .ok_or_else(|| {
                format!(
                    "unknown hook event '{value}'; use {}",
                    Self::ALL.map(Self::name).join(", ")
                )
            })
    }

    /** Return the command-line name of the event
     * Input
        - None (uses self)
     * Output
        - &'static str such as "post-tool-use"
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

    /** Return the event name used by hook hosts in their configuration and payloads
     * Input
        - None (uses self)
     * Output
        - &'static str such as "PostToolUse"
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
        - bool, true for pre-tool-use, post-tool-use, and permission-request
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
    - session: Option<String> - provider session id, None when the provider sent none
    - action: Option<AgentAction> - the tool call, for events where has_action is true
    - stop_hook_active: bool - the agent is already continuing because a stop was blocked
    - model: Option<String> - model the agent host reported, if any
    - source: Option<String> - why the session started (startup, resume, clear, compact), if
      reported
*/
pub(crate) struct ProviderEvent {
    pub(crate) session: Option<String>,
    pub(crate) action: Option<AgentAction>,
    pub(crate) stop_hook_active: bool,
    pub(crate) model: Option<String>,
    pub(crate) source: Option<String>,
}

/** Common interface for agent integrations: an adapter only translates its provider's hook
 * payloads into Crane events and renders Crane's decisions back in the provider's protocol; the
 * policy semantics live in the provider-neutral authority and verify modules
*/
pub(crate) trait AgentAdapter {
    /** Report which agent profile this adapter serves, implemented by each adapter
     * Input
        - None (uses self)
     * Output
        - AgentKind of the adapter
    */
    fn kind(&self) -> AgentKind;

    /** List provider files that configure the hooks enforcing Crane, protected like .crane
     * Input
        - None (uses self)
     * Output
        - &'static [&'static str] of lowercase paths with "/" separators
    */
    fn protected_paths(&self) -> &'static [&'static str] {
        &[]
    }

    /** Translate a hook payload into a Crane event, using the neutral action format by default
     * Input
        - event: HookEvent - lifecycle event
        - payload: &Value - hook payload from stdin (Null when empty)
     * Output
        - ProviderEvent
    */
    fn translate(&self, event: HookEvent, payload: &Value) -> ProviderEvent {
        neutral_event(event, payload)
    }

    /** Answer a pre-tool-use authorization in the provider's protocol; by default print the
     * verdict as JSON and exit with the blocking code when denied
     * Input
        - verdict: &Verdict - decision for the proposed action
     * Output
        - Result<(), String>
        - HOOK_BLOCK error when the action is denied
    */
    fn respond_action(&self, verdict: &Verdict) -> Result<(), String> {
        println!(
            "{}",
            serde_json::json!({
                "decision": verdict.decision.name(),
                "reasons": verdict.reasons,
                "resources": verdict.resources,
            })
        );
        match verdict.decision {
            Decision::Allow => Ok(()),
            _ => Err(format!(
                "HOOK_BLOCK:Crane denied this action: {}",
                verdict.reasons.join("; ")
            )),
        }
    }

    /** Answer a permission-request authorization in the provider's protocol; by default the same
     * as respond_action
     * Input
        - verdict: &Verdict - decision for the tool call awaiting approval
     * Output
        - Result<(), String>
        - HOOK_BLOCK error when the tool call is denied
    */
    fn respond_permission(&self, verdict: &Verdict) -> Result<(), String> {
        self.respond_action(verdict)
    }

    /** Answer a verification event in the provider's protocol; by default print the JSON
     * contract and exit with the blocking code when the agent must repair something
     * Input
        - event: HookEvent - lifecycle event
        - assessment: Assessment - what the report means for the agent
        - report: &Report - verification report
     * Output
        - Result<(), String>
        - HOOK_BLOCK error when the assessment is Block
    */
    fn respond_verification(
        &self,
        _event: HookEvent,
        assessment: Assessment,
        report: &Report,
    ) -> Result<(), String> {
        println!("{}", render_json(report));
        match assessment {
            Assessment::Block => Err("HOOK_BLOCK:one or more Crane policies failed".into()),
            _ => Ok(()),
        }
    }

    /** Initialize an agent integration, by first creating the .crane repository structure and then
     * immediately running verification so the agent sees the current state
     * Input
        - None (uses self)
     * Output
        - Result<(), String>
        - Error if initialization fails or verification finds violations
    */
    fn initialize(&self) -> Result<(), String> {
        initialize_for_agent()?;
        self.verify()
    }

    /** Run verification for the agent, by delegating to the shared verifier so every profile gets
     * identical policy semantics and JSON output
     * Input
        - None (uses self)
     * Output
        - Result<(), String>
        - Error if any policy fails or verification cannot complete
    */
    fn verify(&self) -> Result<(), String> {
        verify_for_agent()
    }

    /** Describe the output and exit-code contract expected by external agents, by returning a fixed
     * explanatory string
     * Input
        - None (uses self)
     * Output
        - &'static str contract description
    */
    fn feedback_contract(&self) -> &'static str {
        "stdout contains Crane's stable JSON verification result; non-zero exit means repair is required"
    }
}

/** Adapter that uses the shared behavior unchanged, used directly for the generic profile and
 * wrapped by the Claude and Codex adapters
 * Fields
    - kind: AgentKind - profile this adapter reports
*/
pub(crate) struct UniversalAgentAdapter {
    kind: AgentKind,
}

impl UniversalAgentAdapter {
    /** Create a universal adapter, by storing the selected profile so shared lifecycle behavior can
     * still report which agent it serves
     * Input
        - kind: AgentKind - selected agent profile
     * Output
        - UniversalAgentAdapter
    */
    pub(crate) fn new(kind: AgentKind) -> Self {
        Self { kind }
    }
}

impl AgentAdapter for UniversalAgentAdapter {
    /** Report the profile this universal adapter was created with, by returning the stored kind
     * Input
        - None (uses self)
     * Output
        - AgentKind stored at construction
    */
    fn kind(&self) -> AgentKind {
        self.kind
    }
}

/** Adapter for Claude Code, wrapping the universal adapter so it has its own type
 * Fields
    - 0: UniversalAgentAdapter - wrapped adapter providing the shared behavior
*/
pub(crate) struct ClaudeAdapter(UniversalAgentAdapter);

/** Adapter for the Codex CLI, wrapping the universal adapter so it has its own type; it reads
 * Codex hook JSON and answers in Codex's hook output format
 * Fields
    - 0: UniversalAgentAdapter - wrapped adapter providing the shared behavior
*/
pub(crate) struct CodexAdapter(UniversalAgentAdapter);

impl AgentAdapter for ClaudeAdapter {
    /** Report the Claude profile, by delegating to the wrapped universal adapter
     * Input
        - None (uses self)
     * Output
        - AgentKind of the wrapped adapter
    */
    fn kind(&self) -> AgentKind {
        self.0.kind()
    }

    /** Protect the Claude Code settings files that register Crane's hooks
     * Input
        - None (uses self)
     * Output
        - &'static [&'static str] of settings paths
    */
    fn protected_paths(&self) -> &'static [&'static str] {
        &[".claude/settings.json", ".claude/settings.local.json"]
    }

    /** Translate a Claude Code hook payload: session_id, stop_hook_active, and for tool events
     * the tool_name and tool_input mapped to a neutral action
     * Input
        - event: HookEvent - lifecycle event
        - payload: &Value - Claude Code hook JSON
     * Output
        - ProviderEvent
    */
    fn translate(&self, event: HookEvent, payload: &Value) -> ProviderEvent {
        ProviderEvent {
            session: payload
                .get("session_id")
                .and_then(Value::as_str)
                .map(String::from),
            action: event.has_action().then(|| claude_action(payload)),
            stop_hook_active: payload
                .get("stop_hook_active")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            model: payload
                .get("model")
                .and_then(Value::as_str)
                .map(String::from),
            source: payload
                .get("source")
                .and_then(Value::as_str)
                .map(String::from),
        }
    }

    /** Answer PreToolUse the Claude Code way: exit code 2 with the reason on stderr to deny,
     * permissionDecision "ask" JSON when approval is required, and silence to allow
     * Input
        - verdict: &Verdict - decision for the proposed tool call
     * Output
        - Result<(), String>
        - HOOK_BLOCK error when the tool call is denied
    */
    fn respond_action(&self, verdict: &Verdict) -> Result<(), String> {
        let reasons = verdict.reasons.join("; ");
        match verdict.decision {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(format!("HOOK_BLOCK:Crane denied this tool call: {reasons}")),
            Decision::ApprovalRequired => {
                println!(
                    "{}",
                    serde_json::json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PreToolUse",
                            "permissionDecision": "ask",
                            "permissionDecisionReason": reasons,
                        }
                    })
                );
                Ok(())
            }
        }
    }

    /** Answer a verification event the Claude Code way: the JSON contract on stdout and stderr
     * with exit code 2 to block, the JSON contract alone when clean, and otherwise non-blocking
     * hook JSON with a systemMessage and, where supported, additionalContext
     * Input
        - event: HookEvent - lifecycle event
        - assessment: Assessment - what the report means for the agent
        - report: &Report - verification report
     * Output
        - Result<(), String>
        - HOOK_BLOCK error when the assessment is Block
    */
    fn respond_verification(
        &self,
        event: HookEvent,
        assessment: Assessment,
        report: &Report,
    ) -> Result<(), String> {
        match assessment {
            Assessment::Block => {
                println!("{}", render_json(report));
                eprintln!("{}", render_json(report));
                Err("HOOK_BLOCK:one or more Crane policies failed".into())
            }
            Assessment::Clean => {
                println!("{}", render_json(report));
                Ok(())
            }
            Assessment::Advisory => {
                println!("{}", claude_advisory(event, report));
                Ok(())
            }
        }
    }
}

impl AgentAdapter for CodexAdapter {
    /** Report the Codex profile, by delegating to the wrapped universal adapter
     * Input
        - None (uses self)
     * Output
        - AgentKind of the wrapped adapter
    */
    fn kind(&self) -> AgentKind {
        self.0.kind()
    }

    /** Protect the project Codex files that register Crane's hooks
     * Input
        - None (uses self)
     * Output
        - &'static [&'static str] of Codex configuration paths
    */
    fn protected_paths(&self) -> &'static [&'static str] {
        &[".codex/hooks.json", ".codex/config.toml"]
    }

    /** Translate a Codex hook payload: session_id, stop_hook_active, and for tool events the
     * tool_name and tool_input mapped to a neutral action (apply_patch patches are parsed into
     * the files they change, relative paths resolved against the payload's cwd)
     * Input
        - event: HookEvent - lifecycle event
        - payload: &Value - Codex hook JSON
     * Output
        - ProviderEvent
    */
    fn translate(&self, event: HookEvent, payload: &Value) -> ProviderEvent {
        ProviderEvent {
            session: payload
                .get("session_id")
                .and_then(Value::as_str)
                .map(String::from),
            action: event.has_action().then(|| codex_action(payload)),
            stop_hook_active: payload
                .get("stop_hook_active")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            model: payload
                .get("model")
                .and_then(Value::as_str)
                .map(String::from),
            source: payload
                .get("source")
                .and_then(Value::as_str)
                .map(String::from),
        }
    }

    /** Answer PreToolUse the Codex way: allow by printing nothing; deny with exit code 2 and the
     * reason on stderr (Codex runs a tool when a hook fails in any other way, so the blocking exit
     * code is used rather than JSON); approval is denied too, since Codex does not support "ask"
     * in PreToolUse yet and an approval must never become a silent allow
     * Input
        - verdict: &Verdict - decision for the proposed tool call
     * Output
        - Result<(), String>
        - HOOK_BLOCK error when the tool call is denied or needs approval
    */
    fn respond_action(&self, verdict: &Verdict) -> Result<(), String> {
        let reasons = verdict.reasons.join("; ");
        match verdict.decision {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(format!("HOOK_BLOCK:Crane denied this tool call: {reasons}")),
            Decision::ApprovalRequired => Err(format!(
                "HOOK_BLOCK:Crane requires human approval for this tool call: {reasons}"
            )),
        }
    }

    /** Answer PermissionRequest the Codex way: deny with a decision object when the contract
     * forbids the tool call; otherwise print nothing, so Codex shows its normal approval prompt
     * and the user decides (Crane never approves on the user's behalf)
     * Input
        - verdict: &Verdict - decision for the tool call awaiting approval
     * Output
        - Result<(), String>, always Ok
    */
    fn respond_permission(&self, verdict: &Verdict) -> Result<(), String> {
        if verdict.decision == Decision::Deny {
            println!(
                "{}",
                json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PermissionRequest",
                        "decision": {
                            "behavior": "deny",
                            "message": format!("Crane denied this tool call: {}", verdict.reasons.join("; ")),
                        },
                    },
                })
            );
        }
        Ok(())
    }

    /** Answer a verification event the Codex way, printing only fields Codex understands and
     * nothing when the report is clean: PostToolUse blocks with decision "block" and gives the
     * report as additionalContext; Stop continues the turn with decision "block" and the report as
     * the reason; anything that must not block becomes a systemMessage
     * Input
        - event: HookEvent - lifecycle event
        - assessment: Assessment - what the report means for the agent
        - report: &Report - verification report
     * Output
        - Result<(), String>, always Ok (Codex reads the decision from stdout)
    */
    fn respond_verification(
        &self,
        event: HookEvent,
        assessment: Assessment,
        report: &Report,
    ) -> Result<(), String> {
        let summary = summarize(report);
        let output = match (event, assessment) {
            (_, Assessment::Clean) => return Ok(()),
            (HookEvent::PostToolUse, Assessment::Block) => json!({
                "decision": "block",
                "reason": format!("Crane verification failed; repair the repository: {summary}"),
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "additionalContext": render_json(report),
                },
            }),
            (HookEvent::PostToolUse, Assessment::Advisory) => json!({
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "additionalContext": format!("Crane (do not modify .crane; a human repairs policy problems): {summary}"),
                },
            }),
            (HookEvent::Stop, Assessment::Block) => json!({
                "decision": "block",
                "reason": format!(
                    "Crane contract not satisfied; repair before finishing: {summary}\n{}",
                    render_json(report)
                ),
            }),
            _ => json!({ "systemMessage": format!("Crane: {summary}") }),
        };
        println!("{output}");
        Ok(())
    }
}

/** Construct the adapter for a profile, by matching the kind and boxing the Claude, Codex, or
 * universal adapter behind the AgentAdapter trait
 * Input
    - kind: AgentKind - selected agent profile
 * Output
    - Box<dyn AgentAdapter> for that profile
*/
pub(crate) fn adapter(kind: AgentKind) -> Box<dyn AgentAdapter> {
    match kind {
        AgentKind::Claude => Box::new(ClaudeAdapter(UniversalAgentAdapter::new(kind))),
        AgentKind::Codex => Box::new(CodexAdapter(UniversalAgentAdapter::new(kind))),
        AgentKind::Generic => Box::new(UniversalAgentAdapter::new(kind)),
    }
}

/** Map a Claude Code tool call to a neutral action: read-only tools to read; Write, Edit,
 * MultiEdit, and NotebookEdit to write with their path and proposed content or edits (a write
 * without a path becomes other, so every argument is scanned); Bash and PowerShell to execute;
 * and any other tool (MCP servers, new built-ins) to other
 * Input
    - payload: &Value - Claude Code PreToolUse or PostToolUse JSON
 * Output
    - AgentAction
*/
fn claude_action(payload: &Value) -> AgentAction {
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let input = payload.get("tool_input").unwrap_or(payload);
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
    let mut action = AgentAction {
        tool: tool.clone(),
        operation: Operation::Other,
        files: Vec::new(),
        command: None,
        arguments: Vec::new(),
        digest: sha256(input.to_string().as_bytes()),
    };
    match (tool.as_str(), path) {
        (
            "Read" | "Glob" | "Grep" | "LS" | "NotebookRead" | "WebFetch" | "WebSearch"
            | "TodoWrite" | "Task" | "Agent",
            _,
        ) => action.operation = Operation::Read,
        ("Write" | "Edit" | "MultiEdit" | "NotebookEdit", Some(path)) => {
            let proposed = match tool.as_str() {
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
        _ => action.arguments = strings(input),
    }
    action
}

/** Translate Crane's neutral action format, used by the generic and Codex profiles:
 * {"session_id", "stop_hook_active", "tool", "operation": "read|write|execute|other", "path",
 * "content" | "edits": [{"old", "new", "all"}] | "delete": true, "command", "arguments": [..]};
 * a write without a path, or an unknown operation, becomes other
 * Input
    - event: HookEvent - lifecycle event
    - payload: &Value - neutral JSON (Null when empty)
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
        AgentAction {
            tool: text(payload, "tool").unwrap_or_else(|| operation.name().into()),
            operation,
            files: match (operation, path) {
                (Operation::Write, Some(path)) => vec![FileChange { path, proposed }],
                _ => Vec::new(),
            },
            command: text(payload, "command"),
            arguments: strings(payload),
            digest: sha256(payload.to_string().as_bytes()),
        }
    });
    ProviderEvent {
        session: text(payload, "session_id"),
        action,
        stop_hook_active: payload
            .get("stop_hook_active")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        model: payload
            .get("model")
            .and_then(Value::as_str)
            .map(String::from),
        source: payload
            .get("source")
            .and_then(Value::as_str)
            .map(String::from),
    }
}

/** Map a Codex tool call to a neutral action: apply_patch to write with the files its patch
 * changes (a patch that cannot be parsed becomes a write with no known files, which the decision
 * engine denies); Bash to execute; update_plan to read; and any other tool (MCP servers, other
 * local functions) to other
 * Input
    - payload: &Value - Codex PreToolUse, PostToolUse, or PermissionRequest JSON
 * Output
    - AgentAction
*/
fn codex_action(payload: &Value) -> AgentAction {
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    let command = input.get("command").and_then(Value::as_str);
    let cwd = payload.get("cwd").and_then(Value::as_str);
    let mut action = AgentAction {
        tool: tool.clone(),
        operation: Operation::Other,
        files: Vec::new(),
        command: None,
        arguments: Vec::new(),
        digest: sha256(input.to_string().as_bytes()),
    };
    match tool.as_str() {
        "apply_patch" => {
            action.operation = Operation::Write;
            action.files = command
                .and_then(|patch| parse_patch(patch, cwd))
                .unwrap_or_default();
        }
        "Bash" => {
            action.operation = Operation::Execute;
            action.command = command.map(String::from);
        }
        "update_plan" => action.operation = Operation::Read,
        _ => action.arguments = strings(input),
    }
    action
}

/** Parse a Codex apply_patch patch into the file changes it proposes: "*** Add File:" becomes the
 * new content, "*** Delete File:" a deletion, and "*** Update File:" one text edit per "@@" hunk
 * (context and removed lines as the old text, context and added lines as the new text); a move
 * ("*** Move to:") deletes the old path and writes the new one with unknown content, and a hunk
 * without old text is unknown because its position cannot be simulated
 * Input
    - patch: &str - text between "*** Begin Patch" and "*** End Patch"
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
                    let context = next.strip_prefix(' ').unwrap_or(next);
                    if !next.is_empty() && !next.starts_with(' ') {
                        return None;
                    }
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

/** Summarize a report's violations in one line for hook messages
 * Input
    - report: &Report - verification report
 * Output
    - String such as "payment (agent): Protected function was modified."
*/
fn summarize(report: &Report) -> String {
    report
        .violations
        .iter()
        .map(|violation| {
            format!(
                "{} ({}): {}",
                violation.policy_id,
                repair_owner(violation),
                violation.message
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/** Collect every string inside a JSON value, recursing through arrays and objects
 * Input
    - value: &Value - JSON value
 * Output
    - Vec<String> of every string leaf
*/
fn strings(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(values) => values.iter().flat_map(strings).collect(),
        Value::Object(values) => values.values().flat_map(strings).collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) => Vec::new(),
    }
}

/** Build Claude Code's non-blocking hook JSON for violations that must not block now, by writing
 * a systemMessage for the user covering human-only failures and pending targets, and for events
 * that support it adding additionalContext that tells the agent not to touch human-only failures,
 * lists the targets its task still has to satisfy, and includes the full JSON report
 * Input
    - event: HookEvent - lifecycle event
    - report: &Report - report whose violations are human-owned or pending targets
 * Output
    - String of Claude Code hook JSON
*/
fn claude_advisory(event: HookEvent, report: &Report) -> String {
    let list = |pending: bool| {
        report
            .violations
            .iter()
            .filter(|violation| is_pending_target(violation) == pending)
            .map(|violation| format!("{}: {}", violation.policy_id, violation.message))
            .collect::<Vec<_>>()
            .join("; ")
    };
    let (human, targets) = (list(false), list(true));
    let mut message = Vec::new();
    let mut context = Vec::new();
    if !human.is_empty() {
        message.push(format!(
            "Crane cannot fully verify this repository until a human repairs .crane metadata: {human}"
        ));
        context.push("Crane reported problems that only a human can repair (repair_owner \"human\"). Agents are blocked from .crane, so do not try to fix or work around them; continue the task and mention them to the user.".to_string());
    }
    if !targets.is_empty() {
        message.push(format!("Crane targets not yet satisfied: {targets}"));
        context.push(format!(
            "Crane target rules require these changes before the task is complete: {targets}"
        ));
    }
    let mut output = serde_json::json!({ "systemMessage": message.join(" | ") });
    let hook_event_name = match event {
        HookEvent::UserPromptSubmit => Some("UserPromptSubmit"),
        HookEvent::PostToolUse => Some("PostToolUse"),
        _ => None,
    };
    if let Some(hook_event_name) = hook_event_name {
        output["hookSpecificOutput"] = serde_json::json!({
            "hookEventName": hook_event_name,
            "additionalContext": format!("{}\n{}", context.join("\n"), render_json(report)),
        });
    }
    output.to_string()
}
