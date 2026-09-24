use std::env;
use std::fs;
use std::path::Path;

use crate::adapter::{adapter, AgentKind};

mod check;
mod checkpoint;
mod context;
mod init;
mod parse;
mod protect;
mod status;

#[cfg(test)]
mod tests;

pub(crate) fn run() -> Result<(), String> {
    // Keep all CLI entry points in one dispatcher so hooks and direct commands share behavior
    let mut arguments = env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "help".into());
    let rest: Vec<String> = arguments.collect();
    match command.as_str() {
        "help" => help(),
        "--version" | "-V" | "version" => version(),
        "init" => init::run(),
        "checkpoint" => checkpoint::run(&rest),
        "protect" => protect::run(&rest),
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

pub(crate) fn initialize_for_agent() -> Result<(), String> {
    // Agent initialization creates repository state before the first verification
    init::run()
}

pub(crate) fn verify_for_agent() -> Result<(), String> {
    // Agent verification always requests the stable machine-readable contract
    check::run(true, true)
}

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
                .ok_or("agent hook requires --event session-start|user-prompt-submit|post-tool-use|stop")?;
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

fn run_agent_hook(event: &str, profile: AgentKind) -> Result<(), String> {
    // Map Claude lifecycle events to context loading or independent verification
    if profile != AgentKind::Claude {
        return Err("automatic hooks are currently supported only for --profile claude".into());
    }
    match event {
        "session-start" => context::run(),
        "user-prompt-submit" | "post-tool-use" | "stop" => verify_for_hook(),
        _ => Err(format!(
            "unknown hook event '{event}'; use session-start, user-prompt-submit, post-tool-use, or stop"
        )),
    }
}

fn verify_for_hook() -> Result<(), String> {
    // Prefix hook failures so main can return Claude's blocking exit code
    verify_for_agent().map_err(|error| format!("HOOK_BLOCK:{error}"))
}

fn version() -> Result<(), String> {
    // Report the package version compiled into this executable
    println!("crane {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}

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
  agent hook --event session-start|user-prompt-submit|post-tool-use|stop --profile claude
  test-all
  context
  status

TARGET is a language-neutral qualified function name, normally Type.method
(or just function for a top-level function). The source language is inferred
from the file extension."#
    );
    Ok(())
}
