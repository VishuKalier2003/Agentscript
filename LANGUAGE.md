# AgentScript v0.1

AgentScript is the policy language consumed by Crane. It has two contract
primitives, `preserve` and `target`, each applied to a function, data,
variable, class, or interface. It is deliberately non-recursive:
there are no imports, inheritance, policy dependencies, macros, composition,
databases, or other policy primitives.

## Policy syntax

```text
policy payment_service {
    checkpoint baseline;
    preserve --function PaymentService.charge;
    preserve --function PaymentService.refund scope flow;
    target --function PaymentService.fee scope file change_type logical_bn;
    preserve --data PaymentService.RATE;
    preserve --interface Payable;
}
```

The grammar is:

```text
program := policy
policy := "policy" identifier "{" statement* "}"
statement := (checkpoint_statement | preserve_statement | target_statement) ";"
checkpoint_statement := "checkpoint" identifier
preserve_statement := "preserve" item qualified_name [ "scope" scope ]
target_statement := "target" item qualified_name target_option*
item := "--function" | "--data" | "--variable" | "--class" | "--interface"
target_option := "scope" scope | "change_type" change_type
scope := "block" | "file" | "flow" | "folder" | "all"
change_type := "logical_bn" | "logical_cn" | "logical_sn" | "semantic"
qualified_name := identifier (("." | "::") identifier)*
```

Every statement must end with `;`, one statement per line. The `policy`
line and the closing `}` must not have a `;`. A missing, doubled, or misplaced
`;` makes the policy malformed.

Identifiers contain ASCII letters, digits, `_`, or `-`. A policy contains one
explicit checkpoint and at least one preserve or target rule. A target's
`scope` and `change_type` may appear in either order, each at most once;
`change_type` is case-insensitive. Policy files are stored as
`.crane` files under `.crane/policies/`.

## Items

The flag after `preserve` or `target` says what kind of code item the rule
points at; `scope` and `change_type` work the same for every kind:

| Flag | Covers | Java | JavaScript | Python | Rust |
|---|---|---|---|---|---|
| `--function` | the function or method | methods, constructors | functions, methods | `def` | `fn` |
| `--data` | only the value stored in a variable | field/local initializer | `const`/`let`/`var` value | right side of `NAME = value` | `const`/`static`/`let` value |
| `--variable` | the whole declaration: name, type, modifiers, value | field or local declaration | `const`/`let`/`var` statement | `NAME = value` | `const`/`static`/`let` item |
| `--class` | the class as a whole, with its fields and methods | `class` | `class` | `class` | `struct`, `enum` |
| `--interface` | the interface as a whole | `interface` | none | none | `trait` |

`--data` passes when only the declaration around the value changes, such as
its type or modifiers; `--variable` does not. Targets are qualified by their
enclosing classes, interfaces, traits, and impl blocks, so a constant in a
class is `PaymentService.RATE` and a method in an interface is
`Payable.pay`. Variables are qualified by type, not by function, so a local
variable assigned more than once, or reused by name in two methods, is
ambiguous and fails closed with `duplicate_target`.

With `scope flow`, a function's flow follows its calls; the flow of any other
item starts from the functions that mention its name.

## Scopes

`scope` goes at the end of a preserve statement (or of `crane protect`) and
sets how much code around the target is protected. It defaults to `block`.

| Scope | Protected code |
|---|---|
| `block` | Only the target function's definition: its braces (Java, JavaScript, Rust) or indented block (Python). |
| `file` | The whole file that defines the target. |
| `flow` | The target, every function it calls, and every function that calls it, followed transitively in both directions across the codebase. The target may sit anywhere in the flow. The flow is traced first, then every function in it is compared. |
| `folder` | Every file in the folder that contains the target, including subfolders. |
| `all` | Every file in the repository. |

`flow` is traced from source by function name: a call to `validate` links to
every function or method named `validate` in any supported file, so the flow
can be larger than the runtime call graph but not smaller. Calls through
function values, reflection, or constructors (`new Foo()`) are not traced. The
flow is computed separately in the checkpoint and the worktree, so adding a
new caller or callee also changes the flow and fails the check.

`flow` finds candidate files with `git grep` for the names being traced and
parses only those, level by level. A syntax error in a file that mentions a
traced name fails the check with `parse_failure`; a syntax error in a file
unrelated to the flow does not affect it.

`file`, `folder`, and `all` compare every file Git sees in that area, tracked
or untracked but not ignored, so adding or deleting a file is a change. Crane
first compares Git content hashes of the current files with the checkpoint and
only reads and compares the files whose contents differ. Hashes are computed
from the file contents on disk, so file timestamps and the
`assume-unchanged` / `skip-worktree` index flags cannot hide an edit.
Supported source files are compared with comments removed (see below); any
other file is compared exactly, apart from line endings. `.crane/` and
`.claude/` are tooling metadata and are never part of a scope.

## Targets

`preserve` guarantees code did **not** change; `target` guarantees it **did**.
A target rule passes only when the code covered by its `scope` differs from
the checkpoint. `scope` takes the same values as for preserve (default
`block`, the function itself); for `file`, `folder`, and `all` the change may
be in any file of that area, and for `flow` in any function of the flow.

`change_type` (optional; any change when omitted) requires a particular kind
of change. Crane measures each unit of code (a function, or a file for the
file-based scopes) from its syntax tree and never judges intent:

| Measurement | What it captures |
|---|---|
| text | the code as written, line endings and trailing spaces normalized |
| shape | the token sequence without comments, with identifiers numbered by first appearance, so a consistent rename keeps it and a reorder does not |
| behaviour | the multiset of literals, operators, and called function names |
| complexity | loop count, maximum loop nesting depth, and branch count |

| `change_type` | Passes when |
|---|---|
| `logical_bn` (business) | behaviour differs: a literal, operator, or called function changed |
| `logical_cn` (complexity) | complexity differs: a loop or branch was added, removed, or nested differently |
| `logical_sn` (structure) | shape differs but behaviour and complexity do not: statements reordered or restructured |
| `semantic` (wording and alignment) | text differs but shape and behaviour do not: renames, comments, layout, or non-code files such as documentation |

A change that also touches behaviour, such as replacing a loop with `sum()`,
satisfies both `logical_cn` and `logical_bn`, but not `logical_sn` or
`semantic`, which require that behaviour did not change. Renaming a called
function counts as `logical_bn`, because Crane cannot tell a rename from
calling a different function.

A target that is not met yet fails with `target_unchanged` (nothing changed)
or `change_type_mismatch` (the change is of another kind; the message lists
what was detected). Both are repaired by the agent. In Claude Code hooks they
are work still to do rather than mistakes: they never block the user's prompt
or individual edits, where the agent receives them as context, and they
block only when the agent tries to stop. A stop that Claude Code is already
retrying because of a stop hook (`stop_hook_active`) is allowed through with
a warning, so an unreachable target cannot keep the agent in a loop.

Target rules can be written by hand or created with the CLI, which checks
that the function exists exactly once in the checkpoint before writing the
policy:

```text
crane target --function TARGET [--policy NAME] [--checkpoint NAME] [scope SCOPE] [change_type CHANGE_TYPE]
```

`scope` and `change_type` may also be written as `--scope` and
`--change-type` (or `--change_type`). The default policy name is
`target_<function>`. Unlike `crane protect`, the current code is not
compared with the checkpoint, because a target is expected to change. Agents
are blocked from running `crane target`, as they are from `crane protect`.

## Checkpoints and preservation

`checkpoint NAME` selects trusted baseline identity `NAME`. The corresponding
`.crane/checkpoints/NAME.json` records an immutable Git commit SHA. A
checkpoint is identity metadata, not a copied source snapshot and not an
approval of future changes.

`preserve --function TARGET` is the contract evaluated against that baseline:
the uniquely resolved source node for `TARGET` in the checkpoint must equal the
uniquely resolved source node in the current worktree.

Valid target forms are a top-level function such as `calculate_tax`, a
type/member target such as `PaymentService.charge`, or a Rust target such as
`Gateway::call`. Overloads are not disambiguated.

## Supported source languages and resolution

Crane resolves source files with these extensions:

- Java: `.java`
- JavaScript/JSX: `.js`, `.jsx`, `.mjs`, `.cjs`
- Python: `.py`
- Rust: `.rs`

The parser is selected by extension. Crane walks parsed syntax nodes and
matches the requested function name and, for qualified targets, its enclosing
type/implementation name. Exactly one match must exist in the checkpoint and
exactly one in the worktree. A duplicate target is ambiguous and fails closed.

The comparison uses the source text of the parsed node with only comments
removed. A comment is a syntax node that the grammar marks as a comment and
whose text starts with the language's comment marker: `#` for Python, and `//`
or `/*` for Java, JavaScript, and Rust (including `///` and `/** */` doc
comments). Python docstrings are string literals, not comments, so they stay
protected.

Everything else is compared as written: indentation, spacing within a line,
and line breaks are significant, because layout can change meaning (Python
blocks are defined by indentation). Only trailing whitespace, blank lines, and
CRLF versus LF line endings are ignored, since removing a comment leaves them
behind and Git may check files out with either line ending. Thus comment-only
and blank-line changes pass, while re-indentation, re-wrapping, and any code
change fail. This is not semantic equivalence: code that behaves the same but
is written differently is still different.

## Repository discovery

`crane discover [--json] [--full]` builds an advisory inventory of the
repository. It does not change what policies can target, how they resolve, or
how they are verified.

**Languages.** Discovery reads symbols from Java, JavaScript, Python, Rust,
TypeScript/TSX, Go, C/C++, and Kotlin. Two parser paths are used:

- For Java, JavaScript, Python, and Rust, it uses the resolver's own item
  matcher. A discovered symbol's qualified name is therefore exactly what a
  policy target resolves to. Discovery reports these symbols as `policy_target`.
- The other languages use discovery-only grammars. They are reported but are
  not targetable yet.

Discovery tolerates syntax errors and marks such files `partial`. Enforcement
still rejects them. Files over 1 MB and binary files are counted but not
parsed. Tool metadata (`.crane`, `.claude`, `.codex`) and vendored folders
(`node_modules`, `vendor`, `third_party`) are excluded.

**Ids.** Every symbol has an id of the form
`symbol:LANGUAGE:NAMESPACE.QUALIFIED`. The namespace follows the language's own
module convention:

| Language | Namespace |
|---|---|
| Java, Kotlin | the declared package |
| Go | the package folder |
| Python | the dotted module path |
| JavaScript, TypeScript | the module path |
| Rust | the crate and module path |
| C++ | the enclosing namespace, or the folder |

So a Java method keeps its id when its file moves within its package. When
two symbols share an id (for example, overloads), the later ones get `~2`,
`~3`, and so on, in path and line order.

Files, modules, folders, and services have ids too: `file:PATH`,
`module:LANGUAGE:NAME`, `folder:PATH`, and `service:PATH`.

**Relationships.**

- **Calls.** A called name resolves to callables of the same language family
  (JavaScript with TypeScript, C with C++). One in the same file is preferred,
  then ones in the same module, then a unique one anywhere. Names matching
  several candidates are listed as `ambiguous_calls` and are not linked.
- **Tests.** Tests are found by file convention (`src/test`, `tests/`,
  `FooTest`, `test_foo`, `foo_test`, `foo.test.ts`, `foo.spec.js`) and by
  symbol convention (`@Test`, `#[test]`, `test_*`, Go `Test*`). A test links to
  what it calls. A test file also links, at file level, to the code its
  callbacks call and to the file it is named after.
- **Services.** Folders with a build manifest (`pom.xml`, `build.gradle(.kts)`,
  `package.json`, `go.mod`, `Cargo.toml`, `pyproject.toml`, `setup.py`,
  `requirements.txt`, `CMakeLists.txt`, `Dockerfile`, `*.csproj`) are service
  candidates, as are the children of `services/`, `apps/`, `cmd/`, and
  `packages/`.
- **Folders, modules, and services** carry dependency counts derived from the
  call links.
- **Owners** come from `CODEOWNERS` (`.github/`, the root, `docs/`, or
  `.gitlab/`), and the last matching rule wins.

**Contracts and checkpoints.** When Crane is initialized, each compiled
clause is mapped onto the inventory with the resolver's matching:

- `resolved`, `missing`, or `ambiguous`, plus the anchor ids;
- the entities its scope covers (flow scope is approximated from the call
  graph).

Each checkpoint lists:

- its status;
- the policies using it;
- the commits since it;
- the files changed since it;
- the covered symbols in those files.

**Risk.** Risk signals are heuristics:

- sensitive names or paths, such as payment, refund, settle, auth, or token;
- fan-in and callers from other modules;
- loops and branches;
- size;
- entry points;
- persistence and network calls;
- missing tests.

A symbol scoring 4 or more is a critical candidate. When the symbol is
targetable and its qualified name is unique, discovery suggests a `preserve`
rule. Suggestions are for a human to review. Nothing is written.

**Incremental indexing.** File content ids come from Git: index ids for clean
tracked files, and `git hash-object` only for modified and untracked files.
Outlines are cached in `.crane/runtime/inventory/index.json` by content id, so
an unchanged file is never parsed again, even after a rename.

Resolving a called name depends only on the definitions with that name. A run
therefore re-resolves only:

- callers in changed files;
- callers of names defined in a changed file, before or after the change.

Every other caller reuses its stored links. `index` in the JSON reports the
files parsed and reused, and the symbols relinked. `--full` ignores the cache
and produces the same inventory.

## Zones

A zone classifies part of the repository so that an organization can say
"Payments is Critical", "Authentication is Restricted", or "Tests are
Routine".

Zones live in `.crane/zones/*.zone`. They are persistent and are written in
terms of meaning rather than file layout. `crane zones` resolves them again
against the current inventory every time it runs. Zones are an input to
authorization. They only restrict: a zone never grants a permission and never
relaxes a contract.

```
zone payments {
    criticality critical;          # routine | sensitive | critical | restricted
    autonomy assisted;             # observe | assisted | delegated | autonomous
    state active;                  # active | degraded | quarantined (default active)
    policy payments;               # optional policy reference
    select subsystem payments;     # one or more selectors
    select symbol java:com.acme.payments.PaymentService.charge;
}
```

Criticality, autonomy, and safety state are three separate concepts:

- **Criticality** says how important the resources are.
- **Autonomy** says how much an agent may do on its own: observe, propose
  for approval (assisted), change within its task (delegated), or change
  freely within its contracts (autonomous).
- **Safety state** says whether the zone is healthy.

A zone's effective autonomy is its declared autonomy, capped by its
criticality and its state:

| Cap | Maximum autonomy |
|---|---|
| criticality `restricted` | observe |
| criticality `critical` | assisted |
| criticality `sensitive` | delegated |
| state `degraded` | assisted |
| state `quarantined` | observe |

A zone becomes `degraded` while any of its selectors does not resolve
cleanly. Where zones overlap, the most restrictive values apply: the highest
criticality, the lowest autonomy, and the worst state.

| Selector | Matches |
|---|---|
| `symbol LANG:ID` or `symbol Type.name` | a symbol by inventory id (without `symbol:`) or by qualified name |
| `module LANG:NAME` or `module NAME` | every symbol and file in a module |
| `service PATH` or `service NAME` | every file of a service |
| `subsystem NAME` | every service, module, and folder with a name segment matching `NAME` (case-insensitive) |
| `policy POLICY` or `policy POLICY:RULE` | everything that policy's clauses cover |
| `tests` | every test file and test symbol |
| `folder PATH` | a folder and everything below it (depends on layout) |
| `path GLOB` | files matching a glob (depends on layout) |

In values, `*` and `?` match within one `/` segment, and `**` crosses
segments.

Each selector resolves to one of four statuses:

- `resolved`.
- `ambiguous`: an exact symbol, module, or service name matched several
  things. All of them are covered, because zones only restrict.
- `missing`: the selector resolved before and no longer does. Its target was
  deleted, renamed, or moved.
- `unresolved`: the selector never resolved.

For a missing selector, `crane zones` shows what it used to cover. It also
lists rename candidates:

- for a symbol, a new symbol of the same kind in the file that held it;
- for a module, service, subsystem, or folder, the place that now holds most
  of its former content (matched by Git content id).

Candidates are never applied automatically. A human updates the zone.

Each zone's last good resolution is kept in
`.crane/runtime/zones/resolution.json`.

A zone's `version` is a SHA-256 of its canonical definition. Reformatting or
reordering a zone file keeps the version. Checkpoints, commits, and policy
edits do not affect zones or their versions.

Conflicts that `crane zones` reports:

- `autonomy_exceeds_criticality`: the declared autonomy is above the
  criticality cap. The cap applies.
- `overlap`: zones covering the same code disagree. The most restrictive
  values apply.
- `policy_reference_missing`: the referenced policy does not exist or does
  not compile.
- `policy_reference_outside_zone`: the referenced policy covers nothing in the
  zone.
- `policy_requires_change`: a `target` rule requires a change to code that its
  zones only let agents observe.

Malformed zone files and duplicate zone ids are reported, and `crane zones`
then exits with code 1.

## Policy discovery and proposals

`crane discover --policies [--json]` answers the question "what should this
organization probably protect?". It uses fixed heuristics over the inventory
and the zones. It understands names, paths, and graphs, not meaning, and the
output lists every heuristic it used. Each candidate has a reason, a
confidence, a suggested rule (or zone), and the entities it affects.

| Signal | Observed when |
|---|---|
| `zone_critical`, `zone_restricted`, `zone_sensitive` | a zone gives the code that criticality |
| `payment_name`, `auth_name`, `secrets_name` | the symbol's own name contains a word from that vocabulary, such as charge, refund, authenticate, password, or "api key" |
| `payment_context`, `auth_context`, `secrets_context` | the enclosing type, module, service, or folder contains such a word |
| `high_centrality` | it has 5 or more resolved callers, or 3 from 2 or more other modules |
| `tested` | a test calls it |
| `owned` | CODEOWNERS gives it owners other than the repository-wide default |
| `migration`, `infrastructure`, `production_config`, `secrets_config` | the path follows the convention, such as `migrations/`, `*.tf`, `.github/workflows`, `*-prod.yml`, or `secrets.yml` |

Confidence follows fixed rules:

- **High** for a symbol in a Critical or Restricted zone, or with a vocabulary
  word in its own name confirmed by context, centrality, tests, or ownership.
- **Medium** for a vocabulary word in the name alone, a Sensitive zone, or
  context together with centrality.
- **Low** for context or centrality alone.

Candidates are non-test functions and methods, plus variables and constants
whose names concern secrets. Regions holding no targetable code, such as
migrations or configuration, get a zone suggestion instead of a rule. They are
High when a zone or a specific owner already marks them, or when they hold
secrets, and Medium otherwise. Output is sorted by confidence, then kind,
then id, and contains no times, so the same repository gives the same output.

`crane policy propose [--name NAME] [--checkpoint NAME] [--min-confidence
high|medium|low]` turns the uncovered candidates (Medium and above by
default) into candidate AgentScript. It writes two files, and neither is
active:

- `.crane/proposals/NAME.crane`: the policy.
- `.crane/proposals/NAME.json`: the reviewable record, `proposal_format` 1.
  It holds the status, the revision, the policy and its `policy_digest`, the
  candidates (each marked `included` or not), the zone suggestions, the
  source state (HEAD, contract version, zone set version, and whether it was
  generated in an agent environment), a `history` of every action, and the
  `activation` once approved.

The review commands:

- `crane policy proposals` and `crane policy show NAME [--json]` read
  proposals.
- `crane policy edit NAME [--file PATH] [--by NAME]` records an edited policy.
  The edited policy must parse, keep the proposal's name, and use a valid
  checkpoint. The edit becomes a new pending revision.
- `crane policy reject NAME --approver NAME [--reason TEXT]` rejects a
  proposal.
- `crane policy regenerate NAME` recomputes the proposal from the current
  repository as a new pending revision.
- `crane policy approve NAME --approver NAME --confirm DIGEST_PREFIX`
  activates a proposal.

`approve` is the only step that activates anything. It writes the policy to
`.crane/policies/NAME.crane`, and only when all of these hold:

- the proposal is pending;
- an approver is named;
- `--confirm` quotes at least the first 12 characters of the reviewed policy
  digest;
- the candidate file is exactly the recorded policy. An unrecorded edit is
  refused until `crane policy edit` records it.

Agent sessions that are already running keep their bound contract.

Agents cannot review or activate proposals:

- `approve`, `reject`, `edit`, and `regenerate` refuse to run when an agent
  environment marker is set: `CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT`,
  `CODEX_SANDBOX`, `CODEX_SANDBOX_NETWORK_DISABLED`, or `CRANE_AGENT`.
- The pre-tool hook denies those commands.
- Writes to `.crane` are denied as always.

An agent may still generate proposals. They are marked with the agent marker
that was found.

These controls stop an agent from approving through its tools. They are not
a sandbox. A process that can run arbitrary code as the user can bypass them.
So activation in CI should come from a reviewed change by a trusted process.

## Task contracts

A task is described in a tracker-independent file, `.crane/tasks/ID.json`.
Agents cannot write it, because it lives in `.crane`. See
`examples/tasks/PAY-1821.json`.

```json
{
  "task_format": 1,
  "task_id": "PAY-1821",
  "title": "Reject negative refunds",
  "description": "Make `PaymentService.refund` reject negative amounts without changing `PaymentService.charge`.",
  "acceptance_criteria": ["`PaymentService.refund` throws for amounts below zero"],
  "repositories": ["acme/shop"],
  "requester": "pm@example.com", "team": "payments", "priority": "high", "labels": ["payments"],
  "references": {"symbols": [], "must_not_change": [], "files": [], "modules": []}
}
```

`crane task plan ID [--json] [--checkpoint NAME] [--propose]` analyzes the
task against:

- the inventory;
- the active policies in `.crane/policies`, the policy pack;
- the zones;
- the checkpoint;
- earlier contracts: sessions bound to the task with `--task`, and earlier
  proposals.

It produces a task contract with six sections:

| Section | Contents |
|---|---|
| `MUST_CHANGE` | symbols the task changes, each with a `target` rule |
| `MUST_NOT_CHANGE` | code the task says to keep, code permanently preserved by a policy in scope, and direct callers or callees of a target outside the task scope, each with a `preserve` rule |
| `MAY_CHANGE` | the other symbols of the modules in scope |
| `REQUIRES_APPROVAL` | targets in zones whose autonomy is `assisted` (for example, Critical zones), and tasks spanning several services |
| `TASK_SCOPE` | the repositories, services, modules, and files in scope |
| `EXPECTED_TESTS` | tests that already exercise a target and their files, or a test to add when none does |

Only code-like references can give authority:

- `references`;
- backticked names;
- qualified names (`Type.method`, `Type::method`);
- paths;
- snake_case and camelCase identifiers.

Plain words never do. A reference is negated when its clause says the code
must stay as it is, for example "without changing `X`", "`X` must not
change", or "do not touch `X`". Clauses are split at commas, semicolons, and
words such as "but" and "without". A prose type name only adds its module to
the scope.

The plan has one of four statuses:

- `planned`: a candidate contract exists. It is never activated by planning.
  `--propose` stores it as the pending proposal `task_ID`, which needs human
  approval (`crane policy approve`). `crane policy regenerate` plans the task
  again.
- `task_needs_clarification`: `clarifications` lists each missing or unusable
  item with its field. Possible reasons:
  - an empty title, description, acceptance criteria, or repositories;
  - a reference to code that does not exist;
  - an ambiguous name, listed with every match;
  - wording that asks for unbounded changes, such as "everywhere" or "as
    needed";
  - no identifiable code to change;
  - code named both to change and to keep.
- `task_conflicts_with_policy`: a target is permanently preserved by an
  active policy, or its zones only let agents observe it. Only a human can
  change the policy or the zone.
- `task_unrelated`: none of the task's repositories is this one (by folder
  name or origin remote).

`crane task plan` exits with code 1 unless the plan is `planned`. Plans contain
no times, so the same task and repository give the same plan.

## Task orchestration (Jira, Asana)

External trackers feed the task-contract machinery through a `TaskSourceAdapter`.
An adapter only translates:

- webhook events into created, assigned, updated, reopened, closed, or
  cancelled;
- the task context into the task as the tracker's API returns it;
- the task into the common task input (`TaskContractInput`).

Repository mapping, planning, approval, sessions, and state live in one
lifecycle that is shared by every source.

Two adapters exist:

- `jira`: Jira webhooks. The webhook carries the issue. Descriptions in Atlassian
  Document Format or wiki markup become text, and inline code becomes
  backticks, so code references survive.
- `asana`: Asana webhooks. Asana events only name a task, so its context is read
  from `.crane/sources/asana/tasks/GID.json`, a stored API response. Live API
  fetching needs credentials and is not part of this local mode.

Linear and GitHub issues need their own adapters, which are not written yet.

`.crane/sources/config.json` holds:

- `agent_assignees`: the names or ids that mean "assigned to the society";
- the mapping from tracker projects to repositories and teams;
- the checkpoint;
- the agent profile and session lifetime used for task sessions;
- for Jira, the custom field that holds acceptance criteria.

Acceptance criteria otherwise come from an "Acceptance criteria" section of
the description.

Events are delivered in either of two ways:

- `crane task ingest --source jira|asana [--delivery ID] FILE...` replays
  stored deliveries, or reads stdin.
- `crane task serve [--addr 127.0.0.1:8787]` accepts `POST /webhooks/jira`
  and `POST /webhooks/asana` with `X-Crane-Token` equal to
  `CRANE_WEBHOOK_TOKEN`. It handles one request at a time and echoes Asana's
  handshake.

There is no message bus.

The lifecycle:

```
RECEIVED -> ANALYZING -> CONTRACT_PROPOSED -> APPROVED -> EXECUTING -> VALIDATING -> PR_READY -> REVIEW -> MERGED -> COMPLETED
                 |              |                             |  ^           |
                 v              v                             v  |           v
              BLOCKED <------ (rejected)                   DEGRADED      EXECUTING (not satisfied yet)
         any unfinished state -> CANCELLED;  FAILED on unrecoverable errors;  CANCELLED/COMPLETED -> RECEIVED on reopen
```

1. A task enters the lifecycle when it is created or assigned to one of the
   `agent_assignees`. Other tickets are ignored.
2. Analysis normalizes the task, maps its project to repositories, and writes
   `.crane/tasks/ID.json`, keeping each version in
   `.crane/runtime/tasks/ID/`. It then plans the task with `crane task plan`
   and proposes the contract `task_ID_vN`. The task becomes BLOCKED, with the
   reasons, in any of these cases:
   - the context is unavailable;
   - the project has no mapping;
   - the planner asks for clarification;
   - the task conflicts with a policy.
3. A human approves the proposal with `crane policy approve`. Then
   `crane task sync` (which also runs after every ingest) moves the task to
   APPROVED and starts its contract session (EXECUTING). The session's provider
   id is `task-ID-vN` and it is bound to the task id, so the same contract
   version never gets a second session.
4. When the session writes an attestation (at stop), sync validates it:
   - PASS leads to PR_READY;
   - drift, a repository mismatch, or a journal problem leads to DEGRADED;
   - anything else returns the task to EXECUTING.
5. A human or CI advances the task (`crane task advance ID --to
   REVIEW|MERGED|...`). Only transitions the state machine allows are accepted.

Idempotency and updates:

- Each event is processed once, by its delivery id or by a digest of the event.
- An update whose normalized task is unchanged keeps the contract version.
- A changed task gets a new contract version. A pending older proposal is
  superseded. A session working under an outdated contract is stopped, and the
  older approved contract stays active until the new version is approved; it
  is retired then.
- Closing or cancelling the ticket (or unassigning the society) cancels the
  task. If the task was MERGED, closing it completes the task instead.

Finishing a task always does three things:

- It closes the task's sessions. This only revokes their runtime authority;
  the worktree is never touched, so it is always safe.
- It supersedes any pending contract.
- It retires the active contract, moving it from `.crane/policies` to
  `.crane/retired`.

Agents cannot drive the lifecycle. `ingest`, `sync`, `advance`, and `serve`
refuse to run in an agent environment, and the pre-tool hook denies those
commands.

## Agent sessions

Claude Code, Codex, and generic agents stay external. Crane neither runs
models nor changes them. A provider-neutral session manager
(`src/agent_session.rs`) gives every agent session the same lifecycle:

```
task -> contract session -> agent adapter -> session start -> agent work -> tool events -> validation -> final reconciliation
```

A session binds the following when it is created. The binding digest covers
all of it, so none of it can be changed later:

- the task;
- the contract and checkpoint;
- the zone constraints of every file at that moment;
- the autonomy mode and the autonomy budget (mutating tool calls and distinct
  files);
- the task scope (the files of the task plan's modules);
- the idle timeout and the expiry;
- the agent identity (profile and provider session).

The journal adds what happened:

- start, last activity, and end times;
- the model the host reported;
- the budget used;
- the safety state.

`crane agent session show ID` prints all of it, and the attestation includes
it.

Every tool call is translated by its adapter into the common `AgentAction` and
decided by one engine, in this order:

1. Reads are allowed.
2. The `.crane` metadata guard applies.
3. The session must be usable: not expired, closed, cancelled, finalized,
   lapsed, or displaced.
4. The contract applies: preserve denies.
5. Governance applies:
   - an observe session may only read;
   - an exhausted budget denies and quarantines the session;
   - a quarantined session is denied;
   - a degraded session is capped at assisted;
   - each written file's zone caps autonomy (restricted means observe,
     critical means assisted);
   - in delegated mode, a write outside the task scope needs approval.

   Observe denies, assisted asks for human approval (Claude asks the user,
   Codex denies, because its PreToolUse cannot ask), and delegated or
   autonomous allows. Shell and unknown tools get the session-level checks,
   since their files are not known before they run.

| Mode | Default budget (actions / files) | Behavior |
|---|---|---|
| `observe` | 0 / 0 | read only |
| `assisted` | 100 / 50 | every change needs approval |
| `delegated` (default) | 300 / 100 | changes outside the task scope need approval |
| `autonomous` | 1000 / 500 | changes allowed within the contract and zones |

The lifecycle:

- **Start**: the host's session-start, or `crane agent session start` for an
  orchestrator. The model receives the contract listing plus a short SESSION
  block: task, mode, budget, scope, expiry, idle timeout, and zones.
- **Resume**: the host's session-start reactivates a closed session.
  `crane agent session resume ID` does the same for a human, restores lapsed
  authority, and lifts a quarantine.
- **Cancellation**: `crane agent session cancel ID`. A cancelled session
  never acts or resumes again. Cancelling a task cancels its sessions.
- **Timeout**: a session expires after its lifetime (8 hours by default) and
  cannot be resumed. A session idle past its idle timeout (30 minutes by
  default) loses authority until a session start, a user prompt, or a human
  resume. `crane agent session sweep` closes such sessions.
- **Finalization**: `crane agent session finalize ID` reconciles one last time,
  writes the final attestation, and ends the session. Completing a task
  finalizes its sessions.

A session degrades when the contract on disk drifts during it, and it is
quarantined when its budget is exhausted or when a human runs
`crane agent session quarantine ID`. `crane agent session extend ID --actions N
--files N` adds budget, which is journaled; the bound budget is never
rewritten.

Agents cannot manage sessions. The pre-tool hook denies every session command
except `list` and `show`, and `start`, `resume`, and `extend` also refuse to
run in an agent environment. Sessions created by earlier versions (session
format 2) are rejected; remove them to start new ones.

## Isolated execution and effect verification

**Isolation.** `crane agent session start --isolate` creates a Git worktree at
`.crane/runtime/worktrees/SESSION_ID`. The worktree is on its own branch
`crane/SESSION_ID`, starts from HEAD, and gets the agent hosts' local hook
settings copied in. The session is bound to that worktree; run the agent
there. Its changes never touch the repository until the branch is merged. Hooks
running inside the worktree use the repository's `.crane` (policies,
sessions, zones), never a copy of it.

Two more commands complete the picture:

- `crane agent session cleanup ID` removes the worktree of a cancelled or
  finalized session and keeps the branch.
- Task orchestration isolates task sessions when `.crane/sources/config.json`
  sets `"isolate": true`.

**Effects.** A tool is authorized before it runs, by the fast decision. After
it runs, Crane looks at what actually changed in the session's worktree,
whatever the tool claimed. It compares every file's Git content id with the
previous observation, so changes made by shell commands, scripts, and
generators are seen like edits. Each observation stores its content in Git's
object store, so earlier versions can be read back.

The journal's `effect` for every tool call lists:

- files added, modified, deleted, and renamed (renames are paired by content);
- symbols added, modified, removed, or moved, compared by content digest, with
  only the innermost changed symbols reported;
- the zones touched;
- the contract clauses checked, out of the total;
- the violations;
- the targets satisfied.

Verification runs at three levels:

1. **Fast effect verification**, after every tool call:
   - Only the contract clauses that the changed files (including deleted and
     renamed paths) or the changed symbols can affect are verified. An
     unrelated change checks nothing.
   - A change the session could not have made without approval is an
     `unauthorized_effect` that the agent must revert. That covers a file in a
     restricted or critical zone, or one outside the task scope in delegated
     mode, that no authorized or approval-gated tool call named.
   - Reads are not observed.
   - A session without an earlier observation (for example, a transient one)
     verifies every clause.
2. **Affected test execution**, at stop or with `crane agent session verify ID
   --level tests`:
   - The tests that exercise the changed files, through the inventory's test
     relationships, are run, along with changed test files. Nothing else runs.
   - The commands come from `.crane/testing.json`:
     `{"timeout_seconds": 300, "commands": {"python": ["python", "-m", "pytest", "{files}"]}}`.
     `{files}` expands to the test files. Agents cannot edit this file, since
     Crane runs these commands.
   - A failing or timed-out test is a `tests_failed` violation.
3. **Full session validation**, at stop and finalization, or with
   `crane agent session verify ID --level full`. It produces the full
   reconciliation of the bound contract, plus the cumulative effect since the
   session started (a reverted change leaves nothing), plus the affected tests
   of everything changed. All of it goes into the attestation.

## Contract tests

`crane test-contract` generates contract tests from the active contracts (with
`--session ID`, from the contract bound to that session, run inside its
worktree) and runs them, then runs the ordinary tests. The two are reported in
separate sections (`contract_tests` and `ordinary_tests` in `--json`).

Each clause becomes three contract tests:

| Rule | Check | Passes when |
| --- | --- | --- |
| preserve | `checkpoint` | the item equals its checkpoint version (comments ignored) |
| preserve | `identity` | the item resolves once on both sides with the same structure, business logic, and complexity, so a whitespace-only reformat fails `checkpoint` but passes `identity` |
| preserve | `scope` | nothing changed within its scope (`not_applicable` for `block`) |
| target | `changed` | the item changed within its scope, and not only through agent-authored test files |
| target | `change_type` | the change is of the declared `change_type` (`not_applicable` without one) |
| target | `scope` | the item resolves exactly once at the checkpoint and in the worktree |

Test statuses are `passed`, `failed`, `not_applicable`, or `scheduled` (with
`--plan`, which generates the tests without running anything). A malformed
policy becomes one failing test. Every contract test is decided by Crane's
verification of the code; no test file, and no test result, can satisfy one.

**Ordinary tests** run with `.crane/testing.json`:

- **Organizational tests** are the test files present at the contracts'
  checkpoints, plus any added outside agent sessions. They always run. Set
  `"suite": {"python": ["python", "-m", "pytest"]}` to run a language's whole
  suite in one command; otherwise `commands[language]` runs with `{files}`
  expanded to the test files.
- **Agent-authored tests** are test files that agents added, wrote, or renamed
  in any contract session of the repository (from the journals' effects and
  authorized writes, never reads). They run separately with
  `commands[language]` and are marked `agent_authored`.
- An organizational test that an agent changed stays organizational, and
  produces the non-blocking finding `organizational_test_modified_by_agent`.
  One deleted since the checkpoint produces `organizational_test_deleted`,
  which fails the ordinary tests.

The command exits non-zero when any contract test or ordinary test fails.

**At finalization**, `crane agent session finalize` always runs the session's
contract tests. The attestation gains `contract_tests` (counts and every test)
and `reconciliation.contract_tests_passed`. Each failed contract test adds a
`contract_test_failed` finding and makes `final_status` `FAIL`.

## Autonomy state machine

Each session carries two dimensions that never mix: **autonomy** and
**safety**. Promotions and demotions change only autonomy. Violations,
quarantines, and recovery evidence change only safety. What the session may do
is the lower of its autonomy and its safety cap: `degraded` caps it at
`assisted`, and `quarantined` caps it at `observe`.

**Legal transitions** (`crane autonomy status` prints them for the policy in
force):

| Dimension | From → to | Trigger | By | Requires |
| --- | --- | --- | --- | --- |
| autonomy | observe → assisted → delegated → autonomous | promote | human | safety active, one step at a time, at most `max_autonomy` |
| autonomy | any → any lower | demote | human | nothing |
| safety | active → degraded | violation | Crane or human | a non-critical violation |
| safety | degraded → quarantined | violation | Crane or human | `violations_to_quarantine` violations in one incident |
| safety | any → quarantined | critical violation, quarantine | Crane or human | a kind listed in `critical_violations` |
| safety | degraded → active | evidence | per condition | any one set in `recovery.degraded.any_of` |
| safety | quarantined → active (or degraded) | evidence | per condition | any one set in `recovery.quarantined.any_of` |

**Triggers.** Crane records these triggers itself:

- the first critical (else first) violation that fast effect verification
  finds after a tool call;
- `budget_exhausted`;
- `contract_drift`, once per session;
- `self_escalation`, when an agent runs `crane autonomy
  promote|demote|approve` or `crane agent session resume|extend`.

Unmet targets (`target_unchanged`, `change_type_mismatch`) are work still to
do, and setup errors are not behaviour, so neither moves safety.

**Recovery evidence** comes only from its own source. Evidence counts only if
it was recorded after the latest violation, and it is discarded once safety is
active again.

| Condition | Recorded when | By |
| --- | --- | --- |
| `verified_repair` | a full validation (`agent session verify --level full`, stop, finalize) finds no behaviour violation | Crane |
| `human_approval` | `crane agent session resume`, `crane autonomy approve`, or a human `agent session start` | human |
| `new_risk_budget` | `crane agent session extend` | human |
| `new_session` | a new session inherits the quarantine of the same agent's latest session on the same task | Crane |

**Who can trigger what.** An agent can trigger nothing. Every attempt is
journaled as `autonomy_rejected` and becomes a critical `self_escalation`
violation.

**Configuration.** Configuration lives in `.crane/autonomy.json`, and every
setting is optional. The policy is bound to each session when it starts (it is
covered by the binding digest), so replaying a journal always gives the same
states. An invalid policy fails closed: no session starts under it.

```json
{
  "max_autonomy": "autonomous",
  "single_step_promotion": true,
  "violations_to_quarantine": 3,
  "critical_violations": ["unauthorized_effect", "budget_exhausted", "self_escalation"],
  "recovery": {
    "degraded": {"to": "active", "any_of": [["verified_repair"], ["human_approval"]]},
    "quarantined": {"to": "active", "any_of": [
      ["human_approval", "verified_repair"],
      ["human_approval", "new_risk_budget"],
      ["human_approval", "new_session"]
    ]}
  },
  "grants": [
    {"zone": "authentication", "autonomy": "delegated", "task": "SEC-9",
     "approved_by": "security-lead", "reason": "token rotation"}
  ]
}
```

The policy is validated when it is loaded:

- Every recovery set must be non-empty.
- Every quarantine set must include `human_approval` or `new_risk_budget`.
- `violations_to_quarantine` is 0 (never) or at least 2.
- `self_escalation` is always critical.
- A session's requested autonomy is capped at `max_autonomy`.

**Precedence.** An earlier check is never relaxed by a later one:

1. Crane metadata.
2. The contract.
3. Zones, meaning criticality and zone safety.
4. Session safety.
5. Session autonomy.
6. Task scope and budget.

A Restricted zone caps every mode at `observe`. Only an organizational grant
in `.crane/autonomy.json` opens it, and the grant can be limited to a task.
When it does, the zone allows the grant's level, still capped by the zone's
safety state. A preserved item inside the zone stays protected.

**Journal.** Every accepted trigger that changes the state is an `autonomy`
event with:

- the trigger and actor;
- the `changes` (dimension, from, to);
- the resulting state and the policy version.

`crane autonomy history` lists these events with the initial state and the
rejected attempts. `crane autonomy status` shows:

- both dimensions and the effective level;
- the incident's violations and evidence;
- what recovery is still missing;
- the next legal promotion;
- the grants in force;
- the precedence.

## Autonomy budget

The autonomy budget measures risk-bearing autonomous authority. It is not
tokens, CPU, memory, or billing. Each session's budget is replayed from its
own journal under the risk-cost model bound to it when it starts, so budgets
are deterministic and never shared between sessions.

| Quantity | Meaning |
| --- | --- |
| `budget_max` | ceiling for the session's current autonomy (`max.<mode>`); a human promotion grants the difference, a demotion lowers it |
| `budget_current` | points left, including unexpired refills |
| `budget_reserved` | points held for actions authorized but not yet run |
| `budget_consumed` | points spent by executed actions |
| `budget_events` | every reservation, consumption, release, penalty, regeneration, refill, and refill request (`crane autonomy budget ID --json`) |

**Cost model** (`.crane/budget.json`, every setting optional). Multipliers
are percentages (100 = 1.0).

```json
{
  "max": {"observe": 0, "assisted": 50, "delegated": 100, "autonomous": 200},
  "operation": {"read": 0, "write": 2, "delete": 4, "execute": 3, "other": 3},
  "criticality": {"routine": 100, "sensitive": 150, "critical": 300, "restricted": 600},
  "environment": {"isolated": 50, "workspace": 100, "production": 500},
  "scope": {"task": 100, "repository": 125, "shared": 250},
  "reversibility": {"reversible": 100, "irreversible": 300},
  "sensitivity": {"none": 100, "contract": 150},
  "privilege_escalation": 60,
  "patterns": {
    "production_paths": ["**/prod/**", "**/production/**", "deploy/**", "**/*.prod.*"],
    "production_commands": ["kubectl ", "terraform apply", "helm upgrade", "--env=prod", "--context prod"],
    "shared_paths": ["lib/**", "libs/**", "shared/**", "common/**", "packages/**"],
    "irreversible_commands": ["rm -rf", "git push", "git reset --hard", "git clean", "drop table", "truncate table"],
    "privileged_paths": [".github/workflows/**", "CODEOWNERS", ".github/CODEOWNERS", "**/sudoers*"],
    "privileged_commands": ["sudo ", "chmod +s", "chown root", "setcap ", "gh secret", "git config --global"]
  },
  "regeneration": {"contract_completed": 20, "contract_tests_passed": 10, "task_milestone": 10, "human_review": 15, "merge": 20},
  "compliance": {"every": 10, "amount": 2, "cap": 20},
  "penalties": {"violation": 10, "critical": "zero"},
  "max_refill_expiry_seconds": 604800
}
```

**Pricing.** The price is the operation's base cost times the multipliers for
criticality, environment, scope, reversibility, and sensitivity, rounded up.
Privilege escalation adds a flat amount, and reads are free. For an action
that writes several files, the worst file sets each factor:
- **Criticality** is the zone's.
- **Scope** is `task` only when every file is in the task scope.
- **Sensitivity** is `contract` when a contract clause covers the change.
- **Environment** is `isolated` in a session worktree, unless a production
  pattern matches.

**Life cycle:**
- **Reservation.** An action the runtime allows outright reserves its price on
  the `pre_tool_use` event (`budget_reserve`, with its factors).
- **Consumption.** When the action runs, the `post_tool_use` event consumes the
  reservation (`budget_consume`).
- **Release.** Reservations of actions that never ran are released at the next
  session start, prompt, or stop (`budget_released`).
- **No charge for approvals.** An action that needs human approval costs
  nothing; the human carries that risk.

**Penalties** ride on the violation's `autonomy` event (`budget_penalty`). A
violation costs `penalties.violation`. A critical violation sets the budget to
zero, refills and reservations included, unless `penalties.critical` is a
number. Running out of budget is not penalized again.

**Regeneration** happens only through controlled events. Each event is
credited once per session (or once per reference) and never above
`budget_max`:

| Event | Source |
| --- | --- |
| `contract_completed` | a full validation that passes (stop, `verify --level full`) |
| `contract_tests_passed` | passing contract tests at finalization or `crane test-contract --session` |
| `task_milestone` | `crane task advance` to `PR_READY` or `COMPLETED`, or `crane autonomy credit` |
| `human_review`, `merge` | `crane task advance` to `MERGED`, or `crane autonomy credit` |
| `sustained_compliance` | every `compliance.every` compliant actions in a row, at most `compliance.cap` per session |

**Exhaustion.** When an action costs more than is available:
- the action needs human approval;
- the session records a `risk_budget_exhausted` violation once (safety
  `degraded`, that is, supervised);
- a `refill_requested` event asks for a human refill;
- autonomy cannot be promoted.

**Refill:**

```bash
crane autonomy refill SESSION_ID --amount N --reason TEXT --approver NAME --expires DURATION
```

The duration is in seconds, or a number with `s`, `m`, `h`, or `d`, up to
`max_refill_expiry_seconds`. A refill behaves as follows:
- The points never raise the budget above `budget_max`.
- They are spent before the base budget, earliest expiry first.
- Whatever is left of them expires at the expiry.
- The refill counts as `new_risk_budget` recovery evidence. With the default
  policy, that alone recovers a degraded session.
- A refill writes only to the session journal. Policies, zones, the models,
  and the session binding are untouched.

**Human-only commands.** `crane autonomy refill` and `crane autonomy credit`
are refused in an agent environment and are denied to agent shells. Either
attempt quarantines the session.

## Failure semantics

Crane never returns PASS when it cannot establish compliance:

- missing target: FAIL with `target_not_found`
- renamed or deleted target: FAIL with `target_not_found`
- duplicate target: FAIL with `duplicate_target`
- missing checkpoint: FAIL with `checkpoint_error`
- malformed or inconsistent checkpoint: FAIL with `checkpoint_error`
- missing checkpoint commit: FAIL with `checkpoint_error`
- unsupported source language: FAIL with `unsupported_language`
- parser error in a source file being examined: FAIL with `parse_failure`
- malformed policy: FAIL with `malformed_policy`
- target not changed: FAIL with `target_unchanged`
- target changed in the wrong way: FAIL with `change_type_mismatch`
- invalid `.crane/config.toml`: FAIL with `verification_error` for the
  configuration-level verification result

Comment-only and blank-line changes pass; indentation and other layout
changes fail with `source_changed`. Changes to unrelated functions or unrelated files do
not affect a preserve rule.

The current worktree may be dirty: Crane verifies the current files without
requiring a commit. A detached HEAD is also allowed because the checkpoint
uses an immutable commit SHA. Neither state changes checkpoint metadata.

Crane requires an existing Git repository and a `.crane` directory for
verification. `crane init` must be run inside the repository first. It does
not fetch missing commits, create checkpoints, or mutate source during checks.
The v0.1 configuration file is reserved and accepts only blank lines and
comments; any other content is rejected as invalid configuration.

## Exit codes and JSON

All successful commands return exit code `0`. A failed check, invalid command,
invalid policy, missing repository setup, or any verification uncertainty
returns a non-zero exit code. `crane check --agent` always emits JSON on
stdout, including setup failures; `crane check --json` emits the same stable
contract for policy evaluation.

The JSON object always has:

```json
{
  "status": "passed | failed",
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

`violations` is empty on success. On failure it is deterministically ordered
by policy path/name and rule order. Every violation contains all seven stable
fields; setup-level failures use `policy_id` `crane` and empty target and
checkpoint fields. `repair_owner` is `agent` for worktree source problems and
`human` for policy, checkpoint, and setup failures that require editing
`.crane`.

## Agent adapter contract

Crane exposes a universal adapter boundary with specialized profiles for
`generic`, `claude`, and `codex`:

```text
crane agent init --profile PROFILE
crane agent verify --profile PROFILE
```

`agent init` creates the normal Crane repository structure and immediately
runs the independent verification. `agent verify` runs verification and
returns the stable JSON result on stdout with a non-zero exit code for any
violation. The adapter does not edit source, create checkpoints, or repair
violations. The host agent consumes the returned text, performs its own repair,
and invokes verification again. A zero exit code is the completion signal for
the host workflow.

For Claude Code, `crane agent install --profile claude` writes project-local
`.claude/settings.local.json` hooks. The `SessionStart` hook emits `crane
context`; `UserPromptSubmit` runs `crane check --agent` for every submitted
prompt; `PostToolUse` runs it after editing tools; and `Stop` runs the same
verification before the agent finishes. These hooks are language-neutral and
invoke the `crane` executable from `PATH`. They do not change policy files,
checkpoints, or source code. Installation refuses to overwrite an existing
settings file. A passing hook exits `0`; a blocked prompt/edit/stop hook exits
`2` and emits the structured verification result.

## Runtime authority and contract sessions

Every rule compiles into a two-sided contract (the Policy IR, `src/ir.rs`):

| Rule | Runtime authority (before a tool runs) | Postcondition (after the agent is done) |
|---|---|---|
| `preserve` | `deny_write` over the covered code | covered code equals the checkpoint version |
| `target` | `permit_write` over the covered code | covered code differs from the checkpoint (with `change_type`, in that way) |

`permit_write` is not an allowlist: code that no clause covers stays writable.
It does not override `preserve` either: a write that changes a target and
preserved code is denied. The covered code is the rule's item plus its scope
(the file, the folder, the flow, or the whole repository). Both sides of a
clause are derived from one rule, so they cannot disagree.

For each policy, the IR records:

- the contract id (the policy name);
- the policy version (the SHA-256 of the policy file);
- the checkpoint name and its commit;
- the clauses;
- the contract hash, a SHA-256 over all of the above, including both sides of
  every clause.

A checkpoint is bound only if its file records its own name and a full commit
SHA. A branch, a tag, `HEAD`, or an abbreviated id could later resolve to
another commit, so a checkpoint that records one fails closed. Recreate it with
`crane checkpoint`.

The contract version is a SHA-256 over every contract hash and malformed
policy. Editing a policy, changing a clause, or re-baselining a checkpoint
therefore produces a new version. A malformed policy stays in the IR, marked
malformed, so that it fails closed.

A contract session (`.crane/runtime/sessions/<agent>-<session id>/`) binds one
agent session to one contract version. It is created on the first hook event
that carries a provider session id, and it is never replaced or refreshed:

- `session.json` is written once. It holds:
  - the session id, the agent, and the provider session id;
  - the optional task id;
  - the creation time and optional expiry;
  - the repository root and the repository id (a SHA-256 of the root commits);
  - the bound IR;
  - the runtime grants derived from the checkpoint (where each item lives, and
    each flow's members);
  - the binding digest, a SHA-256 over all of the above.

  Each time the file is loaded, Crane recomputes the digest and checks that the
  file belongs to its session id. A file that fails either check, is partial,
  or has an older layout denies every mutating tool until a human removes the
  session directory.
- `state` is `active` or `closed`. It is the only thing that changes.
- `journal.jsonl` has one line per event: sequence, time, session, agent,
  contract version, checkpoints, binding digest, tool, normalized operation
  and resources, decision, reasons, result, and a SHA-256 of the tool
  arguments. The arguments themselves are never stored.
- `attestation.json` holds the latest reconciled outcome.

To bind a task, pass `--task ID` to `crane agent hook` in the hook
configuration, or set `CRANE_TASK_ID` for the agent host. The task is fixed when
the session is created. A later hook that presents a different task is refused.

A session is used only in the repository it was bound to. If the root or the
repository id differs, every mutating action is denied and reconciliation fails
with `repository_mismatch`.

If an earlier attempt crashed before `session.json` was written, it may have
left a journal, a state, or an attestation behind. Those files are moved to
`recovered-<time>-<pid>/`, so the new session starts active with a clean
journal and the evidence is kept.

`.crane/runtime/` ignores itself in Git. Agents cannot write to it because it
is inside `.crane`.

Lifecycle:

1. **Session start** binds or loads the session and prints the model context:
   the `CRANE_CONTEXT_V1` listing plus an `ACTIVE CONTRACT` summary. The context
   is informational only and carries no authority material.
2. **Pre-tool-use** authorizes the proposed action without running the
   verifier. The checks apply in this order:
   - Read-only tools are allowed.
   - Any action that touches `.crane`, the hook settings, or runs a mutating
     `crane` command (`checkpoint`, `protect`, `target`, `init`,
     `agent init|install|hook`) is denied.
   - Every mutating action is denied while any of these holds:
     - the contract is incomplete (a malformed policy, a missing or moving
       checkpoint, a missing or ambiguous item);
     - the session has expired or is closed;
     - the session is in another repository;
     - the session file is invalid.
   - A write is simulated on the file's current text and diffed item by item.
     It is denied if it would change code that a `preserve` clause covers,
     unless it restores the checkpoint version. Comment-only edits never count
     as changes.
   - Shell commands and unknown tools are allowed, because their effects cannot
     be known in advance.
3. **Post-tool-use** journals the action and verifies only the clauses it could
   have affected:
   - none for reads;
   - all of them for shell commands and unknown tools;
   - for writes, the clauses whose covered code includes the file or whose
     names the file mentions.

   This is how an allowed shell command that modifies protected code is still
   caught.
4. **Stop** reconciles completely:
   - it verifies every postcondition with the bound IR;
   - it fails with `contract_drift` if the contract on disk no longer matches
     the bound version, naming each policy that changed, was added, or was
     removed, and each checkpoint that moved;
   - it fails with `repository_mismatch` if the session is in another
     repository;
   - it fails with `journal_error` if the journal has lines that do not belong
     to the session;
   - it writes the attestation.

   Blocking follows the verification rules: agent-repairable violations block,
   pending targets block only on a first stop, and human-only problems never
   block.
5. **Session end** reconciles, writes the attestation, and closes the session.
   A resumed provider session reactivates it with the same binding.

Hooks that do not carry a provider session id use a transient session. It makes
the same decisions but keeps no journal and writes no attestation.

The attestation contains:

- the session, agent, and task ids;
- the repository id and the binding digest;
- the bound, runtime, verification, and repository contract versions (the
  first three are always the same);
- each contract's id, hash, policy version, checkpoint, and commit;
- the drift from the contract on disk;
- the authorized and denied actions;
- a result for each `preserve` and `target` clause, plus any other findings;
- answers to the reconciliation questions: `forbidden_mutations_attempted`,
  `required_targets_changed`, `preserve_invariants_satisfied`,
  `change_types_satisfied`, `same_contract_version`, `same_repository`,
  `fully_reconciled`;
- `final_status` (`PASS` or `FAIL`);
- evidence ids: the journal event count and SHA-256, the Git HEAD, and the
  verification time.

The attestation is an evidence object and is not signed.

The `codex` profile reads Codex hook JSON: `session_id`, `cwd`,
`stop_hook_active`, `tool_name`, and `tool_input.command`. An `apply_patch`
patch is parsed into the files it adds, deletes, updates, or moves. It also
handles the `permission-request` event: a forbidden call is denied, and any
other call is left to the user's approval prompt.

The `generic` profile sends the neutral action format on stdin to
`crane agent hook`:

```json
{"session_id": "run-1", "tool": "apply_patch", "operation": "write",
 "path": "src/pay.py", "edits": [{"old": "a", "new": "b", "all": false}]}
```

`operation` is `read`, `write`, `execute`, or `other`. A write gives `content`,
`edits`, or `"delete": true`. An `execute` action gives `command`. Stop events
may send `stop_hook_active`. For pre-tool-use, the verdict is printed as
`{"decision", "reasons", "resources"}`, and a denial exits `2`.
