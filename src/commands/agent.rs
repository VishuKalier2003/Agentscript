use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::adapter::{adapter, AgentAdapter, AgentKind, HookEvent, ProviderEvent};
use crate::agent_session::{self, SessionOptions};
use crate::authority::{AgentAction, Operation, Runtime, Usage};
use crate::effects;
use crate::ir::ContractSet;
use crate::repository::ensure_initialized;
use crate::scope::ScopeContext;
use crate::session::{read_attestation, session_ids, ContractSession, Lifecycle};
use crate::util::option;
use crate::verify::{assess, setup_report, verify_contracts, Assessment, Report};
use crate::zones::model::SafetyState;

/** Handle the agent subcommands, by first reading the operation (default verify) and the
 * --profile/--agent value, building the matching adapter, and then routing to install, hook,
 * session, init, or verify
 * Input
    - args: &[String] - arguments after "agent"
 * Output
    - Result<(), String>
    - Error if the profile or operation is unknown, or the operation fails
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    // Route adapter operations while keeping policy evaluation inside Crane
    let operation = args.first().map(String::as_str).unwrap_or("verify");
    let profile = args
        .iter()
        .position(|argument| argument == "--profile" || argument == "--agent")
        .and_then(|index| args.get(index + 1))
        .map(String::as_str);
    let selected = AgentKind::parse(profile)?;
    let adapter = adapter(selected);
    match operation {
        "install" => match selected {
            AgentKind::Codex => install_codex_hooks().map(|_| ()),
            _ => install_hooks(selected),
        },
        "hook" => {
            let event = option(args, "--event").ok_or(
                "agent hook requires --event session-start|user-prompt-submit|pre-tool-use|post-tool-use|permission-request|stop|session-end (or the host spelling, such as PreToolUse)",
            )?;
            let options = SessionOptions::from_args(args)?;
            hook(HookEvent::parse(&event)?, adapter.as_ref(), &options)
        }
        "session" => session(&args[1..], selected),
        "init" => {
            if selected == AgentKind::Codex {
                // Codex installation is additive and idempotent, so init can always (re)install it
                super::init::run()?;
                install_codex_hooks()?;
            }
            adapter.initialize()?;
            println!(
                "Activated Crane agent adapter profile '{}'.",
                adapter.kind().name()
            );
            println!("{}", adapter.feedback_contract());
            Ok(())
        }
        "verify" | "check" => adapter.verify(),
        _ => Err(format!(
            "unknown agent operation '{operation}'; use 'crane agent init', 'crane agent verify', or 'crane agent session'"
        )),
    }
}

/** Run one lifecycle hook, by reading and translating the provider payload, binding it to its
 * contract session (created on first sight, transient when the provider sends no session id),
 * and then handling the event; for pre-tool-use every failure is turned into a denial so a crash
 * can never let a tool run unchecked
 * Input
    - event: HookEvent - lifecycle event
    - adapter: &dyn AgentAdapter - provider adapter
    - options: &SessionOptions - lifetime, idle timeout, task, autonomy mode, and budget, used
      only when the session is created (a task given later must match the bound one)
 * Output
    - Result<(), String>
    - HOOK_BLOCK error when the action is denied or the agent must repair something now
*/
fn hook(
    event: HookEvent,
    adapter: &dyn AgentAdapter,
    options: &SessionOptions,
) -> Result<(), String> {
    let result = read_payload(event).and_then(|payload| {
        let provider = adapter.translate(event, &payload);
        if let Err(error) = ensure_initialized() {
            return unbound(event, adapter, provider, &error, false);
        }
        let session = match &provider.session {
            Some(id) => {
                agent_session::establish(adapter.kind(), id, options).map(|(session, _)| session)
            }
            None => agent_session::transient(adapter.kind(), options),
        };
        match session {
            Ok(session) => handle(event, adapter, provider, &session),
            Err(error) => unbound(event, adapter, provider, &error, true),
        }
    });
    match (event, result) {
        (HookEvent::PreToolUse, Err(error)) if !error.starts_with("HOOK_BLOCK:") => Err(format!(
            "HOOK_BLOCK:Crane could not authorize this tool call: {error}"
        )),
        (_, result) => result,
    }
}

/** Handle an event under its contract session through the provider-neutral session manager:
 * session-start reactivates a resumable session and prints the concise context; user-prompt-submit
 * verifies every clause (and restores lapsed authority, since a human is present); pre-tool-use and
 * permission-request authorize the action through the common policy engine (contract, zones,
 * autonomy, budget, safety) and journal it; post-tool-use journals the action, verifies only the
 * clauses it could have affected, and degrades the session if the contract drifted; stop
 * reconciles completely and writes the attestation (targets are enforced unless this stop is
 * already a forced retry); session-end reconciles and closes the session (resumable)
 * Input
    - event: HookEvent - lifecycle event
    - adapter: &dyn AgentAdapter - provider adapter
    - provider: ProviderEvent - translated payload
    - session: &ContractSession - bound session
 * Output
    - Result<(), String>
    - HOOK_BLOCK error when the action is denied or the agent must repair something now
*/
fn handle(
    event: HookEvent,
    adapter: &dyn AgentAdapter,
    provider: ProviderEvent,
    session: &ContractSession,
) -> Result<(), String> {
    let protected = adapter.protected_paths();
    match event {
        HookEvent::SessionStart => {
            let context = agent_session::on_start(
                session,
                provider.model.as_deref(),
                provider.source.as_deref(),
            )?;
            print!("{context}");
            Ok(())
        }
        HookEvent::UserPromptSubmit => {
            let report = verify_contracts(session.contracts(), &mut ScopeContext::new(), &|_| true);
            session.record(json!({
                "event": "user_prompt_submit",
                "verification": outcome(&report),
            }))?;
            adapter.respond_verification(event, assess(&report, false), &report)
        }
        HookEvent::PreToolUse => {
            let action = provider.action.unwrap_or_else(unknown_action);
            let verdict = agent_session::authorize(session, protected, &action, "pre_tool_use")?;
            adapter.respond_action(&verdict)
        }
        HookEvent::PermissionRequest => {
            let action = provider.action.unwrap_or_else(unknown_action);
            let verdict =
                agent_session::authorize(session, protected, &action, "permission_request")?;
            adapter.respond_permission(&verdict)
        }
        HookEvent::PostToolUse => {
            let action = provider.action.unwrap_or_else(unknown_action);
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
                effects::observe(session, &action, protected)?
            };
            session.record(json!({
                "event": "post_tool_use",
                "tool": action.tool,
                "operation": action.operation.name(),
                "resources": action.files.iter().map(|change| format!("file:{}", change.path)).collect::<Vec<_>>(),
                "arguments_digest": action.digest,
                "result": "executed",
                "verified_clauses": effect["clauses_checked"].as_u64().unwrap_or(0),
                "verification": outcome(&report),
                "effect": effect,
            }))?;
            agent_session::after_tool(session)?;
            adapter.respond_verification(event, assess(&report, false), &report)
        }
        HookEvent::Stop => {
            let (report, attestation) = effects::validate(session)?;
            session.write_attestation(&attestation)?;
            session.record(json!({
                "event": "stop",
                "stop_hook_active": provider.stop_hook_active,
                "final_status": attestation["final_status"],
                "tests": attestation["tests"].as_array().map(|tests| tests.iter().map(|test| json!({"language": test["language"], "status": test["status"]})).collect::<Vec<_>>()),
            }))?;
            adapter.respond_verification(
                event,
                assess(&report, !provider.stop_hook_active),
                &report,
            )
        }
        HookEvent::SessionEnd => {
            let (_, attestation) = session.reconcile();
            session.write_attestation(&attestation)?;
            session.record(json!({
                "event": "session_end",
                "final_status": attestation["final_status"],
            }))?;
            session.set_lifecycle(Lifecycle::Closed)
        }
    }
}

/** Handle an event when no contract session is available: Crane is not initialized (then only
 * the metadata guard applies, as before sessions existed) or the session could not be
 * established (then every mutating action is denied); verification events report the problem
 * without blocking, since only a human can repair it
 * Input
    - event: HookEvent - lifecycle event
    - adapter: &dyn AgentAdapter - provider adapter
    - provider: ProviderEvent - translated payload
    - error: &str - why there is no session
    - fail_closed: bool - deny mutating actions (true when Crane is initialized)
 * Output
    - Result<(), String>
    - Error for session-start, HOOK_BLOCK error for a denied action
*/
fn unbound(
    event: HookEvent,
    adapter: &dyn AgentAdapter,
    provider: ProviderEvent,
    error: &str,
    fail_closed: bool,
) -> Result<(), String> {
    match event {
        HookEvent::SessionStart => Err(error.into()),
        HookEvent::PreToolUse | HookEvent::PermissionRequest => {
            let empty = ContractSet::new(Vec::new(), Vec::new());
            let runtime = Runtime {
                contracts: &empty,
                grants: &[],
                root: env::current_dir().map_err(|error| error.to_string())?,
                protected: adapter.protected_paths(),
                problem: fail_closed.then(|| error.to_string()),
                governance: None,
                usage: Usage::unlimited(),
                safety: SafetyState::Active,
                safety_reason: None,
            };
            let verdict = runtime.decide(&provider.action.unwrap_or_else(unknown_action));
            match event {
                HookEvent::PermissionRequest => adapter.respond_permission(&verdict),
                _ => adapter.respond_action(&verdict),
            }
        }
        _ => {
            let report = setup_report(error);
            let assessment = match assess(&report, false) {
                Assessment::Block => Assessment::Advisory,
                other => other,
            };
            adapter.respond_verification(event, assessment, &report)
        }
    }
}

/** Read the hook payload from stdin, treating empty input as Null; invalid JSON is an error for
 * pre-tool-use (which then denies) and Null for every other event
 * Input
    - event: HookEvent - lifecycle event
 * Output
    - Result<Value, String>
    - HOOK_BLOCK error if pre-tool-use input cannot be read or parsed
*/
fn read_payload(event: HookEvent) -> Result<Value, String> {
    let mut input = String::new();
    if let Err(error) = io::stdin().read_to_string(&mut input) {
        return match event {
            HookEvent::PreToolUse => Err(format!("HOOK_BLOCK:could not read hook input: {error}")),
            _ => Ok(Value::Null),
        };
    }
    if input.trim().is_empty() {
        return Ok(Value::Null);
    }
    match serde_json::from_str(&input) {
        Ok(payload) => Ok(payload),
        Err(error) if event == HookEvent::PreToolUse => {
            Err(format!("HOOK_BLOCK:invalid hook input: {error}"))
        }
        Err(_) => Ok(Value::Null),
    }
}

/** Build the action used when a tool event carries no tool call, which only the metadata guard
 * and incremental verification of every clause can judge
 * Input
    - None
 * Output
    - AgentAction of an unknown tool with no arguments
*/
fn unknown_action() -> AgentAction {
    AgentAction {
        tool: String::new(),
        operation: Operation::Other,
        files: Vec::new(),
        command: None,
        arguments: Vec::new(),
        digest: String::new(),
    }
}

/** Summarize a report for the journal
 * Input
    - report: &Report - verification report
 * Output
    - &'static str, "pass" or "fail"
*/
fn outcome(report: &Report) -> &'static str {
    if report.violations.is_empty() {
        "pass"
    } else {
        "fail"
    }
}

/** Manage contract sessions: "list" prints one line per session (an unreadable or tampered session
 * is listed as invalid) and "show ID" prints the binding, its digest, governance, activity, drift,
 * and the latest attestation as JSON; "start", "resume", "cancel", "finalize", "sweep",
 * "quarantine", and "extend" run the session lifecycle (refused to agents by the pre-tool hook,
 * and resume, extend, and start also in an agent environment)
 * Input
    - args: &[String] - arguments after "agent session"
    - profile: AgentKind - --profile value, used by start
 * Output
    - Result<(), String>
    - Error if not initialized, the session is unknown, or the subcommand is invalid
*/
fn session(args: &[String], profile: AgentKind) -> Result<(), String> {
    ensure_initialized()?;
    let id = || {
        args.get(1)
            .filter(|value| !value.starts_with("--"))
            .cloned()
            .ok_or_else(|| "this session operation requires a session id".to_string())
    };
    let reason = || option(args, "--reason").unwrap_or_else(|| "by a human".into());
    match args.first().map(String::as_str) {
        None | Some("list") | Some("--profile") | Some("--agent") => {
            let ids = session_ids()?;
            if ids.is_empty() {
                println!("No contract sessions.");
            }
            for id in ids {
                let status = read_attestation(&id)?
                    .and_then(|attestation| attestation["final_status"].as_str().map(String::from))
                    .unwrap_or_else(|| "not reconciled".into());
                let lifecycle = match ContractSession::load(&id) {
                    Ok(Some(session)) => session.lifecycle().name(),
                    Ok(None) => "missing",
                    Err(_) => "invalid",
                };
                println!("{id} {lifecycle} {status}");
            }
            Ok(())
        }
        Some("show") => {
            let id = id()?;
            let session = ContractSession::load(&id)?
                .ok_or_else(|| format!("no contract session '{id}'"))?;
            let mut output = session.describe();
            output["attestation"] = json!(read_attestation(&id)?);
            println!(
                "{}",
                serde_json::to_string_pretty(&output).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        Some("start") => {
            let provider = option(args, "--session")
                .ok_or("crane agent session start requires --session PROVIDER_SESSION_ID")?;
            let options = SessionOptions::from_args(args)?;
            let (session, context) = agent_session::start(profile, &provider, &options)?;
            println!("Contract session {} is ready.", session.describe()["session_id"].as_str().unwrap_or_default());
            if options.isolate {
                println!("Isolated worktree: {} (run the agent there)", session.root_path().display());
            }
            print!("{context}");
            Ok(())
        }
        Some("resume") => {
            let id = id()?;
            agent_session::resume(&id)?;
            println!("Resumed contract session {id}.");
            Ok(())
        }
        Some("cancel") => {
            let id = id()?;
            let cancelled = agent_session::cancel(&id, &reason())?;
            println!(
                "{}",
                if cancelled { format!("Cancelled contract session {id}; it can never act again.") } else { format!("Contract session {id} had already ended.") }
            );
            Ok(())
        }
        Some("finalize") => {
            let id = id()?;
            let attestation = agent_session::finalize(&id)?;
            println!("Finalized contract session {id}: {}", attestation["final_status"].as_str().unwrap_or("unknown"));
            Ok(())
        }
        Some("verify") => {
            let id = id()?;
            let session = ContractSession::load(&id)?.ok_or_else(|| format!("no contract session '{id}'"))?;
            let level = option(args, "--level").unwrap_or_else(|| "fast".into());
            let output = match level.as_str() {
                "fast" => {
                    let (report, effect) = effects::within(&effects::workspace_of(&session).unwrap_or_else(|| std::env::current_dir().unwrap_or_default()), || {
                        effects::observe(&session, &unknown_action(), &[])
                    })?;
                    json!({"level": "fast", "effect": effect, "violations": report.violations.iter().map(|violation| json!({"type": violation.violation_type, "target": violation.target, "message": violation.message})).collect::<Vec<_>>()})
                }
                "tests" => {
                    let tests = effects::within(&effects::workspace_of(&session).unwrap_or_else(|| std::env::current_dir().unwrap_or_default()), || {
                        let (_, changed, _) = effects::cumulative(&session)?;
                        effects::affected_tests(&session, &changed)
                    })?;
                    json!({"level": "tests", "tests": tests})
                }
                "full" => {
                    let (_, attestation) = effects::validate(&session)?;
                    session.write_attestation(&attestation)?;
                    json!({"level": "full", "attestation": attestation})
                }
                other => return Err(format!("unknown verification level '{other}'; use fast, tests, or full")),
            };
            session.record(json!({"event": "verification", "level": level}))?;
            println!("{}", serde_json::to_string_pretty(&output).map_err(|error| error.to_string())?);
            Ok(())
        }
        Some("cleanup") => {
            let id = id()?;
            let branch = agent_session::cleanup(&id)?;
            println!("Removed the worktree of contract session {id}; its branch {branch} is kept.");
            Ok(())
        }
        Some("sweep") => {
            let timed_out = agent_session::sweep()?;
            if timed_out.is_empty() {
                println!("No session timed out.");
            }
            for id in timed_out {
                println!("Timed out contract session {id}; it keeps no authority until resumed.");
            }
            Ok(())
        }
        Some("quarantine") => {
            let id = id()?;
            agent_session::quarantine(&id, &reason())?;
            println!("Quarantined contract session {id}; a human must resume it.");
            Ok(())
        }
        Some("extend") => {
            let id = id()?;
            let number = |key: &str| {
                option(args, key)
                    .map(|value| value.parse::<u64>().map_err(|_| format!("{key} must be a number")))
                    .transpose()
                    .map(Option::unwrap_or_default)
            };
            agent_session::extend(&id, number("--actions")?, number("--files")?)?;
            println!("Extended the budget of contract session {id}.");
            Ok(())
        }
        Some(other) => Err(format!(
            "unknown agent session operation '{other}'; use list, show, start, resume, cancel, finalize, verify, cleanup, sweep, quarantine, or extend"
        )),
    }
}

/** Codex events Crane registers, with whether their hook entry needs a tool matcher */
const CODEX_EVENTS: [(HookEvent, bool); 7] = [
    (HookEvent::SessionStart, false),
    (HookEvent::UserPromptSubmit, false),
    (HookEvent::PreToolUse, true),
    (HookEvent::PermissionRequest, true),
    (HookEvent::PostToolUse, true),
    (HookEvent::Stop, false),
    (HookEvent::SessionEnd, false),
];

/** Install Codex hooks for the repository additively and idempotently, by reading
 * <repo>/.codex/hooks.json (refusing to touch a file that is not a JSON object with an object
 * "hooks" field), skipping every event whose Crane command is already registered there or inline
 * in .codex/config.toml, appending one Crane-owned matcher group for each remaining event, and
 * writing the file through a temporary file; unrelated settings and hooks are never changed
 * Input
    - None
 * Output
    - Result<Vec<String>, String> names of the events that were added (empty when already installed)
    - Error if the repository root cannot be found or the configuration cannot be read or written
*/
fn install_codex_hooks() -> Result<Vec<String>, String> {
    let root = PathBuf::from(crate::repository::git(&["rev-parse", "--show-toplevel"])?);
    let directory = root.join(".codex");
    let path = directory.join("hooks.json");
    let mut document = match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str::<Value>(&content).map_err(|error| {
            format!(
                "{} is not valid JSON ({error}); fix it before installing Crane hooks",
                path.display()
            )
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let inline = fs::read_to_string(directory.join("config.toml")).unwrap_or_default();
    let hooks = document
        .as_object_mut()
        .ok_or_else(|| format!("{} must contain a JSON object", path.display()))?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| format!("\"hooks\" in {} must be an object", path.display()))?;
    let mut added = Vec::new();
    for (event, with_matcher) in CODEX_EVENTS {
        let name = event.host_name();
        let command = format!("crane agent hook --event {name} --profile codex");
        if inline.contains(&command) {
            continue; // already registered inline in .codex/config.toml
        }
        let groups = hooks
            .entry(name)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("hooks.{name} in {} must be an array", path.display()))?;
        let registered = groups.iter().any(|group| {
            group["hooks"].as_array().is_some_and(|handlers| {
                handlers
                    .iter()
                    .any(|handler| handler["command"] == command.as_str())
            })
        });
        if registered {
            continue;
        }
        let mut group = json!({ "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 120,
            "statusMessage": format!("Crane: {}", event.name()),
        }] });
        if with_matcher {
            group["matcher"] = json!("*");
        }
        groups.push(group);
        added.push(name.to_string());
    }
    if added.is_empty() {
        println!(
            "Crane Codex hooks are already installed in {}",
            path.display()
        );
        return Ok(added);
    }
    fs::create_dir_all(&directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let temporary = directory.join(format!("hooks.json.crane-{}", std::process::id()));
    let text = serde_json::to_string_pretty(&document).map_err(|error| error.to_string())? + "\n";
    fs::write(&temporary, text)
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("could not write {}: {error}", path.display())
    })?;
    println!(
        "Installed Crane Codex hooks for {} in {}",
        added.join(", "),
        path.display()
    );
    println!("Codex runs project hooks only after they are trusted: open Codex in this repository and review them with /hooks.");
    println!("The hooks use the Crane executable available on PATH.");
    Ok(added)
}

/** Install Claude Code hooks for the project, by first requiring the claude profile, then creating
 * .claude, refusing to overwrite an existing settings.local.json, and finally writing the Crane
 * hook and permission settings
 * Input
    - profile: AgentKind - selected agent profile
 * Output
    - Result<(), String>
    - Error if the profile is not claude, the settings file already exists, or writing fails
*/
fn install_hooks(profile: AgentKind) -> Result<(), String> {
    // Register project-local hooks so Claude starts Crane as a separate process
    if profile != AgentKind::Claude {
        return Err(
            "automatic hooks are supported for --profile claude and --profile codex".into(),
        );
    }

    let directory = Path::new(".claude");
    fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let path = directory.join("settings.local.json");
    if path.exists() {
        return Err(format!(
            "{} already exists; review it and merge the Crane hooks manually, or remove it before reinstalling",
            path.display()
        ));
    }
    let hook = |event: &str| {
        json!([{ "hooks": [{
            "type": "command",
            "command": format!("crane agent hook --event {event} --profile claude"),
        }] }])
    };
    let with_matcher = |event: &str| {
        let mut entry = hook(event);
        entry[0]["matcher"] = json!("*");
        entry
    };
    let settings = json!({
        "permissions": {
            "deny": [
                "Edit(./.crane/**)",
                "Edit(./.claude/settings.json)",
                "Edit(./.claude/settings.local.json)"
            ]
        },
        "hooks": {
            "SessionStart": hook("session-start"),
            "UserPromptSubmit": hook("user-prompt-submit"),
            "PreToolUse": with_matcher("pre-tool-use"),
            "PostToolUse": with_matcher("post-tool-use"),
            "Stop": hook("stop"),
            "SessionEnd": hook("session-end"),
        }
    });
    let text = serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())? + "\n";
    fs::write(&path, text)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    println!("Installed Claude Code hooks in {}", path.display());
    println!("The hooks use the Crane executable available on PATH.");
    Ok(())
}
