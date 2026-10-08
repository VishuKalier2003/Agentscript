# Crane CLI reference (v0.3.1)

Every command form, with what it does, what to expect, the variations it allows, and an example.

**Example values used throughout:**

| Thing | Example value |
|---|---|
| Repository | `acme/shop` |
| Jira task | `PAY-1830` |
| Governed session | `codex-task-PAY-1830-v1` |
| Zone recommendation | `payments` |
| Policy | `payments_core` |

**Conventions:**

- **🔒 = human only.** The command refuses to run inside an agent environment (Claude Code or
  Codex). When an agent tries it through a hook, the call is denied; when the command would raise
  the agent's own authority, the session is also quarantined.
- **`--confirm D12`** is the first 12 hex characters after `sha256:` of the digest you reviewed.
  Where to read that digest is noted next to each approve command.
- **`--json`** gives machine-readable output and works on almost every command.
- **Plain-language summaries.** Zone recommendations, policy proposals, task contracts, and
  completion events start with a `summary` block: the decision needed, what the record covers,
  what approving it does, and the ready-to-run commands, for example
  `"approve": "crane zones approve payments --approver YOUR_NAME --confirm bd0e26cf4f96"` (its
  `approval_code` is the D12). The readable views (`zones review`, `policy show`,
  `task contract show`, and the listings) print the same summary and end with a "Next step"
  block. The summary is derived from the record's own fields; it is never part of a digest or a
  decision.
- **Exit codes:** `0` OK, `1` error or a "not OK" result, `2` a hook blocked the action.

---

## 1. Core: AgentScript policies and checkpoints

### `crane init`
Creates `.crane/` with `policies/`, `checkpoints/`, `zones/`, `tasks/`, and `config.toml`. Safe to run again. `repo connect` runs it for you.
```bash
crane init
```

### `crane checkpoint [--name NAME]` 🔒
Records the current HEAD commit as a named trusted baseline. Policies and contracts compare against it. Re-recording an existing name moves the baseline, which invalidates contracts bound to it.
```bash
crane checkpoint --name baseline
```

### `crane protect --KIND TARGET [--policy NAME] [--checkpoint NAME] [scope SCOPE]` 🔒
Writes a `preserve` rule: the code must **not** change. It is refused unless the target exists exactly once at the checkpoint.
- `KIND` is `function`, `data`, `variable`, `class`, or `interface`.
- `SCOPE` is `block` (the default), `file`, `flow`, `folder`, or `all`.
```bash
crane protect --function PaymentService.charge --policy payments_core --checkpoint baseline scope flow
```

### `crane target --KIND TARGET [--policy NAME] [--checkpoint NAME] [scope SCOPE] [change_type CHANGE_TYPE]` 🔒
Writes a `target` rule: the code **must** change. `change_type` is one of:
- `logical_bn`: behaviour changed;
- `logical_cn`: complexity changed;
- `logical_sn`: structure changed, behaviour did not;
- `semantic`: only wording, comments, or layout changed.
```bash
crane target --function PaymentService.refund --policy task_refunds scope block change_type logical_bn
```

### `crane parse FILE`
Parses an AgentScript file and prints its rules. It errors on malformed syntax, such as a missing `;` or a missing checkpoint.
```bash
crane parse .crane/policies/payments_core.crane
```

### `crane check [--json]` and `crane check --agent`
Verifies every policy against its checkpoint and exits `1` when any rule fails. `--agent` prints the JSON report with repair hints.
```bash
crane check --json
crane check --agent
```

### `crane test-all`
Same as `crane check`.
```bash
crane test-all
```

### `crane context`
Prints the active contract as plain text, the way an agent sees it.
```bash
crane context
```

### `crane status`
Short summary of the repository and its policies.
```bash
crane status
```

### `crane discover [--json] [--full] [--policies]`
Builds the semantic inventory: symbols, calls, modules, services, and risk signals. It is read-only and works without `init`.
- Incremental by default; `--full` rebuilds everything.
- `--policies` adds candidate protections with their reasons.
```bash
crane discover --policies --json
```

---

## 2. Repository connection

### `crane repo connect [--provider github|git|local] [--checkpoint NAME] [--default-branch NAME] [--refresh] [--json]` 🔒
Connects the repository once. It:
1. initializes `.crane`;
2. detects the provider and owner/name from `origin`;
3. creates the trusted checkpoint if it is missing;
4. runs discovery.

Running it again is idempotent (`already_connected`); `--refresh` updates the stored details. It refuses a folder that is not a Git repository, and a `.crane` copied from another repository.
```bash
crane repo connect --provider github --default-branch main
```

### `crane connect [--refresh] [--json]` 🔒
Alias of `repo connect`.
```bash
crane connect --refresh
```

### `crane repo status [--json]`
Connection, branch, HEAD, the trusted checkpoint's state (fresh, stale, or diverged), discovery state, and policy and zone version drift.
```bash
crane repo status --json
```

### `crane repo inspect [--json]`
Readiness report that lists what is missing.
```bash
crane repo inspect
```

### `crane repo disconnect [--reason TEXT] [--forget]` 🔒
Disconnects but keeps the record. `--forget` deletes the record so this `.crane` can connect to another repository.
```bash
crane repo disconnect --reason "moving repository" --forget
```

---

## 3. Zones (risk regions)

### `crane zones [ZONE_ID] [--json]`
Shows the active zones and the files and symbols each covers. Give an id to see one zone.
```bash
crane zones payments --json
```

### `crane zones recommend [--by NAME] [--json]`
Runs discovery and proposes zones. Recommendations govern nothing until a human approves them; changed evidence produces a new revision.

| Category | Criticality | Autonomy |
|---|---|---|
| security | restricted | observe |
| payments | critical | assisted |
| data access, api, integrations, shared core | sensitive | delegated |
| tests | routine | delegated |
| protected regions | as configured | as configured |

```bash
crane zones recommend --by security-lead
```

### `crane zones recommendations [--json]`
Lists recommendations with status, criticality, autonomy, file count, and confidence.
```bash
crane zones recommendations
```

### `crane zones review ID [--claim] [--by NAME] [--json]`
Shows the rationale, signals, affected files, the exact zone file it would write, and its digest. `--claim` marks it as in review 🔒.
```bash
crane zones review payments --json
crane zones review payments --claim --by security-lead
```

### `crane zones approve ID --approver NAME --confirm D12` 🔒
Writes `.crane/zones/ZONE.zone`, which makes the zone active. Approving twice changes nothing. It never overwrites a hand-written zone, and every decision is audited.

D12 comes from the `digest` field of `zones review ID --json`.
```bash
crane zones approve payments --approver security-lead --confirm 3fa91c07be22
```

### `crane zones reject ID --approver NAME [--reason TEXT]` 🔒
Declines a recommendation. Rejecting twice changes nothing; the decision is audited.
```bash
crane zones reject api --approver security-lead --reason "too broad"
```

### `crane zones review map [--by NAME] [--json]`
Reviews the compact zone map `.crane/zones.map`: its lines, the zones it compiles to, the digest to confirm, and every shadowed or unresolved line. Parse errors are reported with line numbers. Nothing governs agents until the map is approved.
```bash
crane zones review map
```

### `crane zones approve map --approver NAME --confirm D12` and `crane zones reject map --approver NAME [--reason TEXT]` 🔒
Approval compiles the reviewed map into `.crane/zones/zones-map.zone` (one zone per criticality and autonomy pair). It is refused if the map changed since review.
```bash
crane zones approve map --approver security-lead --confirm 7f39ab6f4406
```

### `crane zones map [--file PATH] [--session ID] [--json]`
The agent's advisory view: per file, every symbol with its criticality, autonomy, expected decision (OK, ASK, or DENY), and contract markers. The whole-repository view is also written to `.crane/runtime/zones.agent.md`.
```bash
crane zones map --file payments/service.py
```

### `crane zones audit [--json]`
The hash-chained log of zone approvals and rejections, with chain verification.
```bash
crane zones audit
```

---

## 4. Policies and packs

### `crane policy status [--json]`
Active policies by layer (organization, repository, task), the persistent policy version, and its history.
```bash
crane policy status
```

### `crane policy propose [--name NAME] [--checkpoint NAME] [--min-confidence high|medium|low] [--json]`
Generates candidate AgentScript from discovery and stores it as a pending proposal. Nothing is activated.
```bash
crane policy propose --name payments_core --checkpoint baseline --min-confidence high
```

### `crane policy proposals` and `crane policy show NAME [--json]`
Lists proposals, or shows one with its candidates and digest.
```bash
crane policy proposals
crane policy show payments_core --json
```

### `crane policy edit NAME [--file PATH] [--by NAME]` 🔒
Records your edit of the candidate policy as a new revision. The edited text must parse.
```bash
crane policy edit payments_core --file ./payments_core.crane --by security-lead
```

### `crane policy approve NAME --approver NAME --confirm DIGEST_PREFIX` 🔒
Activates the proposal by writing `.crane/policies/NAME.crane`. It is refused if the candidate file changed after review.

D12 comes from the `policy_digest` field of `policy show NAME --json`.
```bash
crane policy approve payments_core --approver security-lead --confirm 8b1d0e44c9a2
```

### `crane policy reject NAME --approver NAME [--reason TEXT]` 🔒
```bash
crane policy reject payments_core --approver security-lead --reason "freezes too much"
```

### `crane policy regenerate NAME [--by NAME]` 🔒
Regenerates a proposal from the current repository.
```bash
crane policy regenerate payments_core --by security-lead
```

### `crane packs [list] | packs show payments|testing [--json] | packs propose payments [--name NAME]`
The built-in Payments and Testing policy packs. `propose` creates a pending proposal and never activates it.
```bash
crane packs list
crane packs show testing --json
crane packs propose payments --name payments_pack
```

---

## 5. Tasks: tracker intake (user-driven path)

Tasks come from `.crane/sources/jira/issues/KEY.json` (or `asana/tasks/GID.json`). A project must be mapped to this repository in `.crane/sources/config.json`.

### `crane task list [--json]`
Tasks of the connected repository, with their state. The repository must be connected.

States: AVAILABLE, PLANNING, NEEDS_CLARIFICATION, CONTRACT_PENDING_APPROVAL, READY (shown as READY TO RUN once a session is launched), RUNNING, VERIFIED, DELIVERY_PENDING, PR_REVIEW, MERGED, TASK_COMPLETION_PENDING, COMPLETION_RETRY_PENDING, COMPLETED, BLOCKED, FAILED.
```bash
crane task list
```

### `crane task show TASK_ID [--json]`
The normalized task, its contract, the launch binding (and whether it still verifies), and history.
```bash
crane task show PAY-1830 --json
```

### `crane task prepare TASK_ID [--checkpoint NAME] [--by NAME] [--json]` 🔒
Fetches the task, normalizes it, and compiles its contract. A vague task exits `1` with NEEDS_CLARIFICATION. Preparing an unchanged task returns the same contract.
```bash
crane task prepare PAY-1830 --checkpoint baseline --by payments-lead
```

### `crane task approve TASK_ID --approver NAME --confirm DIGEST_PREFIX [--json]` 🔒
Approves the task contract; approving twice changes nothing.

D12 comes from the `contract.digest` field of `task show TASK_ID --json`.
```bash
crane task approve PAY-1830 --approver payments-lead --confirm 5c7e2a90d1f3
```

### `crane task launch TASK_ID --agent claude|codex|generic [--autonomy MODE] [--isolate] [--by NAME] [--json]` 🔒
Creates the session bound to the approved contract (READY TO RUN).
- Launching again with the same agent returns the same session.
- A different agent is refused while one holds the contract.
- An obsolete contract is refused.
- `--isolate` runs the session in a separate git worktree.
```bash
crane task launch PAY-1830 --agent claude --autonomy delegated
```

---

## 6. Tasks: planner and contracts

### `crane task plan TASK_ID|TASK_FILE [--json] [--checkpoint NAME] [--propose]`
Derives the six contract sections without storing a contract. Exits `1` unless the status is `planned`.
- The sections are MUST_CHANGE, MUST_NOT_CHANGE, MAY_CHANGE, REQUIRES_APPROVAL, TASK_SCOPE, and EXPECTED_TESTS.
- The status is one of `planned`, `task_needs_clarification`, `task_conflicts_with_policy`, or `task_unrelated`.
- `--propose` stores the plan as a pending policy proposal.
```bash
crane task plan examples/tasks/PAY-1821.json --json
crane task plan PAY-1830 --propose
```

### `crane task contract compile TASK_ID|TASK_FILE [--checkpoint NAME] [--by NAME] [--json]` 🔒
Compiles a versioned, digest-bound contract. The result is one of:
- `proposed`;
- `clarification_required` (exits `1`);
- `conflicts_with_policy`;
- `unrelated`.

Compiling the same inputs again reports `unchanged`.
```bash
crane task contract compile PAY-1830 --checkpoint baseline
```

### `crane task contract show TASK_ID [--version N] [--json]` and `crane task contract list [--json]`
Shows a contract's sections, bindings, authority, and any invalidation; reading it also detects invalidation. `list` shows every contract.
```bash
crane task contract show PAY-1830 --version 1
crane task contract list
```

### `crane task contract approve TASK_ID --approver NAME --confirm DIGEST_PREFIX` 🔒
Approves the contract and activates its AgentScript. Vague, invalidated, or stale contracts are refused; approving twice changes nothing.
```bash
crane task contract approve PAY-1830 --approver payments-lead --confirm 5c7e2a90d1f3
```

### `crane task contract reject TASK_ID --approver NAME [--reason TEXT]` 🔒 and `crane task contract history TASK_ID [--json]`
Reject declines the contract. History shows every version and the hash-chained audit.
```bash
crane task contract reject PAY-1830 --approver payments-lead --reason "scope too wide"
crane task contract history PAY-1830
```

---

## 7. Tasks: webhook orchestration

### `crane task ingest --source jira|asana [--delivery ID] [EVENT_FILE...] [--json]` 🔒
Replays webhook deliveries (from files, or from stdin) through the lifecycle:

RECEIVED → ANALYZING → CONTRACT_PROPOSED → APPROVED → EXECUTING → VALIDATING → PR_READY → REVIEW → MERGED → COMPLETED, or BLOCKED, FAILED, CANCELLED, or DEGRADED.

Duplicate events are ignored.
```bash
crane task ingest --source jira tests/fixtures/orchestration/jira/01-created-PAY-1821.json
```

### `crane task sync [TASK_ID] [--json]` 🔒
Advances one task (or all tasks) as far as the facts allow: an approval was seen, a session started, an attestation was validated.
```bash
crane task sync PAY-1821
```

### `crane task status [TASK_ID] [--json]`
Task records with their history.
```bash
crane task status PAY-1821 --json
```

### `crane task advance TASK_ID --to STATE [--reason TEXT]` 🔒
Manual transition; only legal transitions are accepted.
```bash
crane task advance PAY-1821 --to REVIEW --reason "PR opened manually"
```

### `crane task serve [--addr 127.0.0.1:8787] [--once]` 🔒
HTTP endpoint for Jira and Asana webhooks. Set `CRANE_WEBHOOK_TOKEN` first. `--once` answers one request and exits.
```bash
CRANE_WEBHOOK_TOKEN=s3cret crane task serve --addr 127.0.0.1:8787
```

---

## 8. Tasks: Jira completion after merge

### `crane task completions [list] [--json]`
Lists completion events with their state: TASK_COMPLETION_PENDING, COMPLETION_RETRY_PENDING, or COMPLETED.
```bash
crane task completions list
```

### `crane task completions show EVENT_ID [--json]`
One event: task, repository, merge SHA, attestation, contract digest, and the result of each step.
```bash
crane task completions show completion-343b749d244822b0aaf5
```

### `crane task completions send [--now] [--task ID]` 🔒
Sends due events to Jira through `trackers.jira.transport`. It never duplicates a comment or transition.
1. Check the issue's status (an issue already done is recorded as completed externally).
2. Post the comment, once.
3. Transition the issue.
4. Check the status again.

A failure leaves the event `retry_pending`, with backoff from 60 s up to 1 h. `--now` ignores the backoff.
```bash
crane task completions send --now --task PAY-1830
```

### `crane task completions reconcile [--send]` 🔒
Restart recovery: re-queues missed events, recovers events that were stuck mid-send, and sends them with `--send`.
```bash
crane task completions reconcile --send
```

---

## 9. Agents: provider hooks

### `crane agent install --profile claude|codex`
Merges Crane's hooks into `.claude/settings.local.json` or `.codex/hooks.json`. Running it again prints "already installed".
```bash
crane agent install --profile claude
```

### `crane agent uninstall --profile claude|codex` 🔒
Removes only Crane's hooks and rules, and deletes the file if it held nothing else. An agent attempting this is quarantined.
```bash
crane agent uninstall --profile codex
```

### `crane agent hooks --profile claude|codex [--json]`
Validates the installed hooks:
- every event is registered exactly once;
- matchers cover all tools;
- the permission rules are present;
- `crane` is on PATH;
- Crane is initialized.

Exits `1` when the hooks are not valid.
```bash
crane agent hooks --profile claude --json
```

### `crane agent status [--profile claude|codex] [--session ID | --task ID] [--json]`
Hook validity and per-session status: attachments, whether the contract is current or obsolete, checkpoints, autonomy, safety, budget, and the last event.
```bash
crane agent status --task PAY-1830
```

### `crane agent hook --event EVENT [--profile generic|claude|codex] [--ttl SECONDS] [--task ID] [--autonomy MODE] [--idle-timeout SECONDS] [--max-actions N] [--max-files N] [--crane-session ID]`
The command the provider's hooks call. It reads the event JSON on stdin and answers in the provider's format.
- **Events:** `session-start`, `user-prompt-submit`, `pre-tool-use`, `post-tool-use`, `permission-request`, `stop`, `session-end`, or the host's own spellings (`PreToolUse`, ...).
- **Session binding order:** `CRANE_SESSION`, then `CRANE_TASK_ID`, then the provider's session id.
- **Failure:** any failure denies the action.
- **Creation options:** the other options apply only when the hook creates a new session.
```bash
echo '{"session_id":"abc","tool_name":"Edit","tool_input":{"file_path":"payments/service.py","old_string":"a","new_string":"b"}}' \
  | CRANE_SESSION=claude-task-PAY-1830-v1 crane agent hook --event pre-tool-use --profile claude
```

### `crane agent init [--profile generic|claude|codex]` and `crane agent verify [--profile ...]`
`init` initializes Crane and verifies (for `codex` it also installs hooks). `verify` runs verification in agent mode.
```bash
crane agent init --profile codex
crane agent verify --profile claude
```

---

## 10. Agent sessions (low level)

### `crane agent session [list | show SESSION_ID]`
Lists sessions with their lifecycle and verdict, or shows one session's binding, governance, activity, drift, and attestation.
```bash
crane agent session list
crane agent session show claude-task-PAY-1830-v1
```

### `crane agent session start --profile PROFILE --session ID [--task ID] [--autonomy MODE] [--isolate] [...]` 🔒
Creates a contract session by hand.
```bash
crane agent session start --profile claude --session s1 --task PAY-1830 --autonomy assisted
```

### `crane agent session verify SESSION_ID [--level fast|tests|full]`
- `fast`: checks the session's effects.
- `tests`: runs the affected tests.
- `full`: runs full validation and writes the attestation.
```bash
crane agent session verify claude-s1 --level full
```

### `crane agent session resume|cancel|finalize|quarantine SESSION_ID [--reason TEXT]` 🔒
Lifecycle control. `finalize` runs the mandatory contract tests and writes the final attestation. A cancelled session never acts again.
```bash
crane agent session finalize claude-s1
crane agent session cancel claude-s1 --reason "task withdrawn"
```

### `crane agent session extend SESSION_ID [--actions N] [--files N]` 🔒
Raises the session's action and file budget.
```bash
crane agent session extend claude-s1 --actions 50 --files 10
```

### `crane agent session cleanup SESSION_ID` 🔒 and `crane agent session sweep` 🔒
`cleanup` removes an ended isolated session's worktree and keeps its branch. `sweep` closes expired or idle sessions.
```bash
crane agent session cleanup claude-s1
crane agent session sweep
```

---

## 11. Governed sessions (the orchestrator)

### `crane session run TASK_ID --agent claude|codex|generic [--autonomy MODE] [--json]` plus one driver 🔒
One command takes an approved task to a verdict. The phases are:

TASK_READY → SESSION_CREATED → AGENT_CONNECTED → RUNNING (possibly DEGRADED or QUARANTINED) → STOPPING → RECONCILING → VERIFIED → DELIVERY_READY, or FAILED.

It exits `1` when the run ends FAILED. Running the task again after a failure creates a new session (`…-r2`).

**Driver 1: `--actions FILE [--approve]`.** Crane executes actions written in the neutral format.
- Only permitted actions run.
- Actions that need approval run only with `--approve`.
- `{"operation":"claim"|"stop"}` entries are recorded as untrusted agent claims.
- The run stops at a quarantine.
```bash
echo '[{"tool":"Edit","operation":"write","path":"catalog/labels.py","edits":[{"old":"return name","new":"return name.strip()"}]},{"operation":"stop","text":"done"}]' > ../actions.json
crane session run PAY-1830 --agent codex --actions ../actions.json --json
```

**Driver 2: `-- AGENT_COMMAND ...`.** Starts a real agent with `CRANE_SESSION` set; its hooks drive every action.
```bash
crane session run PAY-1830 --agent claude -- claude -p "Do task PAY-1830"
```

**Driver 3: `--detach`.** Prepares the session only (SESSION_CREATED). You start the agent yourself, then run `session finish`.
```bash
crane session run PAY-1830 --agent claude --detach
CRANE_SESSION=claude-task-PAY-1830-v1 claude
```

### `crane session finish SESSION_ID [--json]` 🔒
Terminates a governed session:
1. freezes execution;
2. reconciles from the repository, not from the agent's claims;
3. runs the contract tests;
4. runs the repository tests the testing policy asks for;
5. writes the attestation;
6. ends in VERIFIED and DELIVERY_READY, or in FAILED with the failed checks listed.

The repository tests follow `testing.json` `session_tests`: `affected`, `all`, or `none`. Finishing twice changes nothing, and a FAILED session stays FAILED.
```bash
crane session finish claude-task-PAY-1830-v1
```

### `crane session lifecycle SESSION_ID [--json]`
The phase record: history, the binding, action counts, and the termination result.
```bash
crane session lifecycle codex-task-PAY-1830-v1 --json
```

### `crane session inspect SESSION_ID [--json]` and `crane session inspect --export FILE`
Evidence timeline and attestation with chain verification; exits `1` if the chain is broken. `--export FILE` re-verifies an exported session.
```bash
crane session inspect codex-task-PAY-1830-v1
crane session inspect --export ./session-export.json
```

### `crane session export SESSION_ID --json [--otlp]`
Exports the evidence, or OpenTelemetry spans with `--otlp`.
```bash
crane session export codex-task-PAY-1830-v1 --json > session-export.json
crane session export codex-task-PAY-1830-v1 --json --otlp
```

---

## 12. Autonomy and budget

| Value | Options |
|---|---|
| Mode | `observe`, `assisted`, `delegated`, `autonomous` |
| Safety state | `active`, `degraded`, `quarantined` |

### `crane autonomy status [SESSION_ID] [--json]`
Mode, safety, what recovery still needs, and the next legal promotion.
```bash
crane autonomy status codex-task-PAY-1830-v1
```

### `crane autonomy history SESSION_ID [--json]` and `crane autonomy budget SESSION_ID [--json]`
`history` lists journaled transitions. `budget` shows the risk budget: current, max, reserved, consumed, and events.
```bash
crane autonomy history codex-task-PAY-1830-v1
crane autonomy budget codex-task-PAY-1830-v1 --json
```

### `crane autonomy promote|demote SESSION_ID --to MODE [--reason TEXT]` 🔒
Only legal steps are accepted, capped by the organization's maximum.
```bash
crane autonomy promote codex-task-PAY-1830-v1 --to autonomous --reason "clean track record"
crane autonomy demote codex-task-PAY-1830-v1 --to assisted
```

### `crane autonomy approve SESSION_ID [--reason TEXT]` 🔒
Human evidence for recovering from degraded or quarantined.
```bash
crane autonomy approve codex-task-PAY-1830-v1 --reason "reviewed the violation"
```

### `crane autonomy refill SESSION_ID --amount N --reason TEXT --approver NAME --expires DURATION` 🔒
Temporary budget refill. It never exceeds the maximum and never changes policy.
```bash
crane autonomy refill codex-task-PAY-1830-v1 --amount 200 --reason "large refactor" --approver lead --expires 2h
```

### `crane autonomy credit SESSION_ID --event task_milestone|human_review|merge --reference REF --approver NAME` 🔒
Regenerates budget for a milestone; each reference counts once.
```bash
crane autonomy credit codex-task-PAY-1830-v1 --event human_review --reference PR-7 --approver lead
```

---

## 13. Contract tests

### `crane test-contract [--json] [--plan] [--session ID] [--no-ordinary]`
Runs the generated contract tests, then the ordinary tests split into organizational and agent-authored. Exits `1` on failure.
- `--plan` lists the tests without running them.
- `--session` uses that session's bound contract.
- `--no-ordinary` skips the ordinary tests.
```bash
crane test-contract --plan
crane test-contract --session codex-task-PAY-1830-v1 --json
```

---

## 14. Delivery: PR, Slack, approval, merge

Delivery states: AWAITING_APPROVAL, APPROVED, REJECTED, MERGE_FAILED, COMPLETE, BLOCKED.

### `crane deliver run SESSION_ID [--json]` 🔒
Only works from a verified session: orchestrator DELIVERY_READY, or a finalization that passed. It is refused once the delivery is merged.
1. Commits the changes on `crane/SESSION`, with `Crane-Session`, `Crane-Task`, `Crane-Contract-Digest`, and `Crane-Attestation` trailers.
2. Runs the contract tests, repository tests, and configured checks.
3. Computes the binding digest (commit, tree, checks, contract, attestation).
4. Opens the PR: a local record, or GitHub through `gh` when `provider` is `github`.
5. Announces it on Slack.
6. Auto-merges if the merge rule allows it.
```bash
crane deliver run codex-task-PAY-1830-v1
```

### `crane deliver status SESSION_ID [--json]`
Delivery state, eligibility (with what is still missing), approvals, and exceptions.
```bash
crane deliver status codex-task-PAY-1830-v1 --json
```

### `crane deliver approve|reject|request-changes SESSION_ID --approver NAME [--reason TEXT] [--binding DIGEST]` 🔒
Records a human decision.
- It is bound to the current commit and check results.
- A stale `--binding` is refused.
- Recording the same decision twice returns `already_recorded`.
- Approvals expire when `merge_policy.approval_ttl_seconds` is set.
```bash
crane deliver approve codex-task-PAY-1830-v1 --approver payments-lead --reason "looks good"
crane deliver reject codex-task-PAY-1830-v1 --approver payments-lead --reason "missing test"
crane deliver request-changes codex-task-PAY-1830-v1 --approver payments-lead --reason "rename variable"
```

### `crane deliver exception SESSION_ID --check NAME --approver NAME --reason TEXT [--expires DURATION]` 🔒
A scoped, temporary pass for one failing check. Contract tests can never be excepted, and only checks the configuration allows qualify. It refreshes the PR body.
```bash
crane deliver exception codex-task-PAY-1830-v1 --check lint --approver payments-lead --reason "flaky linter" --expires 4h
```

### `crane deliver merge SESSION_ID [--by NAME]` 🔒
Merges only when eligible: the merge rule (by autonomy and zone criticality) is met, the binding is intact, and the contract is still current.
- **Success:** the merge commit becomes the trusted checkpoint, the Jira completion is queued, and the delivery becomes COMPLETE.
- **Failure:** MERGE_FAILED, and nothing is completed.
- **Repeat:** returns `already_merged`.
```bash
crane deliver merge codex-task-PAY-1830-v1 --by release-lead
```

### `crane deliver merged SESSION_ID --sha MERGE_SHA [--by NAME]` 🔒
Records a merge that happened on the host.
- The same SHA again changes nothing.
- A different SHA is refused.
- A merge that did not meet the policy is recorded as failed and never completes.
```bash
crane deliver merged codex-task-PAY-1830-v1 --sha 559243693fb78c6e6de91e5343948ddcaaa5b8aa --by github
```

### `crane deliver slack-action --body FILE --timestamp T --signature S` 🔒
Processes a Slack button click. The HMAC signature is verified (5-minute window) and the button must match the current binding.
```bash
crane deliver slack-action --body ./slack-request.txt --timestamp 1791272538 --signature v0=4f2a...e9
```

---

## 15. Golden path (the whole lifecycle)

### `crane flow [status] [TASK_ID] [--json]`
One top-level stage, the next step and who must act, every layer from repository to task completion, identifier link checks, and the terminal conditions. It works before the repository is connected, and records stage changes.

**Stages:** CONNECT_REPOSITORY, DISCOVERING, REVIEW_REQUIRED, POLICY_APPROVAL, AGENT_READY, TASK_READY, RUNNING, VERIFYING, DELIVERY_READY, REVIEW, MERGING, COMPLETING_TASK, COMPLETED.

**Failure branches:** NEEDS_CLARIFICATION, BLOCKED, DENIED, QUARANTINED, VERIFICATION_FAILED, DELIVERY_FAILED, MERGE_FAILED, COMPLETION_RETRY_PENDING.
```bash
crane flow
crane flow status PAY-1830 --json
```

### `crane flow advance TASK_ID [--now] [--by NAME] [--json]` 🔒
Runs the automatic steps until a human or the agent is needed: reconcile completions, deliver, merge once approved, complete Jira. Running it again changes nothing. `--now` ignores the Jira retry backoff.
```bash
crane flow advance PAY-1830 --by release-lead
crane flow advance PAY-1830 --now
```

### `crane flow audit [TASK_ID|repository] [--json]`
The hash-chained stage transitions with the identifiers they concern.
```bash
crane flow audit PAY-1830
crane flow audit repository --json
```

---

## 16. Dashboard

### `crane dashboard [serve] [--addr HOST:PORT] [--once]` 🔒
Serves the web control plane, by default on `127.0.0.1:8790`. Open the printed link; it carries the access token.

Screens: Flow, Repository, Tasks, Zones, Contracts, Agent Sessions, Policy Simulator, Attestations.
```bash
crane dashboard
crane dashboard serve --addr 127.0.0.1:9000
```

### `crane observe [PATH]` and `crane observe serve [--addr HOST:PORT] [--once]` (serve is 🔒)
The read-only observability API (see OBSERVABILITY.md). It answers GET only, on a fixed table of endpoints with typed parameters, and never changes anything. The server uses its own operator token, `CRANE_OBSERVE_TOKEN` (or a random token printed at start), and listens on 127.0.0.1:8791 by default. It loads every session and task record before it listens (a line on stderr says how many and how long), then revalidates them in the background, so pages lag the records by a few seconds at most.
```bash
crane observe /api/v1/overview
crane observe "/api/v1/decisions?decision=deny&policy=payments"
crane observe /api/v1/sessions/claude-obs-1/autonomy
CRANE_OBSERVE_TOKEN=... crane observe serve
```
Endpoints: `overview`, `repositories`, `agents`, `tasks[/ID]`, `sessions[/ID[/actions|/autonomy]]`, `decisions`, `violations`, `audit`, `whoami`, `endpoints`, all under `/api/v1/`. Lists take `limit` (at most 1000) and `offset`.
`crane observe serve` also serves the read-only Society Overview page at `/`. Open the link it prints. The global Overview data is `/api/v1/dashboard/overview?range=24h&agent=claude,codex` (see OBSERVABILITY.md, section 7.1). The Tasks page data is `/api/v1/dashboard/tasks?range=7d&q=PAY-18&sort=risk&page=1&page_size=50` (section 7.2). One task's details are `/api/v1/dashboard/tasks/PAY-184` (section 7.3). Agents are `/api/v1/dashboard/agents?range=7d` and `/api/v1/dashboard/agents/claude.claude-opus-5-5` (section 7.4). Runs are `/api/v1/dashboard/runs?range=24h&safety=quarantined` and `/api/v1/dashboard/runs/claude-obs-1`; policies are `/api/v1/dashboard/policies` and `/api/v1/dashboard/policies/payments`; zones are `/api/v1/dashboard/zones` and `/api/v1/dashboard/zones/money`; violations are `/api/v1/dashboard/violations?range=7d&severity=critical&status=unresolved` (section 7.6); repositories are `/api/v1/dashboard/repositories` and `/api/v1/dashboard/repositories/acme-shop` (section 7.7); autonomy and risk is `/api/v1/dashboard/autonomy?range=7d` (section 7.8). In a browser, `http://127.0.0.1:8791/runs/claude-obs-1?token=...` opens a run directly.

### `crane observe viewers` and `crane observe viewer add|remove` (add and remove are 🔒)
Grant or revoke read access to the observability API for one organization, repository, or team, without sharing the operator token. The token is printed once; Crane keeps only its SHA-256 digest, in `.crane/runtime/observe/viewers.json` (out of Git). `"*"` grants any value; leaving out `--team` grants every team. A viewer reads only its tenant's sessions, tasks, decisions, and violations; anything else answers 403 (repository or organization) or 404 (a record of another team). `viewers` lists names and grants, never tokens.
```bash
crane observe viewer add payments-oncall --organization acme --repository acme/shop --team payments
crane observe viewers
crane observe viewer remove payments-oncall
```

### `crane dashboard api GET|POST PATH [--body JSON]`
Calls the same API from the command line. POST is 🔒. In Git Bash, `export MSYS_NO_PATHCONV=1` first so paths stay intact.

| Area | Routes |
|---|---|
| Flow | `/api/flow`, `/api/flow/TASK`, `/api/flow/TASK/advance`, `/api/flow/audit` |
| Tasks | `/api/tasks`, `/api/tasks/ID/prepare\|approve\|launch`, `/api/tasks/contracts/...` |
| Zones | `/api/zones`, `/api/zones/recommendations/...`, `/api/zones/audit` |
| Sessions | `/api/sessions/run`, `/api/sessions/ID/lifecycle\|finish` |
| Other | `/api/contracts` (policy editor), `/api/policies/activation`, `/api/repo`, `/api/attestations`, `/api/simulate`, `/api/packs`, `/api/screens` |

```bash
crane dashboard api GET /api/flow/PAY-1830
crane dashboard api POST /api/tasks/PAY-1830/prepare --body '{"by":"lead"}'
crane dashboard api POST /api/contracts --body '{"draft":{"name":"payments_core","checkpoint":"baseline","rules":[{"rule":"preserve","kind":"function","target":"PaymentService.charge"}]},"by":"lead"}'
crane dashboard api POST /api/sessions/run --body '{"task":"PAY-1830","agent":"codex","actions":[{"tool":"Edit","operation":"write","path":"catalog/labels.py","edits":[{"old":"return name","new":"return name.strip()"}]}]}'
```

---

## 17. Help and version

```bash
crane help        # full command reference with explanations
crane --version   # prints: crane 0.3.1   (also: crane -V, crane version)
```

---

## Typical end-to-end sequence

```bash
crane repo connect
crane zones recommend && crane zones review payments --json
crane zones approve payments --approver security-lead --confirm <D12>
crane policy propose --name payments_core && crane policy show payments_core --json
crane policy approve payments_core --approver security-lead --confirm <D12>
crane agent install --profile claude && crane agent hooks --profile claude
crane task list && crane task prepare PAY-1830 && crane task show PAY-1830 --json
crane task approve PAY-1830 --approver payments-lead --confirm <D12>
crane session run PAY-1830 --agent claude --detach
CRANE_SESSION=claude-task-PAY-1830-v1 claude          # the agent works under Crane
crane session finish claude-task-PAY-1830-v1           # VERIFIED / DELIVERY_READY
crane flow advance PAY-1830                            # PR + attestation → REVIEW
crane deliver approve claude-task-PAY-1830-v1 --approver payments-lead
crane flow advance PAY-1830                            # merge → Jira closed → COMPLETED
crane flow status PAY-1830
```
