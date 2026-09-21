use std::env;
use std::path::Path;

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
        "check" => check::run(rest.iter().any(|argument| argument == "--json")),
        "test-all" => check::run(false),
        "context" => context::run(),
        "status" => status::run(),
        _ => Err(format!("unknown command '{command}'. Run 'crane help'.")),
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
  test-all
  context
  status

TARGET is a language-neutral qualified function name, normally Type.method
(or just function for a top-level function). The source language is inferred
from the file extension."#
    );
    Ok(())
}
