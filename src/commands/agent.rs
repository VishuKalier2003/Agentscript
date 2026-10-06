use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::adapter::{adapter, AgentAdapter, AgentKind, HookConfig, HookEvent, ProviderEvent};
use crate::agent_session::{self, SessionOptions};
use crate::authority::{AgentAction, Operation, Runtime, Usage};
use crate::effects;
use crate::ir::ContractSet;
use crate::repository::ensure_initialized;
use crate::scope::ScopeContext;
use crate::session::{read_attestation, session_ids, ContractSession, Lifecycle};
use crate::util::option;
use crate::verify::{assess, setup_report, verify_contracts, Assessment, Report};
use crate::zones::model::{Autonomy, SafetyState};

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
    let json = args.iter().any(|argument| argument == "--json");
    match operation {
        "install" => install(adapter.as_ref()).map(|_| ()),
        "uninstall" => uninstall(adapter.as_ref()),
        "hooks" => {
            let report = validate(adapter.as_ref())?;
            print_hooks(&report, json)?;
            match report["valid"] == true {
                true => Ok(()),
                false => Err(format!(
                    "the {} hooks are not valid; run 'crane agent install --profile {}'",
                    selected.name(),
                    selected.name()
                )),
            }
        }
        "status" => status(args, profile.map(|_| selected), json),
        "hook" => {
            let event = option(args, "--event").ok_or(
                "agent hook requires --event session-start|user-prompt-submit|pre-tool-use|post-tool-use|permission-request|stop|session-end (or the host spelling, such as PreToolUse)",
            )?;
            let event = HookEvent::parse(&event)?;
            let explicit = option(args, "--crane-session").or_else(|| {
                env::var("CRANE_SESSION")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            });
            let options = match SessionOptions::from_args(args) {
                Ok(options) => options,
                Err(error) => return adapter.respond_failure(event, &error, false),
            };
            // A crash must never pass for a decision: a panic becomes the provider's failure answer
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hook(event, adapter.as_ref(), &options, explicit.as_deref())
            }));
            std::panic::set_hook(previous);
            match outcome {
                Ok(result) => result,
                Err(panic) => {
                    let message = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|text| text.to_string()))
                        .unwrap_or_else(|| "unknown error".into());
                    adapter.respond_failure(event, &format!("Crane failed internally ({message})"), false)
                }
            }
        }
        "session" => session(&args[1..], selected),
        "init" => {
            if selected == AgentKind::Codex {
                // Installation is additive and idempotent, so init can always (re)install it
                super::init::run()?;
                install(adapter.as_ref())?;
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
            "unknown agent operation '{operation}'; use init, verify, install, uninstall, hooks, status, hook, or session"
        )),
    }
}

/** Run one lifecycle hook: read and translate the provider payload (the adapter only translates),
 * check that the payload is the event it was registered for, bind it to its Society session
 * (agent_session::bind: an explicitly named session, the task's approved session, the provider's
 * session, or a transient one), check that the agent works in that session's repository, record the
 * provider's attachment, and hand the normalized event to the session manager and authority
 * engine. Whenever no Society decision or verification can be obtained, the adapter answers with
 * its failure response, which never allows a tool call or reports success
 * Input
    - event: HookEvent - lifecycle event
    - adapter: &dyn AgentAdapter - provider adapter
    - options: &SessionOptions - lifetime, idle timeout, task, autonomy mode, and budget, used
      only when the session is created
    - explicit: Option<&str> - Society session named with CRANE_SESSION or --crane-session
 * Output
    - Result<(), String>
    - HOOK_BLOCK error when the action is denied or the agent must repair something now
*/
fn hook(
    event: HookEvent,
    adapter: &dyn AgentAdapter,
    options: &SessionOptions,
    explicit: Option<&str>,
) -> Result<(), String> {
    let payload = match read_payload(event) {
        Ok(payload) => payload,
        Err(error) => return adapter.respond_failure(event, &error, false),
    };
    let provider = adapter.translate(event, &payload);
    let held = provider.stop_hook_active;
    match bound_hook(event, adapter, provider, options, explicit) {
        Ok(()) => Ok(()),
        Err(error) if error.starts_with("HOOK_BLOCK:") => Err(error),
        Err(error) => adapter.respond_failure(event, &error, held),
    }
}

/** Bind a translated event to its session and handle it (see hook)
 * Input
    - event: HookEvent - lifecycle event
    - adapter: &dyn AgentAdapter - provider adapter
    - provider: ProviderEvent - translated payload
    - options: &SessionOptions - session options
    - explicit: Option<&str> - explicitly named Society session
 * Output
    - Result<(), String>
*/
fn bound_hook(
    event: HookEvent,
    adapter: &dyn AgentAdapter,
    provider: ProviderEvent,
    options: &SessionOptions,
    explicit: Option<&str>,
) -> Result<(), String> {
    if let Some(name) = provider.event_name.as_deref() {
        if name != event.host_name() && name != event.name() {
            return Err(format!(
                "the payload is a {name} event, but this hook handles {}; check the hook configuration",
                event.host_name()
            ));
        }
    }
    if let Err(error) = ensure_initialized() {
        return unbound(event, adapter, provider, &error, false);
    }
    let binding = match agent_session::bind(
        adapter.kind(),
        explicit,
        provider.session.as_deref(),
        options,
    ) {
        Ok(binding) => binding,
        Err(error) => return unbound(event, adapter, provider, &error, true),
    };
    if let Err(error) = agent_session::check_workspace(&binding.session, provider.cwd.as_deref()) {
        return unbound(event, adapter, provider, &error, true);
    }
    let mut facts = provider.identity.clone();
    facts["model"] = json!(provider.model);
    facts["cwd"] = json!(provider.cwd);
    facts["first_event"] = json!(event.host_name());
    agent_session::attach(
        &binding.session,
        adapter.kind(),
        provider.session.as_deref(),
        binding.via,
        facts,
    )?;
    handle(event, adapter, provider, &binding.session)
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
            crate::session_orchestrator::connected(session, "session_start")?;
            print!("{context}");
            Ok(())
        }
        HookEvent::UserPromptSubmit => {
            let released = crate::budget::manage::pending(session);
            let report = verify_contracts(session.contracts(), &mut ScopeContext::new(), &|_| true);
            session.record(json!({
                "event": "user_prompt_submit",
                "budget_released": released,
                "verification": outcome(&report),
            }))?;
            adapter.respond_verification(event, assess(&report, false), &report)
        }
        HookEvent::PreToolUse => {
            let action = provider.action.unwrap_or_else(unknown_action);
            let (_, verdict) =
                crate::session_orchestrator::decide(session, protected, &action, "pre_tool_use")?;
            adapter.respond_action(&verdict)
        }
        HookEvent::PermissionRequest => {
            let action = provider.action.unwrap_or_else(unknown_action);
            let (_, verdict) = crate::session_orchestrator::decide(
                session,
                protected,
                &action,
                "permission_request",
            )?;
            adapter.respond_permission(&verdict)
        }
        HookEvent::PostToolUse => {
            let action = provider.action.unwrap_or_else(unknown_action);
            // Observation, incremental verification, and the state updates belong to the orchestrator
            let report = crate::session_orchestrator::observe(session, &action, protected)?;
            adapter.respond_verification(event, assess(&report, false), &report)
        }
        HookEvent::Stop => {
            let released = crate::budget::manage::pending(session);
            let (report, attestation) = effects::validate(session)?;
            session.write_attestation(&attestation)?;
            session.record(json!({
                "event": "stop",
                "budget_released": released,
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
                "provider_session": provider.session,
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
                autonomy: Autonomy::Observe,
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

/** Read the hook payload from stdin, treating empty input as Null; unreadable input or invalid
 * JSON is an error for tool events (which the adapter then answers as a failure, never an allow)
 * and Null for every other event
 * Input
    - event: HookEvent - lifecycle event
 * Output
    - Result<Value, String>
    - HOOK_BLOCK error if pre-tool-use input cannot be read or parsed
*/
fn read_payload(event: HookEvent) -> Result<Value, String> {
    let mut input = String::new();
    if let Err(error) = io::stdin().read_to_string(&mut input) {
        return match event.has_action() {
            true => Err(format!("could not read hook input: {error}")),
            false => Ok(Value::Null),
        };
    }
    if input.trim().is_empty() {
        return Ok(Value::Null);
    }
    match serde_json::from_str(&input) {
        Ok(payload) => Ok(payload),
        Err(error) if event.has_action() => Err(format!("invalid hook input: {error}")),
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
            let state = agent_session::resume(&id)?;
            println!("Resumed contract session {id}; autonomy {}, safety {}.", state.autonomy.name(), state.safety.name());
            if let Some(reason) = &state.reason {
                println!("Still {}: {reason}. Run 'crane autonomy status {id}' for what recovery needs.", state.safety.name());
            }
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
            println!("Final attestation: {}", attestation["final_attestation"]["attestation_digest"].as_str().unwrap_or("unavailable"));
            let tests = &attestation["contract_tests"];
            if !tests.is_null() {
                println!("Contract tests (mandatory): {} passed, {} failed, {} not applicable", tests["passed"], tests["failed"], tests["not_applicable"]);
            }
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
            println!("Quarantined contract session {id}; 'crane autonomy status {id}' lists what recovery needs.");
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

/** Read a provider's hook configuration file: its path, its JSON document ({} when missing), and
 * whether it existed; a file that is not a JSON object is an error, so it is never overwritten
 * Input
    - config: &HookConfig - provider configuration
 * Output
    - Result<(PathBuf, Value, bool), String>
*/
fn read_config(config: &HookConfig) -> Result<(PathBuf, Value, bool), String> {
    let root = PathBuf::from(crate::repository::git(&["rev-parse", "--show-toplevel"])?);
    let path = root.join(config.file);
    let (document, exists) = match fs::read_to_string(&path) {
        Ok(content) => (
            serde_json::from_str::<Value>(&content).map_err(|error| {
                format!(
                    "{} is not valid JSON ({error}); fix it before changing Crane hooks",
                    path.display()
                )
            })?,
            true,
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (json!({}), false),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    if !document.is_object() {
        return Err(format!("{} must contain a JSON object", path.display()));
    }
    if !document.get("hooks").is_none_or(Value::is_object) {
        return Err(format!("\"hooks\" in {} must be an object", path.display()));
    }
    Ok((path, document, exists))
}

/** Write a configuration file through a temporary file
 * Input
    - path: &Path - file
    - document: &Value - JSON
 * Output
    - Result<(), String>
*/
fn write_config(path: &Path, document: &Value) -> Result<(), String> {
    let directory = path.parent().ok_or("invalid configuration path")?;
    fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let temporary = path.with_extension(format!("crane-{}", std::process::id()));
    let text = serde_json::to_string_pretty(document).map_err(|error| error.to_string())? + "\n";
    fs::write(&temporary, text)
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("could not write {}: {error}", path.display())
    })
}

/** Return the provider configuration that registers hooks inline (Codex's config.toml next to
 * hooks.json), empty when there is none
 * Input
    - path: &Path - hook configuration file
 * Output
    - String
*/
fn inline_config(path: &Path) -> String {
    path.parent()
        .map(|folder| fs::read_to_string(folder.join("config.toml")).unwrap_or_default())
        .unwrap_or_default()
}

/** Install a provider's hooks additively and idempotently: every event Crane needs gets one
 * Crane-owned handler group (skipped when the command is already registered, in the file or inline
 * in config.toml), Crane's permission rules are added when missing, unrelated settings and hooks are
 * never changed, and an unchanged configuration is not rewritten
 * Input
    - adapter: &dyn AgentAdapter - provider adapter
 * Output
    - Result<Vec<String>, String> events added (empty when already installed)
*/
fn install(adapter: &dyn AgentAdapter) -> Result<Vec<String>, String> {
    let config = adapter
        .hook_config()
        .ok_or("automatic hooks are supported for --profile claude and --profile codex")?;
    let (path, mut document, _) = read_config(&config)?;
    let inline = inline_config(&path);
    let mut added = Vec::new();
    {
        let hooks = document
            .as_object_mut()
            .ok_or("invalid configuration")?
            .entry("hooks")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or("invalid configuration")?;
        for (event, with_matcher) in config.events {
            let name = event.host_name();
            let command = adapter.hook_command(*event);
            if inline.contains(&command) {
                continue;
            }
            let groups = hooks
                .entry(name)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or_else(|| format!("hooks.{name} in {} must be an array", path.display()))?;
            if registrations(groups, &command) > 0 {
                continue;
            }
            let mut handler =
                json!({"type": "command", "command": command, "timeout": config.timeout});
            if adapter.kind() == AgentKind::Codex {
                handler["statusMessage"] = json!(format!("Crane: {}", event.name()));
            }
            let mut group = json!({ "hooks": [handler] });
            if *with_matcher {
                group["matcher"] = json!("*");
            }
            groups.push(group);
            added.push(name.to_string());
        }
    }
    let mut denied = Vec::new();
    if !config.deny.is_empty() {
        let rules = document
            .as_object_mut()
            .ok_or("invalid configuration")?
            .entry("permissions")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| format!("\"permissions\" in {} must be an object", path.display()))?
            .entry("deny")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("permissions.deny in {} must be an array", path.display()))?;
        for rule in config.deny {
            if !rules.iter().any(|existing| existing == rule) {
                rules.push(json!(rule));
                denied.push(*rule);
            }
        }
    }
    if added.is_empty() && denied.is_empty() {
        println!(
            "Crane {} hooks are already installed in {}",
            display(adapter.kind()),
            path.display()
        );
        return Ok(added);
    }
    write_config(&path, &document)?;
    println!(
        "Installed Crane {} hooks{} in {}",
        display(adapter.kind()),
        if added.is_empty() {
            String::new()
        } else {
            format!(" for {}", added.join(", "))
        },
        path.display()
    );
    if adapter.kind() == AgentKind::Codex {
        println!("Codex runs project hooks only after they are trusted: open Codex in this repository and review them with /hooks.");
    }
    println!("The hooks use the Crane executable available on PATH; check them with 'crane agent hooks --profile {}'.", adapter.kind().name());
    Ok(added)
}

/** Return a provider's display name for messages
 * Input
    - kind: AgentKind - provider profile
 * Output
    - &'static str
*/
fn display(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "Claude Code",
        AgentKind::Codex => "Codex",
        AgentKind::Generic => "generic",
    }
}

/** Count the handlers in a hook event's groups that run a command
 * Input
    - groups: &[Value] - matcher groups of one event
    - command: &str - command
 * Output
    - usize
*/
fn registrations(groups: &[Value], command: &str) -> usize {
    groups
        .iter()
        .flat_map(|group| group["hooks"].as_array().cloned().unwrap_or_default())
        .filter(|handler| handler["command"] == command)
        .count()
}

/** Find the Crane executable the hooks will run: crane (crane.exe) on PATH
 * Input
    - None
 * Output
    - Option<PathBuf>
*/
fn crane_on_path() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["crane.exe", "crane"]
    } else {
        &["crane"]
    };
    env::split_paths(&env::var_os("PATH")?)
        .flat_map(|folder| names.iter().map(move |name| folder.join(name)))
        .find(|path| path.is_file())
}

/** Validate a provider's hook configuration: the file parses; every event Crane needs runs Crane's
 * command exactly once (in the file or inline), with a matcher that covers every tool on tool
 * events; Crane's permission rules are present; the Crane executable is on PATH (a hook that
 * cannot run is a non-blocking error to the provider, which would let tools run unchecked); and
 * Crane is initialized
 * Input
    - adapter: &dyn AgentAdapter - provider adapter
 * Output
    - Result<Value, String> {profile, file, exists, events, permissions, executable, problems,
      warnings, valid}
*/
pub(crate) fn validate(adapter: &dyn AgentAdapter) -> Result<Value, String> {
    let config = adapter
        .hook_config()
        .ok_or("hook validation is supported for --profile claude and --profile codex")?;
    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    let (path, document, exists) = match read_config(&config) {
        Ok(read) => read,
        Err(error) => {
            return Ok(
                json!({"profile": adapter.kind().name(), "file": config.file, "exists": true, "valid": false, "problems": [error], "warnings": [], "events": []}),
            )
        }
    };
    let inline = inline_config(&path);
    if !exists && inline.is_empty() {
        problems.push(format!(
            "{} does not exist; run 'crane agent install --profile {}'",
            config.file,
            adapter.kind().name()
        ));
    }
    let mut events = Vec::new();
    for (event, with_matcher) in config.events {
        let name = event.host_name();
        let command = adapter.hook_command(*event);
        let groups = document["hooks"][name]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let count = registrations(&groups, &command) + usize::from(inline.contains(&command));
        let matchers = groups
            .iter()
            .filter(|group| registrations(std::slice::from_ref(*group), &command) > 0)
            .map(|group| group["matcher"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>();
        match count {
            0 => problems.push(format!("{name} is not registered, so Crane never sees it")),
            1 => {}
            many => problems.push(format!(
                "{name} runs Crane {many} times; remove the duplicates"
            )),
        }
        if *with_matcher {
            for matcher in &matchers {
                if !matches!(matcher.as_str(), "" | "*" | ".*") {
                    problems.push(format!("{name} is registered with matcher '{matcher}', so other tools run without Crane's authorization"));
                }
            }
        }
        events.push(
            json!({"event": name, "command": command, "registered": count, "matchers": matchers}),
        );
    }
    let missing = config
        .deny
        .iter()
        .filter(|rule| {
            !document["permissions"]["deny"]
                .as_array()
                .is_some_and(|rules| rules.iter().any(|existing| existing == **rule))
        })
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        problems.push(format!(
            "permission rules missing: {}",
            missing
                .iter()
                .map(|rule| rule.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let executable = crane_on_path();
    if executable.is_none() {
        problems.push("crane is not on PATH: the provider could not run the hooks, and a hook that cannot run does not stop a tool call".into());
    }
    if ensure_initialized().is_err() {
        problems.push("Crane is not initialized in this repository; run 'crane init'".into());
    }
    if adapter.kind() == AgentKind::Codex {
        warnings.push("Codex runs project hooks only after they are trusted; review them with /hooks in Codex".to_string());
    }
    Ok(json!({
        "profile": adapter.kind().name(),
        "file": config.file,
        "exists": exists,
        "events": events,
        "permissions": {"required": config.deny, "missing": missing},
        "executable": executable.map(|path| path.to_string_lossy().into_owned()),
        "problems": problems,
        "warnings": warnings,
        "valid": problems.is_empty(),
    }))
}

/** Print a hook validation report
 * Input
    - report: &Value - from validate
    - json: bool - print JSON
 * Output
    - Result<(), String>
*/
fn print_hooks(report: &Value, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    println!(
        "{} hooks ({}): {}",
        report["profile"].as_str().unwrap_or_default(),
        report["file"].as_str().unwrap_or_default(),
        if report["valid"] == true {
            "valid"
        } else {
            "NOT VALID"
        }
    );
    for problem in report["problems"].as_array().into_iter().flatten() {
        println!("  problem: {}", problem.as_str().unwrap_or_default());
    }
    for warning in report["warnings"].as_array().into_iter().flatten() {
        println!("  note: {}", warning.as_str().unwrap_or_default());
    }
    Ok(())
}

/** Remove a provider's Crane hooks (disconnect it), refused on behalf of an agent: only handlers
 * running Crane's commands for
 * that provider and Crane's permission rules are removed, groups and events left empty are
 * dropped, every other setting stays, and a file that only held Crane's configuration is deleted;
 * the provider's sessions stay as evidence (their authority ends with them)
 * Input
    - adapter: &dyn AgentAdapter - provider adapter
 * Output
    - Result<(), String>
*/
fn uninstall(adapter: &dyn AgentAdapter) -> Result<(), String> {
    if let Some(marker) = crate::proposals::store::agent_environment() {
        return Err(format!("crane agent uninstall refuses to run in an agent environment ({marker} is set); only a human disconnects Crane from an agent"));
    }
    let config = adapter
        .hook_config()
        .ok_or("hook removal is supported for --profile claude and --profile codex")?;
    let (path, mut document, exists) = read_config(&config)?;
    if !exists {
        println!(
            "Crane {} hooks are not installed ({} does not exist).",
            display(adapter.kind()),
            path.display()
        );
        return Ok(());
    }
    let commands = config
        .events
        .iter()
        .map(|(event, _)| adapter.hook_command(*event))
        .collect::<Vec<_>>();
    let mut removed = 0;
    if let Some(hooks) = document.get_mut("hooks").and_then(Value::as_object_mut) {
        for groups in hooks.values_mut() {
            if let Some(groups) = groups.as_array_mut() {
                for group in groups.iter_mut() {
                    if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                        let before = handlers.len();
                        handlers.retain(|handler| {
                            !handler["command"]
                                .as_str()
                                .is_some_and(|command| commands.iter().any(|ours| ours == command))
                        });
                        removed += before - handlers.len();
                    }
                }
                groups.retain(|group| {
                    group["hooks"]
                        .as_array()
                        .is_none_or(|handlers| !handlers.is_empty())
                });
            }
        }
        hooks.retain(|_, groups| groups.as_array().is_none_or(|groups| !groups.is_empty()));
    }
    let mut rules = 0;
    if let Some(deny) = document
        .pointer_mut("/permissions/deny")
        .and_then(Value::as_array_mut)
    {
        let before = deny.len();
        deny.retain(|rule| !config.deny.iter().any(|ours| rule == ours));
        rules = before - deny.len();
    }
    for (parent, key) in [("/permissions", "deny"), ("", "permissions"), ("", "hooks")] {
        let empty = document
            .pointer(&format!("{parent}/{key}"))
            .is_some_and(|value| {
                value.as_array().is_some_and(Vec::is_empty)
                    || value.as_object().is_some_and(serde_json::Map::is_empty)
            });
        if empty {
            if let Some(object) = (if parent.is_empty() {
                Some(&mut document)
            } else {
                document.pointer_mut(parent)
            })
            .and_then(Value::as_object_mut)
            {
                object.remove(key);
            }
        }
    }
    if removed == 0 && rules == 0 {
        println!(
            "Crane {} hooks are not installed in {}.",
            display(adapter.kind()),
            path.display()
        );
        return Ok(());
    }
    if document.as_object().is_some_and(serde_json::Map::is_empty) {
        fs::remove_file(&path)
            .map_err(|error| format!("could not remove {}: {error}", path.display()))?;
        println!("Removed {} (it only held Crane's hooks).", path.display());
    } else {
        write_config(&path, &document)?;
        println!("Removed {removed} Crane hooks and {rules} permission rules from {}; other settings are unchanged.", path.display());
    }
    let active = session_ids()?
        .into_iter()
        .filter_map(|id| ContractSession::load(&id).ok().flatten())
        .filter(|session| session.identity().0 == adapter.kind() && session.resumable())
        .map(|session| session.id().to_string())
        .collect::<Vec<_>>();
    if !active.is_empty() {
        println!("{} sessions can still be resumed: {}; end them with 'crane agent session cancel ID' or 'finalize ID'.", adapter.kind().name(), active.join(", "));
    }
    Ok(())
}

/** Report provider status: the hook validation of each provider, and the sessions selected by
 * --session, --task, or (by default) every resumable session of the profile, with their provider
 * attachments, task contract, checkpoints, autonomy, safety, budget, and last event
 * Input
    - args: &[String] - arguments after "agent"
    - profile: Option<AgentKind> - --profile, when given
    - json: bool - print JSON
 * Output
    - Result<(), String>
*/
fn status(args: &[String], profile: Option<AgentKind>, json: bool) -> Result<(), String> {
    ensure_initialized()?;
    let kinds = match profile {
        Some(kind) => vec![kind],
        None => vec![AgentKind::Claude, AgentKind::Codex],
    };
    let hooks = kinds
        .iter()
        .filter_map(|kind| validate(adapter(*kind).as_ref()).ok())
        .collect::<Vec<_>>();
    let wanted = option(args, "--session");
    let task = option(args, "--task");
    let mut sessions = Vec::new();
    for id in session_ids()? {
        let Ok(Some(session)) = ContractSession::load(&id) else {
            continue;
        };
        let (kind, bound, _) = session.identity();
        let selected = match (&wanted, &task) {
            (Some(wanted), _) => *wanted == id,
            (None, Some(task)) => bound == Some(task.as_str()),
            (None, None) => kinds.contains(&kind) && session.resumable(),
        };
        if selected {
            sessions.push(agent_session::status_report(&session));
        }
    }
    if let (Some(wanted), true) = (&wanted, sessions.is_empty()) {
        return Err(format!("no contract session '{wanted}'"));
    }
    let report = json!({"hooks": hooks, "sessions": sessions});
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    for hook in &hooks {
        print_hooks(hook, false)?;
    }
    if sessions.is_empty() {
        println!("No active sessions.");
    }
    let text = |value: &Value| value.as_str().map_or_else(|| "-".to_string(), String::from);
    for session in &sessions {
        println!(
            "{} ({}, {}): task {}, contract {} {}, autonomy {}, safety {}, budget {}/{} actions, last {}",
            text(&session["session_id"]),
            text(&session["agent"]),
            text(&session["lifecycle"]),
            text(&session["task_id"]),
            text(&session["task_contract"]["contract_id"]),
            match session["task_contract"]["current"].as_bool() {
                Some(true) => "current".to_string(),
                Some(false) => format!("OBSOLETE: {}", text(&session["task_contract"]["obsolete"])),
                None => text(&session["task_contract"]["status"]),
            },
            text(&session["autonomy"]),
            text(&session["safety"]),
            session["budget"]["actions"],
            session["budget"]["max_actions"],
            text(&session["last_event"]["event"])
        );
        for checkpoint in session["checkpoints"].as_array().into_iter().flatten() {
            println!(
                "  checkpoint {} {} (policy {})",
                text(&checkpoint["checkpoint"]),
                text(&checkpoint["sha"]),
                text(&checkpoint["policy"])
            );
        }
        for attachment in session["attachments"].as_array().into_iter().flatten() {
            println!(
                "  attached: {} session {} via {}",
                text(&attachment["provider"]),
                text(&attachment["provider_session"]),
                text(&attachment["via"])
            );
        }
    }
    Ok(())
}
