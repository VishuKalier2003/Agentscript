# Crane MVP

Crane is a deliberately small, modular contract language for protecting trusted code from unintended AI-agent changes.

This MVP implements two policy primitives:

- `preserve` (code must stay the same)
- `target` (code must be changed, optionally in a given way)

Each rule points at a `--function`, `--data` (the value stored in a
variable), `--variable` (its whole declaration), `--class`, or `--interface`.

It also implements the Git-backed workflow discussed during design:

- `crane init`
- `crane checkpoint`
- `crane protect --KIND ... [scope SCOPE]`
- `crane target --KIND ... [scope SCOPE] [change_type CHANGE_TYPE]`
- `crane parse`
- `crane check`
- `crane test-all`
- `crane context`
- `crane check --agent`
- `crane agent init`
- `crane agent verify`
- `crane agent install --profile claude`
- `crane status`

## Core idea

A checkpoint is an explicit trusted Git commit.

A policy is an independent, non-recursive module:

```crane
policy payment_gateway {
    checkpoint baseline;
    preserve --function GatewayService.call scope flow;
    target --function GatewayService.retry change_type logical_bn;
    preserve --class GatewayConfig;
}
```

`preserve` requires code to stay the same; `target` requires the agent to
change it, optionally with a required kind of change: `logical_bn`
(business logic), `logical_cn` (complexity), `logical_sn` (structure), or
`semantic` (wording and alignment). See [LANGUAGE.md](LANGUAGE.md#targets).

Every statement ends with `;`. An optional `scope` at the end of a preserve
statement widens what is protected around the function: `block` (the
default, just the function), `file`, `flow` (every function it calls and every
function that calls it), `folder`, or `all`. See [LANGUAGE.md](LANGUAGE.md#scopes).
`crane protect` accepts the same suffix, and `crane target` accepts both
`scope` and `change_type`, for example:

```bash
crane protect --function GatewayService.call scope flow
crane target --function GatewayService.retry scope file change_type logical_bn
```

Both options may also be written as flags (`--scope`, `--change-type`).

The verifier compares the protected code in the checkpoint commit with the current working tree. The model is never the authority that decides whether the contract passed.

## Trust model

The developer creates a reviewed Git commit and explicitly records it as a
checkpoint. Crane then independently compares protected source nodes in that
commit and the current worktree. An agent may edit files, but it cannot create
or move a checkpoint through `crane check`, and it cannot turn an uncertain
resolution into a pass.

`preserve` guarantees that the protected function's source text is unchanged
apart from comments, blank lines, and trailing whitespace. Comments may change;
code, indentation, and line layout remain protected. It does not guarantee semantic equivalence, runtime behavior,
security, protection of unrelated code, or protection against changes outside
the resolved function.

## Quick start

Install a release binary as described below, then run Crane in an existing Git
repository. This complete example takes less than five minutes:

```bash
crane init
git add .
git commit -m "trusted baseline"
crane checkpoint --name baseline
crane protect --function PaymentService.charge --policy payment_service
crane check
```

`crane protect` creates the preserve policy under `.crane/policies/`; commit
`.crane/config.toml` and `.crane/policies/` so repository policy is reviewable
and reproducible. Checkpoint metadata under `.crane/checkpoints/` is local
metadata and is ignored by default; create it only from a trusted Git commit
and never accept an agent-generated checkpoint without review.
The v0.1 configuration file is reserved for future use and accepts only blank
lines and comments; other content is rejected as invalid configuration.

After changing the protected function, run:

```bash
crane check
```

Crane reports a failure. Restore the function to its checkpoint version and
run `crane check` again to see `Crane check: PASS`. Checkpoints are metadata
that point to an existing Git commit; Crane never trusts an agent-generated
checkpoint automatically.

For a fresh local demo, create a Git repository containing a source function
such as `PaymentService.charge` before running the commands above.

For machine consumption:

```bash
crane check --json
```

For agent verification, use the same deterministic JSON contract with a
non-zero exit status on violations:

```bash
crane check --agent
```

`crane check --json` and `crane check --agent` return `status` plus a
`violations` array. Each violation has `policy_id`, `rule`, `target`,
`checkpoint`, `violation_type`, and `message`. Ambiguous targets, parser
failures, unsupported languages, missing checkpoints, and malformed policies
are failures, never passes.

For a compact LLM-facing context:

```bash
crane context
```

## Agent integration contract

Crane remains an external verifier; the coding agent is an untrusted actor and
must not decide whether a protected change is acceptable. An integration may
invoke Crane around any agent edit session:

```text
session start
  -> crane context
  -> agent edits repository
  -> crane check --agent
  -> structured violation returned to agent
  -> agent repairs repository
  -> crane check --agent
  -> success
```

`crane context` emits deterministic active policy records. `crane check
--agent` emits JSON on stdout, never changes source or checkpoints, exits zero
when every policy passes, and exits non-zero when a policy fails or cannot be
verified. Claude Code, Codex, GitHub Copilot, or another external agent can
run these commands through its existing terminal/tool integration; no custom
agent runtime is required.

### Agent adapter flow

Crane exposes a small adapter boundary with a universal profile and named
profiles for `generic`, `claude`, and `codex`:

```text
agent receives user request
  -> adapter: crane agent init --profile claude
  -> Crane initializes .crane and immediately verifies
  -> agent edits repository
  -> adapter: crane agent verify --profile claude
  -> JSON violation text is returned to the agent
  -> agent repairs the repository
  -> adapter: crane agent verify --profile claude
  -> exit 0 means the workflow is complete
```

The adapter is deliberately not an agent runtime: it does not invent edits or
silently mutate source. Claude Code, Codex, Copilot, or another host invokes
the same adapter commands through its normal tool interface. Profile names
provide a stable integration contract today and allow host-specific launchers
to specialize later without duplicating Crane policy semantics.

### Automatic Claude Code hooks

Run this once from an application repository:

```text
crane init
crane agent install --profile claude
```

This creates `.claude/settings.local.json` with project-local hooks and makes
Crane run automatically:

- `SessionStart` binds the Claude session to a contract session, which fixes
  the policy versions and checkpoint commits for the whole session, and prints
  the policy context to Claude.
- `UserPromptSubmit` verifies the repository whenever the user submits a
  prompt, before Claude makes new edits.
- `PreToolUse` authorizes every tool call against the bound contract before it
  runs, and denies writes that would change preserved code (see below).
- `PostToolUse` journals every tool call and verifies the clauses the call
  could have affected, which also catches shell commands that changed
  protected code.
- `Stop` reconciles the session completely, writes an attestation, and reports
  any final failure.
- `SessionEnd` reconciles and closes the contract session.

The model reads the policy context, but it is never the authority. Crane
enforces the contract on its own: `PreToolUse` prevents, `PostToolUse`
observes, and `Stop` verifies. `crane agent session list` and
`crane agent session show ID` print each session's binding, its authorized and
denied actions, and its latest attestation (see LANGUAGE.md, "Runtime authority
and contract sessions").

The hooks invoke `crane` from `PATH`, so install a pinned Crane release before
starting Claude Code. On Windows, restart Claude Code after adding the Crane
directory to `PATH`. Claude Code provides the host lifecycle; Crane remains
the independent verifier. Hook output and its non-zero exit status are passed
back to Claude Code, which can repair the reported violation and retry without
the user repeating a Crane instruction.

A blocked prompt/edit/stop hook exits with status `2`, the Claude Code hook
convention for feedback that must be shown to the agent; a passing hook exits
`0`.

Each violation carries a `repair_owner`. Only worktree source problems and
unmet targets are `agent`-repairable; malformed policies, checkpoint errors, and setup errors are
`human`, because fixing them requires editing `.crane`. Hooks block only on
`agent` violations. When every remaining violation is `human`, the hooks exit
`0` with a `systemMessage` warning the user and context telling Claude not to
retry, so a broken policy cannot trap the agent in an endless repair loop.
`crane check` and `crane agent verify` still fail, so CI stays strict.

`PreToolUse` protects the verifier itself. It blocks any non-read-only tool
call that targets `.crane`, `.claude/settings.json`, or
`.claude/settings.local.json`, and any shell command that runs
`crane checkpoint`, `crane protect`, `crane target`, `crane init`, or
`crane agent init|install|hook`, because re-baselining or forging hook events
would hide a violation. It then applies the contract:

- A write whose result would change code a `preserve` rule covers is denied.
  Restoring the checkpoint version is allowed.
- `target` rules authorize changes to the code they cover.
- While a policy is malformed, or a checkpoint or item cannot be resolved,
  every mutating tool is denied. Read-only tools stay available. The
installed settings also add matching `permissions.deny` `Edit` rules. Shell
inspection is best-effort pattern matching; treat it as a guard rail and keep
`.crane` changes under human code review.

Crane does not overwrite an existing `.claude/settings.local.json`; merge the
generated hook entries deliberately if that file already exists. The hooks
never create or move checkpoints, modify policies, or edit source code, and
they work for every language supported by the verifier.

### Automatic Codex CLI hooks

Run this once from an application repository:

```text
crane agent init --profile codex
```

The command initializes `.crane` if needed, installs the Codex hooks, and
verifies the repository. To install or repair only the hooks, run
`crane agent install --profile codex`. To verify on demand, run
`crane agent verify --profile codex` (or `crane agent check --profile codex`).

The installer adds one Crane-owned hook group per event to
`<repo>/.codex/hooks.json`, one of the locations where Codex loads project
hooks. Each group is identified by its command,
`crane agent hook --event <Event> --profile codex`. The installer is additive:
- it keeps every other key and hook in the file;
- it never writes `.codex/config.toml`;
- it skips any event whose Crane command is already registered, either in
  `hooks.json` or inline in `config.toml`;
- running it again changes nothing;
- it refuses to touch a `hooks.json` that is not a JSON object.

No uninstall command exists yet. To remove the hooks, delete the Crane-owned
groups by hand. Codex runs project hooks only after they are trusted, so open
Codex in the repository and review them with `/hooks`.

Codex sends its hook JSON on stdin, and Crane answers in Codex's format:

| Codex event | Crane behavior | Output |
|---|---|---|
| `SessionStart` | Binds or loads the contract session (`codex-<session_id>`) | Plain-text policy context |
| `UserPromptSubmit` | Verifies every clause | `systemMessage` when something fails; never blocks the prompt |
| `PreToolUse` | Authorizes the call with the shared decision engine; `apply_patch` patches are parsed into the files and edits they propose | Silent allow; deny with exit code `2` and the reason on stderr |
| `PermissionRequest` | Same decision | `decision.behavior: "deny"` when forbidden; otherwise nothing, so the user's own approval prompt decides |
| `PostToolUse` | Journals the call and verifies the clauses it could affect | `decision: "block"` with the report as `additionalContext` when the agent broke the contract |
| `Stop` | Reconciles completely and writes the attestation | `decision: "block"` continues the turn while a violation or unmet target remains; a retry with `stop_hook_active` gets a `systemMessage` instead, so it cannot loop |
| `SessionEnd` | Reconciles and closes the session | Nothing |

A single hook can be run by hand for debugging:

```text
crane agent hook --event PreToolUse --profile codex < payload.json
```

`--event` accepts both Codex's names (`PreToolUse`) and Crane's
(`pre-tool-use`).

Protection for Codex covers:
- `.crane`;
- `.codex/hooks.json` and `.codex/config.toml`;
- the mutating `crane` commands (`checkpoint`, `protect`, `target`, `init`,
  `agent init|install|hook`), whether reached through `apply_patch`, `Bash`,
  or an MCP tool.

A patch Crane cannot parse is denied, because a write whose files are unknown
cannot be authorized.

Tested against the hook schema in the current Codex documentation, not a
particular Codex binary. See the limitations below.

The agent-check JSON contract is:

```json
{
  "status": "passed",
  "violations": [
    {
      "policy_id": "payment_service",
      "rule": "preserve",
      "target": "PaymentService.charge",
      "checkpoint": "baseline",
      "violation_type": "source_changed",
      "repair_owner": "agent",
      "message": "Protected function was modified."
    }
  ]
}
```

`status` is `passed` or `failed`; `violations` is always present and is
deterministically ordered by policy and rule. A failed resolution, missing
checkpoint, missing commit, or missing protected function is also reported as
a violation, so verification fails closed.

## Repository discovery

`crane discover` scans the repository and builds a semantic inventory, so you
can see what deserves a contract without writing policies file by file:

```bash
crane discover          # human summary
crane discover --json   # full machine-readable inventory
crane discover --full   # ignore the incremental cache
```

The summary covers repository size, languages, services and modules,
critical candidates (with a suggested `preserve` rule where one would
resolve), existing contract coverage, and checkpoints. The JSON form also
includes:

- every file, module, folder, and symbol;
- call relationships (callers and callees);
- test relationships;
- CODEOWNERS owners;
- risk signals for each symbol.

Each symbol has a stable semantic id, such as
`symbol:java:com.acme.payments.PaymentService.charge`.

Discovery is advisory. It never creates, changes, or enforces a policy, and it
works before `crane init`. Once Crane is initialized, discovery keeps a cache
in `.crane/runtime/inventory/`. The cache is keyed by Git content hashes, so
later runs parse only the files whose content changed. They also re-resolve
calls only for code in changed files and for callers of functions defined
there.

Discovery parses Java, JavaScript, TypeScript/TSX, Python, Rust, Go, C/C++,
and Kotlin, and counts common other file types. Policies can still target only
Java, JavaScript, Python, and Rust, so discovery marks the other languages as
"discovery only".

## Repository zones

Zones in `.crane/zones/*.zone` classify the repository by meaning, so that an
organization can say "Payments is Critical", "Authentication is Restricted",
or "Tests are Routine":

```
zone payments {
    criticality critical;
    autonomy assisted;
    select subsystem payments;
}
```

Each zone has three separate settings:

- a criticality: routine, sensitive, critical, or restricted;
- a default autonomy: observe, assisted, delegated, or autonomous;
- a safety state: active, degraded, or quarantined.

Selectors name symbols, modules, services, subsystems, policies, or tests, so
zones survive moves and are resolved again after every repository change.
Zones are an input to authorization and can only restrict: criticality and
state cap autonomy, and overlapping zones take the most restrictive values.
Zones never grant permissions or relax contracts.

```bash
crane zones               # zones, selector resolution, unresolved selectors, conflicts
crane zones payments      # one zone with every resolved symbol and file
crane zones --json
```

See LANGUAGE.md, "Zones", for selectors, statuses, and conflicts.

## Policy proposals

`crane discover --policies` lists what the organization should probably
protect. Each candidate comes with a reason, a confidence, a suggested rule,
and the entities it affects. For example:

```
PaymentService.charge
  reason: criticality=Critical (zone payments) + payment name 'charge' + payment subsystem (...)
  suggested rule: preserve --function PaymentService.charge;
```

The candidates come from fixed heuristics (zones, payment, auth, and secret
vocabularies, centrality, tests, CODEOWNERS, and migration, infrastructure,
and production-configuration paths), not from semantic understanding.

`crane policy propose` writes them as candidate AgentScript under
`.crane/proposals/`, together with a JSON record a dashboard can review.
Nothing is active until a human or trusted process approves it:

```bash
crane policy propose --name payments_guard
crane policy show payments_guard
crane policy edit payments_guard          # after editing .crane/proposals/payments_guard.crane
crane policy approve payments_guard --approver alice --confirm <first 12 digest characters>
crane policy reject payments_guard --approver alice --reason "too broad"
crane policy regenerate payments_guard
```

Agents cannot approve, reject, edit, or regenerate proposals (see
LANGUAGE.md, "Policy discovery and proposals").

## Task contracts

`crane task plan PAY-1821` reads a tracker-independent task file,
`.crane/tasks/PAY-1821.json` (see `examples/tasks/`). It derives a proposed
task contract with six sections:

- MUST_CHANGE (targets);
- MUST_NOT_CHANGE (preserves);
- MAY_CHANGE;
- REQUIRES_APPROVAL (Critical zones, cross-service work);
- TASK_SCOPE (services and modules);
- EXPECTED_TESTS.

The contract comes with candidate AgentScript. Only code references in the
task (backticked names, `Type.method`, or explicit references) become
authority. A vague or ambiguous task returns `task_needs_clarification` with
exactly what is missing. A task that must change permanently protected code
returns `task_conflicts_with_policy`. `--json` prints the plan, and
`--propose` stores the contract as a pending proposal that only a human can
approve.

## Jira and Asana

Jira and Asana tickets assigned to the society run through one lifecycle:

```
RECEIVED -> ANALYZING -> CONTRACT_PROPOSED -> APPROVED -> EXECUTING -> VALIDATING -> PR_READY -> REVIEW -> MERGED -> COMPLETED
```

The lifecycle also has BLOCKED, FAILED, CANCELLED, and DEGRADED states. Each
ticket becomes a versioned task contract that a human approves. Its agent
session starts exactly once per contract version. Cancelling the ticket stops
the session and retires the contract.

```bash
crane task ingest --source jira tests/fixtures/orchestration/jira/01-created-PAY-1821.json
crane policy approve task_pay_1821_v1 --approver alice --confirm <digest>
crane task sync PAY-1821              # APPROVED -> EXECUTING (session task-PAY-1821-v1)
crane task status
CRANE_WEBHOOK_TOKEN=... crane task serve --addr 127.0.0.1:8787
```

Repository mapping lives in `.crane/sources/config.json` (see the fixture in
`tests/fixtures/orchestration/config.json`). Replayable Jira and Asana
deliveries are in `tests/fixtures/orchestration/`. See LANGUAGE.md, "Task
orchestration".

## Agent sessions

Claude Code and Codex stay your agents: Crane wraps each of their sessions in a
contract session through their hooks. The session binds:

- the task, contract, and checkpoint;
- the zones;
- an autonomy mode (observe, assisted, delegated, or autonomous) and budget;
- timeouts.

Every tool call is decided by the same engine. The contract denies first.
Then zones, the autonomy mode, the task scope, the budget, and the safety
state decide: allow, ask for approval, or deny.

```bash
crane agent hook --event pre-tool-use --profile claude --task PAY-1821 --autonomy delegated
crane agent session list
crane agent session show claude-SESSION
crane agent session resume|cancel|finalize|quarantine claude-SESSION
crane agent session extend claude-SESSION --actions 50
crane agent session sweep          # closes expired and idle sessions
```

Interrupted agents do not keep authority. Sessions expire after 8 hours,
authority lapses after 30 idle minutes until the user returns or a human
resumes, and cancelled or finalized sessions never act again. Recorded Claude
and Codex sessions in `tests/fixtures/sessions/` replay end to end. See
LANGUAGE.md, "Agent sessions".

## Isolated execution and effect verification

Agent sessions can run in their own Git worktree, with
`crane agent session start --isolate`. After every tool call, Crane checks
what actually changed, not what the tool claimed. It records the files and
symbols changed, the zones touched, and the targets satisfied, and it
re-verifies only the contract clauses those changes can affect. A shell script
that rewrites `PaymentService.charge` is caught right after it runs, even
though the command was allowed. Affected tests (configured in
`.crane/testing.json`) and full validation run when the agent stops.

```bash
crane agent session start --profile claude --session s1 --isolate --task PAY-1821
crane agent session verify claude-s1 --level fast|tests|full
crane agent session finalize claude-s1 && crane agent session cleanup claude-s1
```

## Contract tests

`crane test-contract` turns the active contracts into executable verification
requirements and reports them apart from the repository's ordinary tests, so a
customer can tell contract compliance from code correctness:

```text
CONTRACT TESTS (3 passed, 0 failed, 1 not applicable; decided by Crane, never by test files)
✓ PaymentService.charge preserved [payments_core]
✓ PaymentService.charge keeps its semantic identity [payments_core]
- PaymentService.charge block scope preserved [payments_core]
✓ PaymentService.retry changed [task_pay_7]
✓ PaymentService.retry logical_bn satisfied [task_pay_7]
✓ PaymentService.retry found within its folder scope [task_pay_7]

ORDINARY TESTS
  ✓ organizational python: 87 test files passed
  ✓ agent-authored python: 1 test files passed
```

Contract tests are decided by Crane's own comparison of the code with the
checkpoint. A passing test file never satisfies one, and changes to tests the
agent wrote alone never satisfy a target. Organizational tests always run;
tests the agent created are run and marked `agent-authored`. Contract tests are
mandatory when a session is finalized.

```bash
crane test-contract [--json] [--plan] [--session SESSION_ID] [--no-ordinary]
```

## Autonomy and safety

Every session has two separate states. **Autonomy** (`observe`, `assisted`,
`delegated`, `autonomous`) is how much the agent may do on its own. **Safety**
(`active`, `degraded`, `quarantined`) is whether it is behaving. Each has its
own legal transitions:

- Autonomy rises only when a human promotes it, one step at a time
  (`observe → assisted → delegated → autonomous`), while safety is active.
  A human can demote it at any time.
- A violation moves safety from `active` to `degraded`. A critical violation
  (an unauthorized effect, an exhausted budget, or an attempt at
  self-escalation) moves any state to `quarantined`.
- Recovery needs explicit evidence: a verified repair, a human approval, a
  new session, or a new risk budget, in the combinations
  `.crane/autonomy.json` configures. A quarantine never lifts without a
  human.

An agent can never change either state. Trying to, from its shell or through
the CLI, quarantines its session. A quarantine also carries over to the same
agent's next session on the task. The policy hierarchy outranks autonomy: a
Restricted zone stays closed even to an autonomous agent unless
`.crane/autonomy.json` grants that authority, and the contract still applies
inside a grant.

```bash
crane autonomy status [SESSION_ID] [--json]
crane autonomy history SESSION_ID [--json]
crane autonomy promote|demote SESSION_ID --to MODE   # humans only
crane autonomy approve SESSION_ID                     # human approval as recovery evidence
```

## Autonomy budget

Each session also has a **risk budget**: the autonomous authority it may
spend without a human. It is not tokens, compute, or money.

- **Cost.** An action the agent takes on its own costs points from the
  configurable risk-cost model in `.crane/budget.json`. The price depends on
  the operation, resource criticality, environment (isolated, workspace, or
  production), scope (task, repository, or shared library), reversibility,
  contract sensitivity, and privilege escalation. Under the defaults, a
  routine source write costs less than a shared-library write, which costs
  less than a production write, which costs less than privilege escalation.
  Actions a human approves cost nothing.
- **Penalties.** Violations cost points, and critical ones set the budget to
  zero.
- **Regeneration.** The budget regenerates only through controlled events, and
  never above `budget_max`:
  - a completed contract;
  - passing contract tests;
  - task milestones, reviews, and merges;
  - sustained compliant behaviour.
- **Exhaustion.** When the budget runs out:
  - actions need human approval;
  - the session is degraded;
  - promotion is blocked;
  - a refill is requested.
- **Refills.** A refill is temporary and never changes a policy:

```bash
crane autonomy budget SESSION_ID [--json]
crane autonomy refill SESSION_ID --amount 20 --reason "reviewed plan" --approver lead --expires 4h
crane autonomy credit SESSION_ID --event human_review --reference PR-42 --approver reviewer
```

## Evidence and attestation

Every session keeps an append-only, hash-chained journal. Crane projects it
into one **evidence record** per meaningful event. Each record carries:

- the organization, team, agent, session, and task;
- the contract version and checkpoint;
- the autonomy mode and safety state;
- the budget before and after;
- the tool, operation, and resources;
- the policy decision;
- violations, repairs, test results, and human interventions.

Raw tool arguments are never stored. Actions are kept as a digest plus a
normalized summary, such as the programs a shell command runs and its argument
count.

When a session is finalized, Crane derives a **deterministic attestation**
from that evidence and writes it to `final_attestation.json`. It records:

- the task, contract, checkpoints, agent, and policy versions;
- an action summary;
- denied actions, approvals, violations, and repairs;
- human interventions;
- contract and ordinary tests;
- the final state and final decision;
- a digest over all of the above.

```bash
crane session inspect SESSION_ID            # timeline and attestation
crane session export SESSION_ID --json      # binding, journal, evidence, attestation
crane session export SESSION_ID --otlp      # OpenTelemetry OTLP/JSON spans
crane session inspect --export FILE         # re-derive and check an export on its own
```

The organization and team come from `.crane/organization.json`
(`{"organization": "acme", "team": "payments"}`). Without that file, the
organization is the owner of the `origin` remote.

## Delivery: pull requests, CI checks, Slack, and merge

`crane deliver run SESSION_ID` takes a reconciled session (finalizing it if
needed) through to a pull request:

1. It commits the session's changes on a delivery branch.
2. On that branch it runs the final contract tests, the repository tests, and
   the lint and security checks configured in `.crane/delivery.json`.
3. It opens or updates the pull request with the contract summary and the
   attestation.
4. It notifies the configured Slack channels and people.

**Merge policy.** Merging follows the policy for the session's final
autonomy and the zone criticality of the changed files:
- Under the defaults, an autonomous session that changed only routine code
  merges automatically once every check passes.
- Critical and restricted changes need two approvals.
- Everything else needs one approval.

**Slack.** The message's buttons are View PR, View contract, Approve, Reject,
and Request changes, plus Approve exception where the configuration allows
one. Clicks are verified with Slack's signing secret and count only for mapped
approvers and the current commit.

**Exceptions.** An exception covers one allowed failing check of one commit
and session, expires, and is recorded in both journals. Contract tests can
never be excepted. Approvals and exceptions never change a policy.

**After the merge.** Once the merge is verified, Crane:
- records the merge commit;
- makes it the trusted checkpoint `trusted_<session>`;
- completes the task;
- only then queues the Jira or Asana completion;
- re-finalizes the attestation.

```bash
crane deliver run SESSION_ID
crane deliver status SESSION_ID
crane deliver approve|reject|request-changes SESSION_ID --approver NAME [--reason TEXT]
crane deliver exception SESSION_ID --check lint --approver NAME --reason TEXT --expires 4h
crane deliver merge SESSION_ID
crane task serve   # also accepts Slack button clicks on POST /slack/actions
```

## Semantic control plane

Connect the repository once, then open the control plane:

```bash
crane connect          # records identity, remote, default branch, languages
crane dashboard        # prints http://127.0.0.1:8790/?token=...
```

The control plane is not a generic admin console; every screen comes from the
repository's own metadata and Crane's records:

| Screen | What it shows |
| --- | --- |
| **Repository** | semantic inventory, critical regions, policy coverage, unresolved policy targets |
| **Zones** | selectors, criticality, autonomy defaults, safety state, resolved resources |
| **Contracts** | a visual policy editor with a live AgentScript preview, an Advanced AgentScript editor, version and approval history, and the policy packs |
| **Agent Sessions** | agent behavior, autonomy, budget, actions, violations, tests, and final outcome |
| **Policy Simulator** | shadow-mode evaluation of a draft or proposal against recorded session history, with false-positive analysis |
| **Attestations** | each attestation with its full evidence trail |

**Contracts.** A policy made in the dashboard is only a pending proposal.
Activating it is the same human approval as `crane policy approve`.

**API.** Everything the page shows comes from one API, which the command line
can call directly: `crane dashboard api GET /api/zones`.

**Policy packs.** There are two packs, and they only recommend:
- **Payments** finds payment processing, refunds, transaction logic, and
  payment state transitions, and suggests preserve rules and critical zones.
  `crane packs propose payments` turns its suggestions into a pending
  proposal.
- **Testing** finds test directories, test frameworks, test ownership gaps,
  and the `.crane/testing.json` commands that contract tests need.

```bash
crane packs show payments
crane packs show testing
crane packs propose payments
```

## Installation

Prebuilt binaries for Linux, macOS (Intel and Apple Silicon), and Windows are
published on the [GitHub Releases page](https://github.com/VishuKalier2003/Agentscript/releases).
Download the archive for your platform, extract it, and put the binary in a
directory on your `PATH`.

### Windows

Download `crane-v0.1.2-x86_64-pc-windows-msvc.zip`, extract `crane.exe`, and
add its directory to the user `PATH` through **System Properties → Environment
Variables**. Open a new PowerShell window afterward.

### macOS

Download the `x86_64-apple-darwin` archive for Intel Macs or the
`aarch64-apple-darwin` archive for Apple Silicon Macs. Extract `crane`, move it
to `~/.local/bin` or `/usr/local/bin`, and ensure that directory is on `PATH`.

### Linux

Download `crane-v0.1.2-x86_64-unknown-linux-gnu.tar.gz`, extract `crane`, move
it to `~/.local/bin` or `/usr/local/bin`, and ensure that directory is on
`PATH`.

Verify the installation:

```bash
crane --version
```

### Build from source

Install the stable Rust toolchain, then run:

```bash
cargo install --git https://github.com/VishuKalier2003/Agentscript.git --bin crane
```

Alternatively, clone the repository and run `cargo build --release`; the
binary is written to `target/release/crane` (or `crane.exe` on Windows).

## Publishing a release

Create and push a semantic-version tag after merging the desired changes:

```bash
git tag v0.1.2
git push origin v0.1.2
```

The [release workflow](./.github/workflows/release.yml) builds archives for
Linux, macOS Intel, macOS Apple Silicon, and Windows, publishes SHA-256
checksums, and creates a GitHub Release with generated release notes. The
workflow can also be started manually from GitHub Actions by supplying an
existing tag.

## Consuming Crane from an application repository

An application repository should commit its reviewed `.crane/policies/` and
checkpoint metadata, then download a pinned Crane release in CI. It should not
compile Crane from `main` for every build. The application checkpoint identifies
the trusted application commit; the Crane release identifies the verifier
version.

The recommended CI sequence is:

```text
checkout application with full Git history
  -> download pinned Crane release
  -> verify the release checksum
  -> crane context
  -> crane check --json
```

For an interactive agent adapter, use:

```text
crane context
  -> agent edits application
  -> crane check --agent
  -> return JSON violations to the agent
  -> agent repairs
  -> crane check --agent
```

Crane provides the adapter-facing commands `crane agent init` and
`crane agent verify`; these coordinate initialization and verification only.
They do not implement an agent runtime, create checkpoints automatically, or
duplicate policy semantics.

The Claude adapter is activated by the project-local hook file, not by the
application CI YAML. Once `crane agent install --profile claude` has created
`.claude/settings.local.json` and Claude Code is restarted, pasting a prompt
automatically invokes the `UserPromptSubmit` hook. No Crane command needs to be
pasted into the Claude conversation. The YAML workflow is a separate CI gate
that runs on pushes and pull requests.

## GitHub Actions CI

Crane includes a GitHub Actions workflow at
[`.github/workflows/ci.yml`](./.github/workflows/ci.yml). It runs automatically
for every branch push and pull request, and can also be started manually from
the Actions tab.

The workflow installs the stable Rust toolchain and runs:

```text
cargo fmt --all -- --check
cargo check --locked
cargo test --locked
cargo build --release --locked
bash scripts/smoke_test.sh
```

The result appears in GitHub under the repository's **Actions** tab and on
pull requests under **Checks**. A failed command makes the workflow fail, so
the check can be required by branch protection rules in repository settings.

## Language design

Crane MVP is intentionally non-recursive and modular:

```text
policy
  └── statements
       └── preserve
```

Policies do not import, inherit, invoke, extend, or depend on other policies.

Supported source languages are Java, JavaScript/JSX, Python, and Rust. The
comparison model and all failure cases are specified in
[`LANGUAGE.md`](./LANGUAGE.md).

## Why Rust?

Rust is used for the core because Crane needs to become a portable, deterministic CLI that can run locally, inside CI, Git hooks, editors, and agent lifecycle hooks without requiring a language runtime. Rust also gives strong static typing and explicit ownership, which are useful as the parser, AST, IR, code model, and evaluator grow.

Java would be productive for a backend-heavy version, but it makes distribution heavier and ties the core to a JVM runtime. Python is excellent for experimentation, but a standalone verifier distributed into every repository is less frictionless. Go is the strongest alternative and would also work well; Rust was selected because Crane is fundamentally a language/compiler-style systems tool where strong types and explicit data structures are useful.

This is not primarily a performance decision. It is a portability, packaging, and systems-design decision.

## Language-independent function protection

`preserve --function TARGET` uses a language-neutral qualified name. `TARGET`
is normally `Type.method` (or a single top-level function name), rather than a
language-specific signature or source path. Crane detects the parser from the
file extension and compares the complete syntax node for the function with
only comments removed, using each language's own comment syntax (`#` for
Python; `//` and `/* */` for Java, JavaScript, and Rust).

The MVP includes tree-sitter grammars for Java (`.java`), JavaScript/JSX
(`.js`, `.jsx`, `.mjs`, `.cjs`), Python (`.py`), and Rust (`.rs`). Unsupported
extensions are ignored during source target resolution and produce an explicit
resolution error when they are the only match.

Rust targets may use Rust's qualified `Type::method` spelling. Rust source is
always parsed by the Rust tree-sitter grammar during resolution; only `.crane`
files are parsed as Crane policies.

Checkpoints are metadata-only JSON files containing a Git commit reference.
Crane reads baseline source with `git show` and current source from the
worktree; it never creates copied target snapshots. Missing commits produce an
error and are not fetched automatically.

## Current limitations

This MVP intentionally keeps the implementation narrow:

- Function resolution is source-based and conservative; overloaded functions
  are not disambiguated by parameters.
- The check compares the extracted tree-sitter function region with comments,
  blank lines, and trailing whitespace removed; indentation and layout
  changes fail, so running a code formatter over a protected function
  requires a new checkpoint.
- Duplicate target matches fail closed.
- Git commit SHA is the immutable baseline; branch is recorded only as metadata.
- Claude Code and Codex CLI project hooks are supported through
  `crane agent install --profile claude|codex`. Other agent hosts must call
  `crane agent hook --profile generic` with Crane's neutral action JSON from
  their own hook, MCP, or wrapper integration.
- Codex support follows the hook schema documented by OpenAI and was verified
  by tests that replay that schema. It has not been run against a live Codex
  binary here. Codex fails open when a hook crashes or times out, so Crane
  denies with exit code `2`. Codex does not support `permissionDecision: "ask"`
  in `PreToolUse` yet, so any approval requirement is surfaced as a denial
  there. The `apply_patch` simulation matches hunks by exact text, so a hunk
  Codex would place with fuzzy matching may be judged as unknown and denied
  when it touches protected code.
- Pre-execution authorization simulates file writes. Shell commands and unknown
  tools cannot be simulated, so their effects are caught after they run (at
  post-tool-use and stop) rather than prevented. Shell inspection for `.crane`
  access is pattern matching, not a sandbox.
- Attestations are evidence objects and are not cryptographically signed. The
  session binding digest is an unkeyed SHA-256. It catches edited and partial
  session files, but someone who can write `.crane/runtime` can recompute it.
- Discovery resolves calls by name, not by type, so dynamic dispatch and
  calls through variables can be missed or left ambiguous. Its risk signals
  and service candidates are heuristics. The pinned Kotlin grammar (0.3.1, the
  last one built for tree-sitter 0.20) reports some single-line class and
  object bodies as syntax errors. Those files are marked `partial`, but their
  symbols are still read.
- Task orchestration runs in local mode. Asana task context comes from stored
  API responses (live fetching is not implemented), and the webhook endpoint
  is a minimal single-threaded HTTP server for localhost or a trusted proxy.
  Asana webhooks are authenticated with the shared token, and their HMAC
  signatures are not verified. Task state lives in `.crane/runtime/tasks`,
  which Git ignores. Stopping a session revokes its runtime authority, but it
  cannot terminate the agent process.
- Zones constrain writes whose files are known before they run; a shell command's effect on
  a zoned file is caught right after it runs (as an unauthorized effect), not prevented.
- Contract tests check the code against the checkpoint and do not execute it;
  behaviour is covered by the ordinary tests. Agent-authored tests are
  recognised from session journals, so a test file an agent wrote outside any
  Crane session counts as organizational.
- Autonomy changes are human-only because agent environments are detected
  (by their environment markers) and agent shell commands are inspected. A
  process that hides those markers and runs outside the hooks is not
  observed. The journal itself is protected only like the rest of `.crane`.
- Pricing reads what an action declares: the files a write names, and the
  text of a shell command, matched against configured path globs and command
  fragments. A shell command that writes production files without saying so
  is priced as an ordinary command. Its effect is still verified after it
  runs.
- The journal's hash chain detects edits, insertions, removals, and reordering
  of journal events. Anyone who can rewrite the whole journal can recompute
  the chain, and attestations are not signed. Anchor exported attestation
  digests elsewhere (CI artifacts, a ticket, a transparency log) when that
  matters. OpenTelemetry output is produced as OTLP/JSON for a collector's
  file receiver; Crane does not send it over the network.
- **Outbound messages are queued, not sent.** Slack messages and Jira or
  Asana completion calls are written to `.crane/runtime/delivery/outbox` for a
  forwarder to send; Crane makes no outbound network calls. Slack clicks
  reach Crane through `crane task serve` (or `crane deliver slack-action`).
- **The local provider uses this repository.** It merges with `git merge
  --no-ff` and needs the base branch checked out with no uncommitted tracked
  changes. A session that is not isolated has its changes moved onto the
  delivery branch, so the working tree returns to the base branch until the
  merge.
- **The `github` provider is untested.** It uses the GitHub CLI (`gh`) and
  has not been run against GitHub here.
- **The dashboard is local.** It serves one user on one address, behind a
  token printed at start. It has no SSO, accounts, or multi-tenancy.
- **Simulation has limits.** It replays what journals recorded: changed
  symbols and files, never raw arguments. Flow scope is approximated by its
  target, and false-positive classification is a heuristic based on how each
  session ended.
- **Packs are heuristics.** The Payments pack matches payment vocabulary in
  names and their surroundings. The Testing pack reads test files to recognize
  frameworks. Both only recommend.
- Isolation uses Git worktrees, not containers: an agent process can still reach files outside
  its worktree, which effect verification of the worktree does not observe.
- Sessions created by Crane 0.2.0 or earlier (session format 1) are rejected
  with "unsupported session format". Remove
  `.crane/runtime/sessions/<id>` to start a new session for that agent.
- Hooks installed by earlier versions lack the `SessionEnd` hook and the `*`
  matcher on `PostToolUse`. Reinstall them, or merge the generated settings,
  so that every tool call is journaled and verified.
- `deny-read`, `deny-write`, `require-read`, `require-write`, and `static-database` are intentionally not implemented yet.
