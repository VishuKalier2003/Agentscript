use std::env;
use std::path::Path;

mod agent;
mod check;
mod checkpoint;
mod context;
mod discover;
mod init;
mod parse;
mod policy;
mod protect;
mod status;
mod target;
mod task;
mod zones;

#[cfg(test)] // Compile the module only when running tests, not in production builds
mod tests;

pub(crate) use context::render as render_context;

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
        "discover" => discover::run(&rest), // advisory semantic inventory of the repository
        "policy" => policy::run(&rest), // reviewable policy proposals, activated only by approval
        "task" => task::run(&rest),     // task-to-contract planning, never activated here
        "zones" => zones::run(&rest),   // resolved repository zones, an input to authorization
        "agent" => agent::run(&rest),   // agent adapters, hooks, and contract sessions
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
  agent hook --event EVENT [--profile generic|claude|codex] [--ttl SECONDS] [--task ID]
             [--autonomy observe|assisted|delegated|autonomous] [--idle-timeout SECONDS]
             [--max-actions N] [--max-files N]
  agent session [list | show SESSION_ID]
  agent session start --profile PROFILE --session ID [--task ID] [--autonomy MODE] [--isolate] [...]
  agent session verify SESSION_ID [--level fast|tests|full]
  agent session cleanup SESSION_ID
  agent session resume|cancel|finalize|quarantine SESSION_ID [--reason TEXT]
  agent session extend SESSION_ID [--actions N] [--files N]
  agent session sweep
  test-all
  context
  status
  discover [--json] [--full] [--policies]
  policy propose [--name NAME] [--checkpoint NAME] [--min-confidence high|medium|low] [--json]
  policy proposals | show NAME [--json]
  policy edit NAME [--file PATH] [--by NAME]
  policy approve NAME --approver NAME --confirm DIGEST_PREFIX
  policy reject NAME --approver NAME [--reason TEXT]
  policy regenerate NAME [--by NAME]
  zones [ZONE_ID] [--json]
  task plan TASK_ID|TASK_FILE [--json] [--checkpoint NAME] [--propose]
  task ingest --source jira|asana [--delivery ID] [EVENT_FILE...] [--json]
  task sync [TASK_ID] [--json]
  task status [TASK_ID] [--json]
  task advance TASK_ID --to STATE [--reason TEXT]
  task serve [--addr 127.0.0.1:8787] [--once]

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
the session. --task (or CRANE_TASK_ID) binds a task id when the hook creates the session.
Sessions also bind an autonomy mode (default delegated), an autonomy budget, an idle timeout
(default 1800 s), and a lifetime (default 8 hours), plus the zones and the task scope; every
tool call is judged by the contract, then by zones, mode, scope, budget, and safety state.
After a tool runs, its actual effect (files and symbols changed, zones touched) is verified
incrementally; affected tests (.crane/testing.json) and full validation run at stop. --isolate
starts a session in its own Git worktree on branch crane/SESSION_ID.
discover inventories the repository (languages, services, modules, symbols, calls, tests,
owners, contracts, checkpoints) and suggests critical code to protect; it is advisory and
enforces nothing. --json prints the full inventory; --full ignores the incremental cache.
zones resolves the zones in .crane/zones (criticality, autonomy, safety state per semantic
selector) against the current inventory and shows resolved entities, unresolved selectors, and
conflicts. Zones only restrict; they never grant permissions or relax a contract.
discover --policies lists heuristic candidates for protection with reason, confidence, suggested
rule, and affected entities. policy propose writes them as candidate AgentScript under
.crane/proposals without activating it; only a human or trusted process can approve (which
writes .crane/policies/NAME.crane), reject, edit, or regenerate a proposal, never an agent.
task plan reads .crane/tasks/TASK_ID.json and derives MUST_CHANGE, MUST_NOT_CHANGE, MAY_CHANGE,
REQUIRES_APPROVAL, TASK_SCOPE, and EXPECTED_TESTS from code references (backticks, Type.method,
references), or reports task_needs_clarification with what is missing; --propose stores the
contract as a pending proposal. task ingest replays Jira or Asana webhook deliveries (or stdin)
through the lifecycle RECEIVED, ANALYZING, CONTRACT_PROPOSED, APPROVED, EXECUTING, VALIDATING,
PR_READY, REVIEW, MERGED, COMPLETED (or BLOCKED, FAILED, CANCELLED, DEGRADED); task serve accepts
the same deliveries over HTTP with CRANE_WEBHOOK_TOKEN. Repository mapping and agent assignees
live in .crane/sources/config.json; agents cannot ingest, sync, advance, or serve.
agent install --profile codex adds Crane's hooks to .codex/hooks.json without touching other
hooks; running it again adds nothing. agent init --profile codex also initializes .crane,
installs those hooks, and verifies."#
    );
    Ok(())
}
