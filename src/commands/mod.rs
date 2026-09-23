use std::env;
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
    init::run()
}

pub(crate) fn verify_for_agent() -> Result<(), String> {
    check::run(true, true)
}

fn agent(args: &[String]) -> Result<(), String> {
    let operation = args.first().map(String::as_str).unwrap_or("verify");
    let profile = args
        .iter()
        .position(|argument| argument == "--profile" || argument == "--agent")
        .and_then(|index| args.get(index + 1))
        .map(String::as_str);
    let selected = AgentKind::parse(profile)?;
    let adapter = adapter(selected);
    match operation {
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

fn version() -> Result<(), String> {
    println!("crane {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}

fn help() -> Result<(), String> {
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
  test-all
  context
  status

TARGET is a language-neutral qualified function name, normally Type.method
(or just function for a top-level function). The source language is inferred
from the file extension."#
    );
    Ok(())
}
