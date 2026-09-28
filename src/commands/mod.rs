use std::env;
use std::path::Path;

mod agent;
mod check;
mod checkpoint;
mod context;
mod init;
mod parse;
mod protect;
mod status;
mod target;

#[cfg(test)] // Compile the module only when running tests, not in production builds
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
    let mut arguments = env::args().skip(1); // Keeping arguments mutable since iterator uses pointer and loc may shift
    let command = arguments.next().unwrap_or_else(|| "help".into());
    let rest: Vec<String> = arguments.collect(); // converts one collection to another, here Iterator to vector
    match command.as_str() {
        "help" => help(),
        "--version" | "-V" | "version" => version(),
        "init" => init::run(), // creates the metadata directories at root level
        "checkpoint" => checkpoint::run(&rest), // creates checkpoint metadata for the current git commit
        "protect" => protect::run(&rest),       // command line for the preserve --function call
        "target" => target::run(&rest),         // command line for the target --function call
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
        "agent" => agent::run(&rest), // agent adapters, hooks, and contract sessions
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
  protect --KIND TARGET [--policy NAME] [--checkpoint NAME] [scope SCOPE]
  target --KIND TARGET [--policy NAME] [--checkpoint NAME] [scope SCOPE] [change_type CHANGE_TYPE]
  parse FILE
  check [--json]
  check --agent
  agent init [--profile generic|claude|codex]
  agent verify [--profile generic|claude|codex]
  agent install --profile claude|codex
  agent hook --event EVENT [--profile generic|claude|codex] [--ttl SECONDS]
  agent session [list | show SESSION_ID]
  test-all
  context
  status

TARGET is a language-neutral qualified name, normally Type.member
(or just the name for a top-level item). The source language is inferred
from the file extension.
--KIND is --function, --data (a variable's stored value), --variable (its whole declaration),
--class, or --interface.
SCOPE is block (default), file, flow, folder, or all.
CHANGE_TYPE is logical_bn, logical_cn, logical_sn, or semantic (default: any change).
scope and change_type may also be written as --scope and --change-type.
EVENT is session-start, user-prompt-submit, pre-tool-use, post-tool-use, permission-request,
stop, or session-end; host spellings such as PreToolUse are accepted too. The hook payload is
read from stdin: Claude Code hook JSON for --profile claude, Codex hook JSON for --profile codex,
and Crane's neutral action JSON for --profile generic. --ttl only applies when the hook creates
the session.
agent install --profile codex adds Crane's hooks to .codex/hooks.json without touching other
hooks; running it again adds nothing. agent init --profile codex also initializes .crane,
installs those hooks, and verifies."#
    );
    Ok(())
}
