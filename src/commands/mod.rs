use std::env;
use std::path::Path;

mod agent;
mod autonomy;
mod check;
pub(crate) mod checkpoint;
mod context;
mod dashboard;
mod deliver;
mod discover;
mod flow;
pub(crate) mod init;
pub(crate) mod observe;
mod parse;
mod policy;
mod protect;
mod repo;
mod session;
mod status;
mod target;
mod task;
mod test_contract;
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
        "test-contract" => test_contract::run(&rest), // contract compliance, apart from ordinary tests
        "zones" => zones::run(&rest), // resolved repository zones, an input to authorization
        "autonomy" => autonomy::run(&rest), // autonomy and safety state machine of sessions
        "session" => session::run(&rest), // evidence: inspect and export a session
        "deliver" => deliver::run(&rest), // branch, checks, pull request, approvals, merge
        "connect" => dashboard::connect_command(&rest), // crane repo connect
        "repo" => repo::run(&rest), // the repository connection: connect, status, inspect, disconnect
        "flow" => flow::run(&rest), // the golden path: one lifecycle from connection to completion
        "dashboard" => dashboard::run(&rest), // the semantic control plane and its API
        "observe" => observe::run(&rest), // the read-only observability API
        "packs" => dashboard::packs_command(&rest), // Payments and Testing policy packs
        "agent" => agent::run(&rest), // agent adapters, hooks, and contract sessions
        _ => Err(format!("unknown command '{command}'. Run 'crane help'.")),
    }
}

/** Validate the installed hooks of an agent provider (used by the golden path)
 * Input
    - kind: crate::adapter::AgentKind - claude or codex
 * Output
    - Result<serde_json::Value, String> the validation report
*/
pub(crate) fn validate_hooks(kind: crate::adapter::AgentKind) -> Result<serde_json::Value, String> {
    agent::validate(crate::adapter::adapter(kind).as_ref())
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
  agent uninstall --profile claude|codex
  agent hooks --profile claude|codex [--json]
  agent status [--profile claude|codex] [--session ID | --task ID] [--json]
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
  policy status [--json]
  policy propose [--name NAME] [--checkpoint NAME] [--min-confidence high|medium|low] [--json]
  policy proposals | show NAME [--json]
  policy edit NAME [--file PATH] [--by NAME]
  policy approve NAME --approver NAME --confirm DIGEST_PREFIX
  policy reject NAME --approver NAME [--reason TEXT]
  policy regenerate NAME [--by NAME]
  test-contract [--json] [--plan] [--session ID] [--no-ordinary]
  autonomy status [SESSION_ID] [--json]
  autonomy history SESSION_ID [--json]
  autonomy promote|demote SESSION_ID --to MODE [--reason TEXT]
  autonomy approve SESSION_ID [--reason TEXT]
  autonomy budget SESSION_ID [--json]
  autonomy refill SESSION_ID --amount N --reason TEXT --approver NAME --expires DURATION
  autonomy credit SESSION_ID --event task_milestone|human_review|merge --reference REF --approver NAME
  session inspect SESSION_ID [--json] | session inspect --export FILE
  session export SESSION_ID --json [--otlp]
  deliver merged SESSION_ID --sha MERGE_SHA [--by NAME]
  deliver run|status SESSION_ID [--json]
  deliver approve|reject|request-changes SESSION_ID --approver NAME [--reason TEXT]
  deliver exception SESSION_ID --check NAME --approver NAME --reason TEXT [--expires DURATION]
  deliver merge SESSION_ID [--by NAME]
  deliver slack-action --body FILE --timestamp T --signature S
  repo connect [--provider github|git|local] [--checkpoint NAME] [--default-branch NAME] [--refresh] [--json]
  repo status|inspect [--json]
  repo disconnect [--reason TEXT] [--forget]
  connect [--refresh] [--json]   (same as repo connect)
  dashboard [serve] [--addr HOST:PORT] [--once]
  dashboard api GET|POST PATH [--body JSON]
  observe [PATH] | observe serve [--addr HOST:PORT] [--once]   (read-only observability API)
  packs [list] | packs show payments|testing [--json] | packs propose payments [--name NAME]
  zones [ZONE_ID] [--json]
  zones recommend [--by NAME] [--json] | zones recommendations [--json]
  zones review ID|map [--claim] [--by NAME] [--json]
  zones approve ID|map --approver NAME --confirm DIGEST_PREFIX | zones reject ID|map --approver NAME [--reason TEXT]
  zones map [--file PATH] [--session ID] [--json]
  zones audit [--json]
  session run TASK_ID --agent claude|codex|generic [--actions FILE] [--approve] [--autonomy MODE] [--detach] [--json] [-- AGENT_COMMAND...]
  session finish SESSION_ID [--json] | session lifecycle SESSION_ID [--json]
  flow [status] [TASK_ID] [--json] | flow advance TASK_ID [--now] [--by NAME] [--json] | flow audit [TASK_ID|repository] [--json]
  task completions [list | show EVENT_ID | send [--now] [--task ID] | reconcile [--send]] [--json]
  task list [--json] | task show TASK_ID [--json]
  task prepare TASK_ID [--checkpoint NAME] [--by NAME] [--json]
  task approve TASK_ID --approver NAME --confirm DIGEST_PREFIX [--json]
  task launch TASK_ID --agent claude|codex|generic [--autonomy MODE] [--isolate] [--by NAME] [--json]
  task plan TASK_ID|TASK_FILE [--json] [--checkpoint NAME] [--propose]
  task contract compile TASK_ID|TASK_FILE [--checkpoint NAME] [--by NAME] [--json]
  task contract show TASK_ID [--version N] [--json] | task contract list [--json]
  task contract approve TASK_ID --approver NAME --confirm DIGEST_PREFIX
  task contract reject TASK_ID --approver NAME [--reason TEXT] | task contract history TASK_ID [--json]
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
A hook binds its provider event deterministically: to the Society session named by CRANE_SESSION
(or --crane-session), else to the agent's launched session of the task's current approved contract,
else to the session keyed by the provider's own session id, else to a transient one; the provider's
session is recorded as attached, a payload for another event or an agent working outside the
session's repository is refused, and whenever no Society decision can be obtained the provider gets
its blocking or deny answer, never an allow. agent hooks validates the installed hooks, agent
uninstall removes only Crane's, and agent status reports hooks and sessions.
Sessions also bind an autonomy mode (default delegated), an autonomy budget, an idle timeout
(default 1800 s), and a lifetime (default 8 hours), plus the zones and the task scope; every
tool call is judged by the contract, then by zones, mode, scope, budget, and safety state.
After a tool runs, its actual effect (files and symbols changed, zones touched) is verified
incrementally; affected tests (.crane/testing.json) and full validation run at stop. --isolate
starts a session in its own Git worktree on branch crane/SESSION_ID.
test-contract turns every active contract clause into contract tests (preserve: checkpoint,
identity, scope; target: changed, change type, scope), decided by Crane from the code, never by
test files, then runs the ordinary tests split into organizational and agent-authored ones.
--plan lists the tests without running them; --session ID tests that session's bound contract in
its worktree. Contract tests are mandatory when a session is finalized.
autonomy runs each session's state machine with two separate dimensions: autonomy (observe,
assisted, delegated, autonomous; changed only by a human promotion, one step at a time, or
demotion) and safety (active, degraded, quarantined; violations degrade, critical violations
quarantine, and recovery needs the evidence sets in .crane/autonomy.json: verified_repair,
human_approval, new_session, new_risk_budget). An agent can never change either; trying
quarantines its session. status shows the policy and states; history shows every transition.
Each session also has a risk budget (not tokens, compute, or money): autonomous actions cost
points from the risk-cost model in .crane/budget.json (operation, criticality, environment, scope,
reversibility, policy sensitivity, privilege escalation); violations cost points and critical ones
zero it; controlled events regenerate it up to its maximum. When it runs out, actions need human
approval and a refill is requested; refill adds temporary points with an approver and an expiry.
session inspect shows a session's evidence: the hash-chained journal projected into one record
per meaningful event (organization, team, agent, task, contract, checkpoint, autonomy, safety,
budget before and after, tool, operation, resources, decision, violations, repair, tests, human
intervention; never raw tool arguments) and the deterministic attestation derived from it.
session export --json prints all of it; --otlp prints it as OpenTelemetry OTLP/JSON spans;
inspect --export FILE re-derives everything from an export alone and checks it.
deliver run takes a reconciled session (finalizing it if needed) to a pull request: it commits
the changes on a branch, runs the final contract tests, the repository tests, and the checks in
.crane/delivery.json there, opens or updates the pull request with the contract and attestation,
and notifies Slack. Merging follows the merge policy (approvals per session autonomy and zone
criticality; autonomous routine changes may merge automatically); approvals come from the CLI or
signed Slack actions and never change a policy; exceptions are scoped to one check, commit, and
session, and expire. After a verified merge, Crane records the merge commit as a trusted
checkpoint, completes the task, queues the Jira or Asana completion, and re-finalizes the
attestation.
zones recommend runs discovery and turns its signals (payment and security vocabulary, persistence,
entry points, integrations, shared components, protected regions, the policy packs, and session
history) into zone recommendations: proposals in .crane/zone-proposals that govern nothing until a
human approves one (zones approve, quoting its digest), which writes .crane/zones/ZONE.zone;
zones reject declines it; every decision is in the zone audit log; agents cannot decide either.
repo connect connects the repository once: it initializes .crane, makes sure the trusted
checkpoint exists (never moving it), runs discovery, and records identity, provider (github, git,
or local, read from the origin remote without any network call), owner, name, default branch, and
the policy and zone versions; connecting again changes nothing (--refresh updates it). repo status
shows the connection, checkpoint staleness, and discovery freshness; repo inspect adds branches,
worktrees, capabilities, and history; repo disconnect keeps everything for a later reconnect.
connect records the repository once (identity, remote, default branch, languages); dashboard
serves the semantic control plane (Repository, Zones, Contracts, Agent Sessions, Policy
Simulator, Attestations) on a local address, and dashboard api answers the same API from the
command line. Policies made in the dashboard's visual or Advanced (AgentScript) editor become
pending proposals; the simulator replays session history in shadow mode. packs show runs the
Payments or Testing pack, which only recommends; packs propose payments makes a pending proposal.
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
flow is the golden path: one stage for the whole lifecycle (CONNECT_REPOSITORY, DISCOVERING,
REVIEW_REQUIRED, POLICY_APPROVAL, AGENT_READY, TASK_READY, RUNNING, VERIFYING, DELIVERY_READY, REVIEW,
MERGING, COMPLETING_TASK, COMPLETED, or a failure branch) derived from the subsystems, every layer from
the repository to the task completion with the identifiers that link them, audited transitions, and
flow advance for the steps that need no human decision (deliver, merge once approved, complete Jira).
session run launches an approved task and drives it through the governed lifecycle (TASK_READY,
SESSION_CREATED, AGENT_CONNECTED, RUNNING, DEGRADED or QUARANTINED, STOPPING, RECONCILING, VERIFIED,
DELIVERY_READY, or FAILED): actions come from an action script (executed by Crane where permitted),
an agent command whose hooks attach with CRANE_SESSION, or a detached agent finished later with
session finish; termination freezes execution, reconciles the repository, runs the contract tests
and the repository tests of the testing policy, attests, and only a DELIVERY_READY session can be
delivered.
task list shows the tracker tasks (Jira, Asana snapshots in .crane/sources) mapped to the connected
repository with their state: AVAILABLE, PLANNING, NEEDS_CLARIFICATION, CONTRACT_PENDING_APPROVAL,
READY, RUNNING, COMPLETED, BLOCKED, or FAILED; task prepare fetches, validates, and compiles the
task's contract; task approve approves it; task launch starts the session with the chosen agent,
bound to the task, contract digest, repository, checkpoint, agent, autonomy, budget, zones, and
policy version (READY TO RUN); launching again reuses it, and an obsolete contract never launches.
task plan reads .crane/tasks/TASK_ID.json and derives MUST_CHANGE, MUST_NOT_CHANGE, MAY_CHANGE,
REQUIRES_APPROVAL, TASK_SCOPE, and EXPECTED_TESTS from code references (backticks, Type.method,
references), or reports task_needs_clarification with what is missing; --propose stores the
contract as a pending proposal. task contract compile turns a task into a versioned contract bound
to the repository, task, checkpoint SHA, persistent policy version, zone set, autonomy policy,
budget model, and organization (.crane/task-contracts/TASK/vN.json); a vague task compiles to
clarification_required and grants nothing; only a human approves it (quoting its digest), and a
contract whose bindings changed is invalidated. policy status shows which policies are active in
the organization, repository, and task layers. task ingest replays Jira or Asana webhook deliveries (or stdin)
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
