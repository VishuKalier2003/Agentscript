use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

use crate::adapter::{adapter, AgentKind};   // binary compilation of the project is termed as crate, here using the adapter.rs file
use serde_json::Value;

mod check;
mod checkpoint;
mod context;
mod init;
mod parse;
mod protect;
mod status;

#[cfg(test)]  // Compile the module only when running tests, not in production builds
mod tests;

/** Act as the gateway for every CLI command, by first reading the command name from the process
 * arguments (default help), collecting the remaining arguments, and then matching the name to the
 * handler that implements it
 * Input
    - None (arguments are read from the process environment)
 * Output
    - Result<(), String>
    - Error if the command is unknown or its handler fails
*/
pub(crate) fn run() -> Result<(), String> {
    // Keep all CLI entry points in one dispatcher so hooks and direct commands share behavior
    let mut arguments = env::args().skip(1);  // Keeping arguments mutable since iterator uses pointer and loc may shift
    let command = arguments.next().unwrap_or_else(|| "help".into());
    let rest: Vec<String> = arguments.collect();    // converts one collection to another, here Iterator to vector
    match command.as_str() {
        "help" => help(),
        "--version" | "-V" | "version" => version(),
        "init" => init::run(),    // creates the metadata directories at root level
        "checkpoint" => checkpoint::run(&rest),   // creates checkpoint metadata for the current git commit
        "protect" => protect::run(&rest),   // command line for the preserve --function call
        "parse" => {
            let path = rest.first().ok_or("parse requires a .crane file")?;
            parse::run(Path::new(path))
        }

        "check" => check::run(
            rest.iter().any(|argument| argument == "--json"),
            rest.iter().any(|argument| argument == "--agent"),
        ),
        "test-all" => check::run(false, false),
        "context" => context::run(),
        "status" => status::run(),
        "agent" => agent(&rest),
        _ => Err(format!("unknown command '{command}'. Run 'crane help'.")),
    }
}

/** Initialize Crane on behalf of an agent adapter, by delegating to the init command
 * Input
    - None
 * Output
    - Result<(), String>
    - Error if initialization fails
*/
pub(crate) fn initialize_for_agent() -> Result<(), String> {
    // Agent initialization creates repository state before the first verification
    init::run()
}

/** Verify policies on behalf of an agent adapter, by running check in JSON and agent mode
 * Input
    - None
 * Output
    - Result<(), String>
    - Error if setup fails or any policy is violated
*/
pub(crate) fn verify_for_agent() -> Result<(), String> {
    // Agent verification always requests the stable machine-readable contract
    check::run(true, true)
}

/** Handle the agent subcommands, by first reading the operation (default verify) and the
 * --profile/--agent value, building the matching adapter, and then routing to install, hook, init,
 * or verify
 * Input
    - args: &[String] - arguments after "agent"
 * Output
    - Result<(), String>
    - Error if the profile or operation is unknown, or the operation fails
*/
fn agent(args: &[String]) -> Result<(), String> {
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
        "install" => install_agent_hooks(selected),
        "hook" => {
            let event = args
                .iter()
                .position(|argument| argument == "--event")
                .and_then(|index| args.get(index + 1))
                .map(String::as_str)
                .ok_or("agent hook requires --event pre-tool-use|session-start|user-prompt-submit|post-tool-use|stop")?;
            run_agent_hook(event, selected)
        }
        "init" => {
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
            "unknown agent operation '{operation}'; use 'crane agent init' or 'crane agent verify'"
        )),
    }
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
fn install_agent_hooks(profile: AgentKind) -> Result<(), String> {
    // Register project-local hooks so Claude starts Crane as a separate process
    if profile != AgentKind::Claude {
        return Err("automatic hooks are currently supported only for --profile claude".into());
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
    let settings = r#"{
  "permissions": {
    "deny": [
      "Edit(./.crane/**)",
      "Edit(./.claude/settings.json)",
      "Edit(./.claude/settings.local.json)"
    ]
  },
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "crane agent hook --event session-start --profile claude"
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "crane agent hook --event user-prompt-submit --profile claude"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit|NotebookEdit",
        "hooks": [
          {
            "type": "command",
            "command": "crane agent hook --event post-tool-use --profile claude"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "crane agent hook --event pre-tool-use --profile claude"
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "crane agent hook --event stop --profile claude"
          }
        ]
      }
    ]
  }
}
"#;
    fs::write(&path, settings)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    println!("Installed Claude Code hooks in {}", path.display());
    println!("The hooks use the Crane executable available on PATH.");
    Ok(())
}

/** Run the handler for one Claude Code hook event, by first requiring the claude profile and then
 * mapping pre-tool-use to the metadata guard, session-start to policy context, and the remaining
 * events to hook verification
 * Input
    - event: &str - hook event name
    - profile: AgentKind - selected agent profile
 * Output
    - Result<(), String>
    - Error if the profile or event is unsupported, or the handler blocks
*/
fn run_agent_hook(event: &str, profile: AgentKind) -> Result<(), String> {
    // Map Claude lifecycle events to context loading or independent verification
    if profile != AgentKind::Claude {
        return Err("automatic hooks are currently supported only for --profile claude".into());
    }
    match event {
        "pre-tool-use" => protect_crane_metadata(),
        "session-start" => context::run(),
        "user-prompt-submit" | "post-tool-use" | "stop" => check::run_hook(event),
        _ => Err(format!(
            "unknown hook event '{event}'; use pre-tool-use, session-start, user-prompt-submit, post-tool-use, or stop"
        )),
    }
}

/** Block agent tool calls that would change Crane's own enforcement, by first reading the
 * PreToolUse JSON from stdin, then checking read-only tools as allowed, file tools by path only,
 * shell tools by command text (protected paths or mutating crane commands), and any other tool by
 * all of its arguments, returning HOOK_BLOCK when protected metadata is touched
 * Input
    - None (hook payload is read from stdin)
 * Output
    - Result<(), String>
    - HOOK_BLOCK error if the input is invalid or the tool call touches protected metadata
*/
fn protect_crane_metadata() -> Result<(), String> {
    // Block metadata edits before they happen so agents cannot weaken their own verifier
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| format!("could not read Claude hook input: {error}"))?;
    if input.trim().is_empty() {
        return Ok(());
    }
    let payload: Value = serde_json::from_str(&input)
        .map_err(|error| format!("HOOK_BLOCK:invalid Claude hook input: {error}"))?;
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_input = payload.get("tool_input").unwrap_or(&payload);
    let touches_metadata = match tool_name {
        // Read-only tools may inspect .crane so agents can explain failures to the user
        "Read" | "Glob" | "Grep" | "LS" | "NotebookRead" | "WebFetch" | "WebSearch"
        | "TodoWrite" | "Task" | "Agent" => false,
        // File tools are checked by path only so editing docs that mention .crane stays allowed
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => ["file_path", "notebook_path", "path"]
            .iter()
            .filter_map(|field| tool_input.get(*field).and_then(Value::as_str))
            .any(references_protected_path),
        "Bash" | "PowerShell" => tool_input
            .get("command")
            .and_then(Value::as_str)
            .map(|command| references_protected_path(command) || runs_mutating_crane(command))
            .unwrap_or(true),
        // Unknown tools (MCP servers, new built-ins) fail closed if any argument names protected metadata
        _ => contains_protected_path(tool_input),
    };
    if touches_metadata {
        return Err(
            "HOOK_BLOCK:agents may not modify .crane metadata, run mutating crane commands, or edit Claude hook settings; a human must review policy, checkpoint, and hook changes"
                .into(),
        );
    }
    Ok(())
}

/** Search a JSON value for a protected path, by recursing through arrays and objects and testing
 * every string it contains
 * Input
    - value: &Value - tool input JSON
 * Output
    - bool, true if any string references protected metadata
*/
fn contains_protected_path(value: &Value) -> bool {
    match value {
        Value::String(text) => references_protected_path(text),
        Value::Array(values) => values.iter().any(contains_protected_path),
        Value::Object(values) => values.values().any(contains_protected_path),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

/** Check whether text references protected metadata, by first normalizing slashes and case, then
 * looking for the Claude settings files, and finally finding each ".crane" occurrence that stands
 * alone as a name (so names like my.crane or .craneignore do not match)
 * Input
    - value: &str - path or command text
 * Output
    - bool, true if the text references .crane or Claude hook settings
*/
fn references_protected_path(value: &str) -> bool {
    // Match .crane as a whole path component anywhere in the text, plus the hook settings that enforce Crane
    let text = value.replace('\\', "/").to_ascii_lowercase();
    if text.contains(".claude/settings.json") || text.contains(".claude/settings.local.json") {
        return true;
    }
    let is_name_character =
        |character: char| character.is_ascii_alphanumeric() || "_-.".contains(character);
    text.match_indices(".crane").any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + ".crane".len()..].chars().next();
        !before.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
            && !after.is_some_and(is_name_character)
    })
}

/** Detect shell commands that change Crane metadata through the CLI, by splitting the command on
 * whitespace and shell separators, finding tokens whose program name is crane or crane.exe, and
 * checking whether the next tokens are checkpoint, protect, init, or agent init/install
 * Input
    - command: &str - shell command text
 * Output
    - bool, true if the command runs a mutating crane subcommand
*/
fn runs_mutating_crane(command: &str) -> bool {
    // Re-baselining or rewriting policies through the CLI would bypass the file guard
    let tokens = command
        .split(|character: char| character.is_whitespace() || "&|;()`\"'".contains(character))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    tokens.iter().enumerate().any(|(index, token)| {
        let program = token.replace('\\', "/").to_ascii_lowercase();
        let program = program.rsplit('/').next().unwrap_or_default();
        if program != "crane" && program != "crane.exe" {
            return false;
        }
        match tokens.get(index + 1).copied() {
            Some("checkpoint" | "protect" | "init") => true,
            Some("agent") => matches!(tokens.get(index + 2).copied(), Some("init" | "install")),
            _ => false,
        }
    })
}

/** Print the crane version, by reading the package version compiled into the binary
 * Input
    - None
 * Output
    - Result<(), String>, always Ok
*/
fn version() -> Result<(), String> {
    // Report the package version compiled into this executable
    println!("crane {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}

/** Print the command reference, by writing a fixed usage text listing every command
 * Input
    - None
 * Output
    - Result<(), String>, always Ok
*/
fn help() -> Result<(), String> {
    // Keep the command contract discoverable without requiring repository setup
    println!(
        r#"Crane MVP

Commands:
  init
  checkpoint [--name NAME]
  protect --function TARGET [--policy NAME] [--checkpoint NAME]
  parse FILE
  check [--json]
  check --agent
  agent init [--profile generic|claude|codex]
  agent verify [--profile generic|claude|codex]
  agent install --profile claude
  agent hook --event pre-tool-use|session-start|user-prompt-submit|post-tool-use|stop --profile claude
  test-all
  context
  status

TARGET is a language-neutral qualified function name, normally Type.method
(or just function for a top-level function). The source language is inferred
from the file extension."#
    );
    Ok(())
}
