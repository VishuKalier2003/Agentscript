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
| `symbol FILE::Type.name` or `symbol FILE::Type.*` | a symbol, or every member of a type, in one file (either part may be a pattern) |
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

### Symbol granularity

A `symbol` selector restricts only changes to the symbols it selects. Every
other selector kind restricts every change to the files it covers. When an
agent writes a file, Crane works out which symbols the change adds, removes,
or modifies (a member's change also changes its enclosing types). It then
applies the file's whole-file zones plus the zones of those symbols. When the
changed symbols cannot be determined (an unsupported language, a new text
that does not parse, a new or renamed file), every zone touching the file
applies. The same rule holds for the effects Crane verifies after a tool runs.

Delivery approval and the autonomy budget's risk cost still use every zone
touching a changed file.

### The zone map

`.crane/zones.map` is a compact, one-line-per-target alternative to zone
files:

```
# target                                     criticality autonomy   # note
payments/**                                  routine     autonomous # the whole service
payments/service.py::PaymentService.charge   critical    assisted   # money moves here
payments/service.py::PaymentService.*        sensitive   delegated
payments/models.py                           critical    assisted
```

A target is a folder (`DIR/**`), a file or file pattern, `FILE::Symbol`, or
`FILE::Class.*`. Blank lines and `#` comments are allowed; a note follows
`#` after whitespace. Errors are reported with their line numbers.

The map is a draft until a human approves it:

```
crane zones review map
crane zones approve map --approver NAME --confirm D12
```

Approval compiles each distinct (criticality, autonomy) pair into one ordinary
zone, `map_CRITICALITY_AUTONOMY`, and writes them to
`.crane/zones/zones-map.zone`. That file holds ordinary zones, so the
authority engine reads no new format. The digest covers the compiled zones,
so reordering, reformatting, or editing notes changes neither the digest nor
the zone set version. A changed map is reported as "changed since approval"
until it is reviewed and approved again. Approving a changed map changes the
zone set version, which invalidates contracts bound to the old version.

Overlaps resolve as for any zones: the most restrictive values win. `crane
zones` and `crane zones review map` list each line that stricter lines fully
shadow (it changes no decision) and each line that matches nothing (its zone
is degraded). Agents may never write the map: it is `.crane` metadata.

### The agent's zone view

`crane zones map [--file PATH] [--session ID] [--json]` prints, per file,
every symbol with its effective criticality and autonomy, the decision an
edit to it would get (`OK`, `ASK` for human approval, or `DENY`), and the
contract markers on it (`preserve(POLICY)`, `TARGET(POLICY)`). It lists the
files that zones touch, the task scope's files, and files with
contract-covered symbols. The decisions come from the same calculation the
authority engine uses. The view is advisory: enforcement stays in the
authority engine.

The whole-repository view is written to `.crane/runtime/zones.agent.md`. It is
regenerated when a session starts (for that session's governance) and after
zone, map, policy, and task contract approvals (for new sessions). A
session's start context stays short: a pointer to the file plus one line
for each file (at most 15) whose edits need approval, are denied, or touch
contract markers, naming only the symbols that differ from the rest of the
file (of the task scope's files when a task is bound).

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
the scope. A task that names exactly one type (class or interface) and no
method is bounded to that type: the type is `MUST_CHANGE`, and the rest of its
module may change. Several types and no method need clarification.

The plan has one of four statuses:

- `planned`: a candidate contract exists. It is never activated by planning.
  `--propose` stores it as the pending proposal `task_ID`, which needs human
  approval (`crane policy approve`). `crane policy regenerate` plans the task
  again.
- `task_needs_clarification`: `clarifications` lists each missing or unusable
  item with its field. Possible reasons:
  - an empty title, description, or repositories (empty acceptance criteria
    are advisory: `blocking: false`, and `EXPECTED_TESTS` asks for a
    regression test instead);
  - a scope covering more than half of the repository's source files (in
    repositories of at least 10 source files): that is repository-wide
    authority;
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

## Task contract compilation and policy activation

**Policy activation state.** `crane policy status [--json]` (or
`GET /api/policies/activation`) lists every policy in `.crane/policies` with:

- its digest, checkpoint, and rule count (or why it is malformed);
- its origin: the approved proposal and approver, or a hand-written file;
- its layer.

| Layer | Policies |
|---|---|
| `organization` | the names listed in `.crane/organization.json` under `policies` |
| `task` | task contracts (approved from a task proposal, or named `task_*`) |
| `repository` | every other policy |

The persistent policy version is the digest of the organization and
repository layers. Task contracts are left out of it, so approving one never
invalidates another. Each change of that version is recorded in
`.crane/policy-activation.json` with the policies activated, changed, or
deactivated. Nothing is recorded on behalf of an agent.

**Compilation.** `crane task contract compile TASK [--checkpoint NAME]`
plans the task and stores `.crane/task-contracts/TASK/vN.json`
(`task_contract_format` 1). A task file given as a path is first stored as
`.crane/tasks/ID.json`. The record holds:

| Field | Content |
|---|---|
| `contract_id`, `version`, `status`, `digest` | identity, lifecycle, and the SHA-256 of everything below |
| `contract` | `MUST_CHANGE`, `MUST_NOT_CHANGE`, `MAY_CHANGE`, `REQUIRES_APPROVAL`, `TASK_SCOPE`, `EXPECTED_TESTS` |
| `bindings` | see below |
| `clarifications`, `conflicts` | why the contract cannot be approved |
| `agentscript`, `policy_name`, `proposal` | the executable contract (`task_ID_vN`) and its pending policy proposal |
| `authority` | whether the contract grants anything, and why |
| `approval`, `rejection`, `invalidation`, `history` | decisions and what happened |

The `bindings` are:

| Binding | Value |
|---|---|
| `repository` | identity (the digest of the root commits), owner, and name |
| `task` | id, and digest of the task file |
| `checkpoint` | name and full SHA |
| `policy_version` | the persistent policy version |
| `zone_set_version` | the zone set version |
| `autonomy` | autonomy policy version and maximum autonomy |
| `budget` | risk-cost model version, and the budget at the maximum autonomy |
| `organization` | organization, team, organization policies, and their digest |

An identical compile changes nothing (`unchanged: true`).

**Statuses.**

| Status | Meaning |
|---|---|
| `proposed` | planned; waits for a human |
| `clarification_required` | the task is vague; the command exits with code 1 |
| `conflicts_with_policy` | a permanent policy or zone forbids the change |
| `unrelated` | the task is for another repository |
| `approved` | a human approved its digest |
| `rejected` | a human declined it |
| `superseded` | a newer version replaced it |
| `invalidated` | a binding changed |
| `retired` | the task finished |

None of these grants authority except `approved`.

**Approval.** The command is:

```
crane task contract approve TASK --approver NAME --confirm DIGEST_PREFIX
```

The digest prefix is at least 12 characters. Approval:

- refuses an agent environment, and any status but `proposed`;
- refuses a proposed contract whose task file changed since it was compiled;
- activates the AgentScript through its policy proposal (as
  `crane policy approve task_ID_vN` would);
- retires an earlier approved version;
- is audited.

Approving again returns `already_approved`. Approving the policy proposal
directly approves the contract too. `crane task contract reject TASK
--approver NAME [--reason]` declines a contract that awaits a decision;
rejecting again returns `already_rejected`.

**Versioning and invalidation.** Every read compares a proposed or approved
contract's bindings with the repository. When the repository, checkpoint SHA,
persistent policy version, zone set, autonomy policy, budget model, or
organization changed, the contract becomes `invalidated`:

- `invalidation.changed` lists each binding with its bound and current value;
- a pending proposal is superseded, and an approved task policy is retired;
- the event is audited.

An invalidated contract is compiled again as a new version. A changed task
only sets `task_changed`: it is versioned by compiling again, and an approved
earlier version stays in force until the new one is approved. Agents' reads
report invalidation without writing it.

**Sessions.** A session bound to a task (`--task`, `CRANE_TASK_ID`, or
orchestration) records the contract in `governance.task_contract`, and takes
its scope from `TASK_SCOPE`. Unless the contract is `approved`, the session's
autonomy is capped at `assisted`. A task without a compiled contract is planned
at session start, and only a `planned` one keeps its autonomy. Writes outside
the task scope need approval in `delegated` and `autonomous` mode.

**Orchestration.** Tracker tasks are compiled through the same layer:

- a `proposed` contract is `CONTRACT_PROPOSED`, and any other status is
  `BLOCKED`;
- an invalidated contract cancels the task's sessions and blocks it;
- `crane task sync` adopts a newer contract version compiled by a human.

**Audit and protection.** Compilations, approvals, rejections, and
invalidations are appended to `.crane/task-contracts/audit.jsonl`,
hash-chained (`crane task contract history TASK`). Agents cannot write
`.crane`. `crane task contract compile|approve|reject` refuse agent
environments, and an agent shell running them is denied and quarantined
(self-escalation).

**Dashboard.** The API mirrors the CLI:

- `GET /api/tasks/contracts` and `GET /api/tasks/contracts/ID`;
- `GET /api/tasks/contracts/ID/history`;
- `POST /api/tasks/contracts/ID/compile|approve|reject`.

## Task intake: from a tracker task to a launched session

The intake is the user-driven path:

1. The repository is connected.
2. The user chooses a task.
3. Society fetches it.
4. Society validates and compiles its contract.
5. A human approves the contract.
6. The user selects Claude or Codex.
7. The session starts.

It reuses the task source adapters (`TaskSourceAdapter`). `list` returns the
tasks a source offers (local mode: `.crane/sources/jira/issues/*.json`,
`.crane/sources/asana/tasks/*.json`), and `done` tells whether the tracker
closed the task. Mapping, planning, and launching are source-independent. The
runtime authority engine only sees a task id and a task contract.

| Command | Effect |
|---|---|
| `crane task list` | tasks whose project maps to the connected repository, with state, contract, agent, checkpoint, and session; tasks of other projects are counted under `unmapped`; refused until `crane repo connect` |
| `crane task show ID` | the task as normalized from the tracker, its assignees, the current task contract, the launch with its verification, and the intake history |
| `crane task prepare ID [--checkpoint NAME]` | fetches the context, normalizes it with the repository mapping into `.crane/tasks/ID.json`, and compiles the contract (`crane task contract compile`); a vague task exits with code 1 in `NEEDS_CLARIFICATION`; a failure is recorded as `FAILED` |
| `crane task approve ID --approver NAME --confirm PREFIX` | approves the current contract (`crane task contract approve`) |
| `crane task launch ID --agent claude\|codex\|generic [--autonomy MODE] [--isolate]` | opens the contract session with provider session id `task-ID-vN` (Crane session `AGENT-task-ID-vN`, `-rK` after an ended one), bound to the task |

**States** (derived each time from the tracker, the orchestration record, the
contract, and the session; changes are kept in
`.crane/runtime/intake/ID.json`):

| State | When |
|---|---|
| `AVAILABLE` | mapped, no contract |
| `PLANNING` | while `prepare` compiles |
| `NEEDS_CLARIFICATION` | contract `clarification_required` |
| `CONTRACT_PENDING_APPROVAL` | contract `proposed` |
| `READY` | contract `approved`; `ready_to_run` is true once a session is launched and the agent has not connected ("READY TO RUN") |
| `RUNNING` | the session journaled the agent's start or actions |
| `COMPLETED` | done in the tracker, task lifecycle completed, contract retired, or session finalized |
| `BLOCKED` | contract conflicting, unrelated, rejected, superseded, or invalidated; a launch binding that no longer verifies; or cancelled in the tracker |
| `FAILED` | fetching or planning failed (after the latest contract) |

**Launch binding.** A launch records `launch.binding` and its SHA-256
`launch.binding_digest`. The binding holds:

- the task id, contract id, version, and digest;
- the session id and its own binding digest;
- the agent;
- the repository identity, owner, and name;
- the checkpoint name and SHA;
- the autonomy mode, initial safety, maximum autonomy, and autonomy policy
  version;
- the budget (mutating actions, files, risk model version, risk budget);
- the zone set version and zones;
- the persistent policy version and the contract set version;
- the session's `task_contract`.

Every read rebuilds it from the session (whose own binding digest is checked
on load) and the stored contract. The task is `BLOCKED` unless the rebuilt
binding, the recorded binding, and the recorded digest all agree.

**Idempotence.**

- Planning an unchanged task returns the same contract version.
- Approving again returns `already_approved`.
- Launching again with the same agent returns the session already bound to
  the contract (`launched: false`).
- Another agent is refused while a resumable session holds the contract.

**Obsolete contracts.** A launch is refused when:

- the contract is not `approved`, or is invalidated;
- a newer version exists;
- the task file changed;
- the tracker's task no longer normalizes to what was planned;
- the tracker closed the task.

After launch, a mutating tool call of a session bound to an approved contract
is denied once that contract version is superseded, retired, rejected, or
invalidated.

**Protection.** `crane task prepare|approve|launch` refuse agent environments.
An agent shell running them is denied, and `approve` or `launch` from an agent
quarantines its session.

**Dashboard.** The Tasks screen and its API:

- `GET /api/tasks`, `GET /api/tasks/ID`;
- `POST /api/tasks/ID/prepare|approve|launch`.

Task completion stays with the orchestration and delivery lifecycle.

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

## Evidence and attestation

**Journal.** `.crane/runtime/sessions/ID/journal.jsonl` is the authoritative,
append-only record. Each event is stamped with:

- its sequence number and time;
- the session, agent, contract version, checkpoints, and binding digest;
- a `chain` link: `sha256(previous link + "\n" + the event's canonical JSON)`,
  starting from `genesis`.

Verification checks that sequence numbers have no gaps, that time never runs
backwards, and that every link chains. The result is one of:

| Status | Meaning |
| --- | --- |
| `verified` | the whole journal chains |
| `legacy` | the journal was written before chaining |
| `partial` | chaining started partway through |
| `broken` | any edit, insertion, removal, or reordering |

`crane session inspect` refuses a broken journal.

**No raw arguments.** Tool calls are journaled as `arguments_digest` (SHA-256
of the tool input) and a `summary`:

- shell commands: the programs run and the argument count;
- writes: the number of files and the kind of each change.

Command text, file contents, and argument values are never stored.

**Evidence records.** One record per journal event (`evidence_format` 1). Each
record is replayed from the session's bound document and journal alone:

| Field | Content |
| --- | --- |
| `organization`, `team` | from `.crane/organization.json` (or the `origin` remote's owner), bound at session start |
| `agent`, `session`, `task` | session identity |
| `contract`, `checkpoints` | contract version and `name@sha` checkpoints |
| `autonomy`, `safety` | state after the event |
| `budget` | `before` and `after`: current, available, reserved, max |
| `tool`, `operation`, `resources`, `action_digest`, `action_summary` | the action, without raw arguments |
| `decision`, `reasons` | the policy decision |
| `violations` | violation kinds found |
| `repair` | `compliance_restored` (a passing check after a failing one) or `verified_repair` |
| `tests` | ordinary tests at stop; contract and ordinary tests at finalization |
| `human` | a start, prompt, approved tool call, resume, promotion, demotion, approval, refill, credit, limit extension, cancellation, or rejected agent attempt |
| `category`, `event`, `seq`, `at`, `chain` | classification and position |

**Attestation.** The attestation is a pure function of the bound document and
the journal. Object keys are sorted, so the same evidence always yields the
same bytes and the same `attestation_digest`.

At finalization, the `session_finalized` event records the final status, the
findings, the contract test counts and failed tests, and the ordinary test
results. The attestation is then written to `final_attestation.json`, and its
digest is printed. It contains:

- `task`, `organization`, `team`, `agent` (with the models reported);
- `contract`, `checkpoints`, `policy_versions` (contract, autonomy policy,
  budget model, zone set, binding digest);
- `action_summary` (authorizations by decision, operation, and tool; executed
  actions; files changed; budget consumed);
- `denied_actions`;
- `approvals` (each `approved_and_executed` or `not_executed`);
- `violations`, `repairs`, `human_interventions`;
- `contract_tests`, `ordinary_tests`;
- `final_state` (lifecycle, autonomy, safety, budget);
- `final_decision` (`PASS`, `FAIL`, `CANCELLED`, or `INCOMPLETE`, with
  reasons);
- `evidence` (event count and chain verification);
- `attestation_digest`.

**Commands:**

| Command | Output |
| --- | --- |
| `crane session inspect ID [--json]` | identity, chain status, a timeline of evidence records, and the attestation |
| `crane session export ID --json` | `{export_format, session (the bound document), lifecycle, journal, chain, evidence, attestation}` |
| `crane session inspect --export FILE [--json]` | re-derives the evidence and attestation from the export's bound document and journal alone, and checks them against what it carries; non-zero exit when the chain is broken or anything differs |
| `crane session export ID --otlp` | OpenTelemetry OTLP/JSON (see below) |

The `--export` check proves that an export is sufficient to reconstruct the
session even after its directory is gone.

**OpenTelemetry.** The OTLP/JSON output has these properties:
- There is one trace per session, with a root `crane.session` span.
- There is one span per evidence record, named
  `crane.<category>.<event>`, with `crane.*` attributes.
- Denials and violations get error status.
- It is derived from the evidence records, which stay authoritative, and is
  meant for an OpenTelemetry collector's file receiver.

## Delivery

**Pipeline** (`crane deliver run SESSION_ID`, human or trusted process only):

1. **Reconciliation.** The session is finalized if it is not already, which
   runs the mandatory contract tests. Delivery requires the evidence
   attestation's final decision to be `PASS`; otherwise nothing is delivered.
2. **Branch.** An isolated session commits in its worktree on its own branch.
   Otherwise the changes move from the base branch onto
   `<branch_prefix><session>`, and only the files the session changed are
   committed. The base branch is the configured `base_branch`, or else the
   checked-out branch.
3. **Checks on the branch:**
   - the final contract tests (`contract_tests`, never exceptable);
   - the repository tests (`repository_tests`, from `.crane/testing.json`);
   - every configured check.

   Output is kept only as a digest.
4. **Pull request.** The pull request is opened or updated:
   - the `local` provider writes `pull_request.md` and numbers pull requests
     locally;
   - the `github` provider pushes the branch and uses `gh`.

   The body has the contract clauses, the contract tests, the checks, the
   attestation digest and counts, and the merge rule.
5. **Slack.** A Block Kit message goes to each `slack.notify` target via the
   outbox.
6. **Merge.** If the merge policy allows autonomous merge, the change merges
   right away.

The task moves from `PR_READY` to `REVIEW` when the pull request opens.

**Merge policy** (`merge_policy` in `.crane/delivery.json`). The first rule
that covers the change applies, otherwise the default. A rule covers a change
by:
- `autonomy`: the session's final autonomy modes;
- `min_criticality` / `max_criticality`: the highest zone criticality of the
  changed files; unzoned files count as routine.

A rule sets `approvals` (distinct people), `approvers` (empty means any mapped
person), and `auto_merge`. A rule that could merge critical or restricted
changes without approval is rejected when the configuration loads.

| Default rule | Covers | Needs |
| --- | --- | --- |
| `autonomous-routine` | autonomous sessions, routine changes | 0 approvals, merges automatically |
| `critical` | critical and restricted changes | 2 approvals, merged by a human |
| `default` | everything else | 1 approval, merged by a human |

**Eligibility.** A delivery may merge only when all of these hold:
- the attestation is `PASS` and the session ended `active`;
- the contract tests passed;
- every other check passed or has a valid exception;
- the branch has not moved since the checks ran;
- no rejection or change request exists for the current commit;
- the approvals of the current commit are in.

`crane deliver status` lists what is missing.

**Slack** (`slack` settings):
- `notify`: channels and people to notify.
- `users`: maps Slack user ids to approver names.
- `signing_secret_env`: names the environment variable holding the signing
  secret.
- `pr_url_template` and `contract_url_template`: link templates.

Clicks arrive at `POST /slack/actions` (`crane task serve`) or through
`crane deliver slack-action --body FILE --timestamp T --signature S`. Each
request must carry a valid `X-Slack-Signature`, an HMAC-SHA256 of
`v0:timestamp:body`, and be at most five minutes old. Each button carries the
delivery and commit it is about, so clicks on an older commit are refused, and
clicks by unmapped users are recorded and not counted.

| Button | What happens |
| --- | --- |
| View PR, View contract | links; the click is recorded |
| Approve, Reject, Request changes | the same rules as the CLI |
| Approve exception | shown only for failing checks listed in `exceptions.allowed_checks` |

**Exceptions** (`crane deliver exception`, or Slack). An exception is:
- **scoped** to one failing check listed in `exceptions.allowed_checks`;
- **resource-bound** to the current commit;
- **session-bound** to the delivery's session;
- **temporary**, lasting at most `exceptions.max_duration_seconds` (Slack uses
  `default_duration_seconds`);
- **approved** by an approver of the covering rule;
- **auditable**, recorded in the delivery journal (with its scope) and the
  session journal.

Exceptions, approvals, and rejections never touch policies, zones, or the
configuration.

**After a verified merge.** With the local provider, the merge is
`git merge --no-ff` of the branch into the base branch; with `github`, it is
`gh pr merge`. Crane then:
- checks that the merge commit contains the checked commit;
- records the merge commit;
- writes the trusted checkpoint `trusted_<session>` at the merge commit (the
  new trusted repository state);
- moves the task through `REVIEW` and `MERGED` to `COMPLETED`;
- only then queues the tracker completion in the outbox: for Jira, a comment
  and a transition (`trackers.jira.done_transition`); for Asana, a story and
  `completed: true`;
- records `delivery_merged` in the session journal and re-finalizes
  `final_attestation.json`, whose `delivery` section names the merge, the
  approvals, and the exceptions.

**Delivery journal.** `.crane/runtime/delivery/SESSION/journal.jsonl` is
append-only and hash-chained like session journals. It records:
- `submitted`, `pull_request`, `notified`;
- `approval`, `rejection`, `changes_requested`, `exception`;
- `unauthorized_action`, `viewed`;
- `merged`, `task_completed`, `attestation_finalized`, `blocked`.

The delivery state is replayed from it.

**Agents.** Every `crane deliver` command except `status` is denied to agent
shells and refused in agent environments. An attempt to run, approve, except,
or merge quarantines the session.

## Verified delivery pipeline

The pipeline runs from a verified session through these steps:

1. commit;
2. contract tests;
3. repository tests and checks;
4. attestation;
5. pull request;
6. Slack review;
7. approval;
8. merge;
9. trusted checkpoint.

**Verification.** `crane deliver run ID` refuses:

- a governed session that is not `DELIVERY_READY`;
- any session whose finalization did not reconcile to PASS;
- an already merged delivery.

The round records `verification`, either `{"by": "orchestrator", "phase"}` or
`{"by": "finalization"}`.

**Binding.** The delivery commit is made with trailers: `Crane-Session`,
`Crane-Task`, `Crane-Contract-Digest`, and `Crane-Attestation`. After the
checks, the round records:

- `head` and `tree` (`HEAD^{tree}`);
- `binding`: repository identity, owner, and name; task; contract id, digest,
  and version; checkpoint name and SHA; base; branch; head; tree;
  `checks_digest` (the check results and contract tests); attestation; zones;
  autonomy;
- `binding_digest`, its SHA-256.

The pull request body has a "Delivery binding" table, and sections for the
contract, affected zones, validation, attestation (with the autonomy mode),
exceptions (refreshed when one is granted), and the merge policy.

**Slack.** The announcement text names the repository, task, commit, contract
digest, attestation, and binding digest. Every button's value carries:

- `delivery`, `head`, and `binding`;
- `repository`, `task`, `contract`, and `attestation`;
- `check`, for an exception button.

Signature verification (`v0` HMAC-SHA256 with the signing secret, at most 5
minutes old) is unchanged.

**Decisions.** Approvals, rejections, and change requests are recorded with
the round's `binding` and `checks_digest`. A click about another commit or
binding is refused and recorded as `unauthorized_action`. The same person
making the same decision on the same round again changes nothing
(`already_recorded`). Decisions count only for the round whose binding they
name: a new commit or new check results need new approvals. With
`merge_policy.approval_ttl_seconds`, an approval counts only that long, and an
expired one can be given again.

**Eligibility.** The merge rule for the session's autonomy and highest zone
criticality applies: approvals from the rule's approvers, checks or valid
exceptions, contract tests, and no rejection. Merging also requires that:

- the binding still matches its digest, and the checked commit's tree still
  exists;
- the branch has not moved;
- the task contract is still current (not superseded or invalidated);
- a governed session is still `DELIVERY_READY`.

**Merge.** `crane deliver merge ID` goes through the provider:

| Provider | Merge |
|---|---|
| `local` | `git merge --no-ff` on the base, aborted on conflict |
| `github` | `pr merge --match-head-commit HEAD` with `github_cli`, then the base is fetched |

Every merge must contain the checked commit. A failure is recorded as
`merge_failed`: the delivery state is `MERGE_FAILED`, no checkpoint is
trusted, and the task and session are not completed. A merged delivery merged
again returns `already_merged`.

`crane deliver merged ID --sha SHA` records a merge reported by the host:

- the same merge again changes nothing;
- a different SHA for a merged delivery is refused;
- a merge of a delivery that was not eligible is recorded as `merge_failed`
  ("merged without eligibility") and is never completed.

**Completion.** The merge commit becomes the checkpoint `trusted_ID` and the
connected repository's `trusted_checkpoint`. The task is completed, and only
then its tracker issue. The governed session's record gains
`delivery: {state: COMPLETE, merge_sha}`, and the attestation is re-finalized.

**Delivery states** (`delivery_state` in `crane deliver status`):

| State | Meaning |
|---|---|
| `AWAITING_APPROVAL` | waiting for the merge policy |
| `APPROVED` | eligible |
| `REJECTED` | the current round was rejected |
| `MERGE_FAILED` | the last merge of the current round failed |
| `COMPLETE` | merged |
| `BLOCKED` | no submitted round |

## The golden path (crate::flow)

The golden path composes the existing subsystems into one lifecycle. It holds
no state of its own beyond an audit of what it observed. Every stage is
derived on each read from the subsystem that owns it:

- the connection (`repo`);
- discovery;
- zone review, and policy activation and proposals;
- hook validation;
- the task intake and task contracts;
- the session orchestrator;
- delivery and task completion.

**Setup stages** (scope `repository`), in order:

| Stage | When |
|---|---|
| `CONNECT_REPOSITORY` | not connected |
| `DISCOVERING` | never discovered |
| `REVIEW_REQUIRED` | no zone recommendations yet, or some wait for review |
| `POLICY_APPROVAL` | policy proposals pending, or no organization or repository policy active |
| `AGENT_READY` | no Claude Code or Codex hooks installed and valid |
| `TASK_READY` | setup complete |

**Task stages.** A task that has no contract yet waits at the setup stage.
After that, the first match wins:

| Stage | When |
|---|---|
| `COMPLETED` | completion event confirmed and every terminal condition holds (else `BLOCKED`) |
| `COMPLETION_RETRY_PENDING` | completion event awaiting a retry |
| `COMPLETING_TASK` | completion event queued, or merged with no event yet |
| `MERGE_FAILED` | delivery `MERGE_FAILED` |
| `DENIED` | delivery `REJECTED` |
| `MERGING` | delivery `APPROVED` |
| `REVIEW` | delivery `AWAITING_APPROVAL` |
| `DELIVERY_FAILED` | delivery `BLOCKED` |
| `DELIVERY_READY` | orchestrator `DELIVERY_READY` |
| `QUARANTINED` | orchestrator `QUARANTINED`, or `FAILED` with quarantined safety |
| `VERIFICATION_FAILED` | orchestrator `FAILED` |
| `VERIFYING` | orchestrator `STOPPING` or `RECONCILING` |
| `RUNNING` | orchestrator acting phases |
| `NEEDS_CLARIFICATION` | intake `NEEDS_CLARIFICATION` |
| `DENIED` | contract rejected |
| `BLOCKED` | intake blocked or failed, an invalidated contract, a verified session with nothing to deliver, or identifiers that do not agree |
| `TASK_READY` | plan, approve the contract, or start the agent |

Each stage view has `stage`, `step` (1 to 14), `failure`, `reason`, `next` (the
command), and `waiting_for` (human, agent, society, or nobody).

**Layers.** `crane flow status [TASK] [--json]` (`GET /api/flow`,
`GET /api/flow/TASK`) returns the top-level `stage` and `current` view, the
setup layers, and per task:

- `task`, `contract`, `agent`, `session`, `authority`;
- `verification`, `attestation`, `pull_request`, `approval`, `merge`,
  `task_completion`;
- `links` and `terminal`.

**Links** check that the identifiers agree:

| Link | Must be the same in |
|---|---|
| contract digest | contract, session binding, orchestrator binding, pull request binding, completion event |
| session | intake launch, orchestrator, delivery |
| task | intake, delivery, orchestrator, completion |
| merge commit | delivery, completion event, orchestrator delivery |
| attestation | pull request binding, completion event |

**Terminal conditions** of `COMPLETED`, all checked again on every read:

- the completion is confirmed;
- the merge is verified, is in the repository, and contains the checked
  commit;
- the merge commit is the trusted checkpoint's commit;
- a governed session recorded the delivery;
- an orchestrated task record completed;
- every link agrees.

**Audit.** A stage change of a scope (`repository` or a task id) is recorded
once:

- in `.crane/runtime/flow/state.json`;
- as a transition in `.crane/runtime/flow/audit.jsonl`, hash-chained, with the
  identifiers (contract digest, session, pull request, binding, merge, and
  completion event).

`regression` marks a move back along the happy path. An unchanged stage
writes nothing. Nothing is written before Crane is initialized or on behalf of
an agent. `crane flow audit [SCOPE]` and `GET /api/flow/audit` show the log.
Since every stage is derived from durable subsystem records, a restart (or a
lost `state.json`) loses nothing.

**Advance.** `crane flow advance TASK [--now]` (`POST /api/flow/TASK/advance`)
first reconciles task completions (restart recovery), then repeats, until a
human, the agent, or a backoff is next:

| Stage | Step |
|---|---|
| `DELIVERY_READY` | `deliver run` |
| `MERGING` | `deliver merge` (the merge policy already holds, so the approvals were given) |
| `COMPLETING_TASK`, `COMPLETION_RETRY_PENDING` | completion dispatch |

Each step is the owning subsystem's own idempotent operation, so advancing
again changes nothing. Advancing refuses agent environments, and an agent
shell running it is quarantined.

## Task completion

**Invariant.** A task is completed only after the delivery pipeline verified
that its governed change merged (`crane deliver merge`, or a verified
`crane deliver merged` report). The intake shows the lifecycle:

| State | When |
|---|---|
| `RUNNING` | the agent works |
| `VERIFIED` | verified, with no changes |
| `DELIVERY_PENDING` | verified, not delivered yet |
| `PR_REVIEW` | a pull request is open |
| `MERGED` | merged, with no completion event yet (`reconcile` queues it) |
| `TASK_COMPLETION_PENDING` | the completion event is queued or being sent |
| `COMPLETION_RETRY_PENDING` | the tracker was unreachable; the event is retried |
| `COMPLETED` | Jira confirmed it, or the issue was already done |

A finalized session, passing tests, a pull request, or an approval never makes
a task `COMPLETED`. The orchestrated task record stays `MERGED` until the
completion is confirmed, and only then moves to `COMPLETED`, retiring its
contract.

**Completion event.** `.crane/runtime/completions/ID.json`, where `ID` is
`completion-` followed by 20 hex digits of
`sha256(repository identity, task, merge SHA)`. The same merge always gives
the same event. It holds:

- the task id, the tracker and its issue key;
- the repository identity, owner, and name;
- the merge SHA and the delivery;
- the attestation the merge relied on and the contract digest;
- `status`, `attempts`, `next_attempt_at`, `last_error`, and `needs_attention`;
- each step's result: `checked`, `comment`, `transition`, `verified`;
- the outbox entries holding its requests;
- its history.

The Jira comment and transition are written to the delivery outbox (channel
`jira`, each tagged with `completion_event` and `step`). Queuing the same merge
again returns the existing event (`duplicate: true`).

**Dispatcher** (`crane task completions send [--now] [--task ID]`):

- It handles the due Jira events, one dispatcher at a time; a lock left for 5
  minutes by a dead process is replaced.
- Each attempt runs the steps that are not done yet, saving each result at
  once:
  1. `GET` the issue status. An issue already done before Crane transitioned
     it is `completed_externally`: no comment, no transition.
  2. Post the comment, unless a comment carrying `[crane-completion:ID]`
     already exists.
  3. `POST` the transition (`trackers.jira.done_transition`).
  4. `GET` the status again; it must be done.
- A confirmed event completes the task and is journaled as `task_completed` in
  the delivery.
- A failure counts the attempt, sets `retry_pending`, and schedules the next
  attempt after 60 seconds, doubling each time up to an hour. Unreachable
  Jira, 429, and 5xx are plain retries; another 4xx also sets
  `needs_attention`.
- `--now` ignores the backoff. Nothing reruns the engineering session.

**Transport.** `trackers.jira.transport` is a command. It receives
`{method, url, path, body}` on stdin (the URL built from
`trackers.jira.base_url`) and answers `{status, body}` on stdout; a non-zero
exit means Jira could not be reached. `examples/jira_transport.py` implements
it for Jira Cloud (`JIRA_EMAIL`, `JIRA_API_TOKEN`).

**Reconciliation** (`crane task completions reconcile [--send]`, safe to run
at any time, for example after a restart):

- every merged delivery of a task gets its event (queued again under the same
  id if the process stopped before queuing it);
- an event left `sending` by a process that died becomes `retry_pending`, and
  its recorded steps are not repeated;
- a confirmed event whose task did not complete completes it;
- with `--send`, the due events are sent.

A Jira webhook closing the issue of a `MERGED` task marks its pending event
`completed_externally`. `send` and `reconcile` refuse agent environments, and
an agent shell running them is denied.

## Repository connection

`.crane/connection.json` (`connection_format` 2) is the one record of the repository Crane
works on. Format 1 records written by the first control plane are read and upgraded.

**Connect.** `crane repo connect` (alias `crane connect`), with options `--provider github|git|local`,
`--checkpoint NAME` (default `baseline`), `--default-branch NAME`, `--refresh` and `--json`. It runs
from the repository's top level, wherever it is started.

It refuses:
- a directory that is not a Git repository (`invalid repository`);
- a repository without commits (`missing Git metadata`);
- a Crane session worktree;
- an agent environment;
- a `.crane` connected to another repository. Release that one first with
  `crane repo disconnect --forget`.

Then it:
1. creates Crane's directories (`init`);
2. creates the trusted checkpoint at HEAD if it does not exist (never moving an existing one);
3. runs discovery;
4. writes the record atomically.

Connecting an already connected repository writes nothing. A disconnected repository is
reconnected with its history kept.

**Record:**

| Field | Content |
| --- | --- |
| `repository_id` | digest of the root commits, the same identity sessions bind to |
| `provider`, `host`, `owner`, `name`, `full_name` | parsed from `origin` |
| `remote` | the remote, with credentials removed |
| `web_url`, `pull_request_url` | from the provider |
| `default_branch` | given, else `origin/HEAD`, else the checked-out branch |
| `local_path`, `connection_status` | where it is, and `connected` or `disconnected` |
| `trusted_checkpoint` | its name |
| `last_discovery` | head, files, symbols, languages, whether any language is enforceable |
| `policy_version`, `zone_version` | at connect time |
| timestamps, `history` | connected, refreshed, disconnected, reconnected, checkpoint created; each with time and actor |

**Providers.** Providers implement one adapter (`RepositoryProvider`): `recognizes`, `web_url`,
`pull_request_url`, and `capabilities`. Capabilities are local only; for GitHub that means whether
the `gh` CLI is installed.

| Provider | Recognizes |
| --- | --- |
| `github` | `github.com` and `github.*` (or `--provider github` for other hosts) |
| `git` | any other host |
| `local` | no remote, or a local path |

No provider calls the network. Outbound work stays with the outbox and its forwarder.

**Status.** `crane repo status [--json]` reports the trusted checkpoint against HEAD:

| Checkpoint status | Meaning |
| --- | --- |
| `current` | HEAD is the checkpoint |
| `stale` | HEAD is past it; `commits_since` gives how far |
| `diverged` | HEAD no longer contains it |
| `missing` | its commit is gone |
| `invalid` | not a full SHA, or a name mismatch |
| `absent` | the checkpoint file does not exist |

It also reports:
- discovery: `fresh`, `stale`, or `never` (`crane discover` keeps it fresh);
- whether policies or zones changed since connecting.

`crane repo inspect` adds:
- branches (delivery branches marked) and worktrees (session worktrees marked);
- every checkpoint;
- provider capabilities and configured files;
- installed agent hooks;
- readiness, with what is missing;
- the history.

**Disconnect.** `crane repo disconnect [--reason TEXT] [--forget]` sets `disconnected`. The
control plane answers 409 until the repository is reconnected; nothing else is deleted.

**Reuse.** Consumers read the record and fall back to the `origin` remote when not connected:
- the dashboard (`/api/connection`, `/api/repo`, and the header);
- task planning (a task's `repositories` match `name` or `owner/name`);
- evidence (the organization defaults to the owner);
- delivery (the base branch defaults to `default_branch`).

## Zone recommendations and review

**Discovery result.** `crane zones recommend [--by NAME]` (or `POST /api/zones/recommendations`)
runs discovery, resolves the active zones, and records a discovery result in
`.crane/zone-proposals/discovery.json` (the latest run and the list of runs). A result holds:
- the run id, time, and HEAD;
- the recommender version (`zone-recommender@1`);
- inventory size and signal counts;
- the task history considered;
- the recommendations produced and what changed.

**Recommendations.** Each recommendation is `.crane/zone-proposals/ID.json` (`review_format` 1):

| Field | Content |
| --- | --- |
| `id`, `zone_id`, `status`, `active` | identity and lifecycle |
| `revision`, `digest`, `zone_text` | the exact `.zone` file it would write, and its SHA-256 |
| `recommendation.selectors` | semantic `module` selectors; `folder`/`path` for regions; `tests` |
| `recommendation.affected` | files and symbols, as resolved by the zone engine |
| `criticality`, `autonomy`, `safety_state` | suggested values (`safety_state` is `active`) |
| `rationale`, `signals`, `confidence` | why, the evidence, and how sure |
| `sources` | `inventory:risk`, `proposals:engine`, `pack:payments@1`, `pack:testing@1`, `task_history` |
| `history`, `activation`, `rejection` | what happened and when |

Recommendations live outside `.crane/zones`, so they govern nothing.

**Signals and categories.** A module is recommended under its strongest category:

| Category | From | Criticality / autonomy |
| --- | --- | --- |
| `security` | auth or secrets vocabulary | restricted / observe |
| `payments` | payment vocabulary or the Payments pack | critical / assisted |
| `data_access` | persistence calls | sensitive / delegated |
| `api` | entry points | sensitive / delegated |
| `integrations` | network calls | sensitive / delegated |
| `shared_core` | high fan-in or cross-module callers | sensitive / delegated |

Protected regions from the proposal engine (migrations, infrastructure, production configuration,
secret configuration) each get a recommendation: critical/assisted, or restricted/observe for
secrets. Test code gets `tests` (routine/delegated).

**Confidence.** It is `high` with two kinds of evidence including a strong one (a domain name, a
pack, persistence, or an entry point), `medium` with one strong kind or three or more symbols, and
otherwise `low`. Sessions that changed the code (and violations they caused) add to the rationale
and raise medium to high. A recommendation whose files are already governed at the same or a
higher criticality is reported `covered` and not proposed.

**Lifecycle:**
- A discovery run is *discovered*.
- Each recommendation starts `proposed`.
- `in_review` is set with `crane zones review ID --claim`.
- A human then sets `approved` (ACTIVE) or `rejected`.
- `withdrawn` means the evidence disappeared while it was pending.

**Versioning.** Rediscovery keeps unchanged recommendations as they are. A change makes a new
`revision`:
- an approved zone keeps governing until the new revision is approved (`revision_proposed`);
- a rejected one is reopened only when it changed (`reopened`).

Zone names written by hand are never reused. A revision that moves to a new name retires the file
of its earlier approval.

**Approval:**

```
crane zones approve ID --approver NAME --confirm DIGEST_PREFIX
```

The digest prefix is at least 12 characters. Approval:
- refuses an agent environment;
- validates the zone;
- never overwrites a hand-written zone file;
- writes `.crane/zones/ZONE.zone` atomically, rolling back if the zone set would not load;
- records the activation (approver, revision, zone set version before and after).

Approving the same revision again returns `already_approved` and writes nothing.
`crane zones reject ID --approver NAME [--reason]` is the counterpart; rejecting again returns
`already_rejected`.

**Audit.** Each decision is appended to `.crane/zone-proposals/audit.jsonl`, hash-chained like
session journals (`crane zones audit`, `GET /api/zones/audit`).

**Protection.** `crane zones approve|reject|review --claim` refuse agent environments. Agent shells
running them are denied, and an attempt to approve or reject is self-escalation, which quarantines
the session. Agents can never write `.crane/zones`. Agents may run `zones recommend`; the result
is labelled with the agent environment.

**Dashboard.** The API mirrors the CLI:
- `GET /api/zones/recommendations` and `POST /api/zones/recommendations`;
- `GET /api/zones/recommendations/ID`;
- `POST /api/zones/recommendations/ID/review|approve|reject`;
- `GET /api/zones/audit`.

The Zones screen shows the recommendations with their rationale, signals, affected files, and the
zone file each would write, next to the active zones.

**Effect.** An approved zone is an ordinary zone. Sessions started afterwards bind it and the
authority engine enforces it; sessions already running keep the zones they were started with.

## Control plane and policy packs

**Connection.** `crane connect` (or `POST /api/connection`) writes
`.crane/connection.json` once. It records:
- the repository identity (a digest of the root commits);
- the root, the `origin` remote, and the default branch;
- the HEAD, languages, files, and symbols;
- who connected and when.

Connecting again returns the same connection; `--refresh` updates the metadata
but keeps who connected and when. A connection made for another repository is
refused, and agents cannot connect. Until the repository is connected, every
screen answers `409`.

**API.** `crane dashboard` serves the page on `127.0.0.1:8790` (`--addr` to
change it) and `/api/*`. API calls need the `X-Crane-Token` printed at start
(or set `CRANE_DASHBOARD_TOKEN`); the page itself carries no data.
`crane dashboard api METHOD PATH [--body JSON]` runs the same route from the
command line, which is how the dashboard and the CLI stay identical.

| Endpoint | Screen |
| --- | --- |
| `GET /api/repository` | Repository: inventory summary and targets, critical candidates and critical/restricted zones, `policy_coverage` (as `crane discover --json` reports it), unresolved targets |
| `GET /api/zones` | Zones: exactly `crane zones --json` |
| `GET /api/contracts`, `GET /api/contracts/NAME` | Contracts: active policies (AgentScript and editor draft); proposals; per contract the `versions` (generated, regenerated, edited revisions with digests), `approvals` (approved, rejected, superseded, retired), and the Git history of the active file |
| `POST /api/contracts/preview` | the AgentScript a draft generates, or every problem in it |
| `POST /api/contracts/parse` | the draft for AgentScript (Advanced to visual) |
| `POST /api/contracts` | `{draft}` or `{agentscript}`: a new pending proposal (`origin.editor` is `visual` or `advanced`) |
| `POST /api/contracts/NAME/edit`, `/approve`, `/reject` | the proposal workflow; approval needs the approver and the digest confirmation |
| `GET /api/sessions`, `GET /api/sessions/ID` | Agent Sessions: autonomy, safety, budget, actions, denials, violations, tests, outcome; the detail adds autonomy, budget, and evidence records |
| `POST /api/simulate` | Policy Simulator: `{draft}`, `{agentscript}`, or `{proposal}` |
| `GET /api/attestations`, `GET /api/attestations/ID` | Attestations: the stored final attestation, whether it matches the evidence, the chain, the evidence records, and the delivery journal |
| `GET /api/packs`, `GET /api/packs/NAME`, `POST /api/packs/payments/propose` | policy packs |

`POST` calls are refused in an agent environment and denied to agent shells.

**Visual editor.** A draft is
`{"name", "checkpoint", "rules": [{"rule", "kind", "target", "scope", "change_type"}]}`.
It generates canonical AgentScript, one line per rule with block scope left
implicit, which is validated by the same parser that compiles active policies.
The Advanced editor edits AgentScript directly and converts back.

**Simulator (shadow mode).** The simulator replays every session journal's
mutating actions: the symbols and files from authorizations, and the effects
of executions.

For a preserve rule, each action that touches what the rule covers is judged:

| Scope | What counts as touching |
| --- | --- |
| block, flow | the target symbol (flow is approximated by the target) |
| file | the target or its file |
| folder | the target or its folder |
| all | anything |

| Shadow decision | When |
| --- | --- |
| `would_deny` | an authorization the rule would have denied |
| `would_flag` | an execution with no authorization, such as a shell command |
| `already_denied` | the action was already denied; nothing changes |

For a target rule, the simulator lists the sessions that never changed the
target.

Each rule reports how it resolves in today's repository. Each newly blocked
action is classified:

| Classification | When |
| --- | --- |
| `likely_true_positive` | the action caused a violation, or its session failed, was quarantined, or was rejected or sent back in review |
| `likely_false_positive` | its session passed or merged |
| `undetermined` | its session is still open |

The analysis gives the counts and the false-positive rate. Nothing is
enforced.

**Policy packs.** Only two exist, and neither activates anything:

- **`payments@1`** recommends `preserve` rules for non-test functions in
  payment code, in four categories:
  - `payment_processing`
  - `refunds`
  - `transaction_logic`
  - `payment_state_transitions`

  Whole types are recommended only for payment state. Symbols already covered
  by a contract are marked `covered`. It also suggests a critical zone for
  each payment module, and `proposed_policy` holds the uncovered rules.
  `crane packs propose payments` makes them a pending proposal (origin
  `{"kind": "pack"}`) to review and approve like any other.
- **`testing@1`** reports test directories (with languages, frameworks, and
  owners), test ownership gaps (with a CODEOWNERS suggestion), and contract-test
  configuration per framework: `configured` or `new`, with the
  `.crane/testing.json` entry. `suggested_testing_json` holds all of it;
  nothing is written.

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

## Session orchestrator: the governed lifecycle

`crate::session_orchestrator` owns the lifecycle of a governed session. It
coordinates the existing components and adds none of their decisions:

- the task intake launches the session;
- the session manager and authority engine decide actions;
- effect verification observes changes;
- the autonomy state machine and the budget keep their state;
- finalization reconciles and runs the contract tests.

**State machine.**

| From | Allowed next phases |
|---|---|
| `TASK_READY` | `SESSION_CREATED`, `FAILED` |
| `SESSION_CREATED` | `AGENT_CONNECTED`, `STOPPING`, `FAILED` |
| `AGENT_CONNECTED` | `RUNNING`, `DEGRADED`, `QUARANTINED`, `STOPPING`, `FAILED` |
| `RUNNING` | `DEGRADED`, `QUARANTINED`, `STOPPING`, `FAILED` |
| `DEGRADED` | `RUNNING`, `QUARANTINED`, `STOPPING`, `FAILED` |
| `QUARANTINED` | `RUNNING`, `DEGRADED`, `STOPPING`, `FAILED` |
| `STOPPING` | `RECONCILING`, `FAILED` |
| `RECONCILING` | `VERIFIED`, `FAILED` |
| `VERIFIED` | `DELIVERY_READY` |
| `DELIVERY_READY`, `FAILED` | none (terminal) |

The structure follows from the table:

- Execution is frozen from `STOPPING` on: mutating actions are denied, and no
  acting phase is reachable again.
- `VERIFIED` is entered only from `RECONCILING`.
- `DELIVERY_READY` is entered only from `VERIFIED`, when the session changed
  files.
- `FAILED` reaches nothing.

**Record.** `.crane/runtime/orchestrator/SESSION.json` holds the phase, the
history, action counts, the termination, and the lifecycle binding with its
SHA-256. The binding covers:

- the repository (identity, owner, name, root) and the task;
- the contract id and version, and the contract digest;
- the policy version and zone version;
- the checkpoint (name, SHA);
- the agent;
- the autonomy mode and the initial safety state;
- the budget (actions, files, risk model, risk budget);
- the organization configuration;
- the session's own binding digest.

**Per action** (`decide`, `observe`; also used by the hook pipeline, so
Claude Code and Codex hooks go through it):

1. receive the normalized `AgentAction`;
2. refuse it when execution is frozen;
3. verify the binding (a mismatch denies the action and fails the session);
4. `agent_session::authorize`;
5. answer `ALLOW`, `DENY`, `APPROVAL`, or `QUARANTINE` (denied with quarantined
   safety; evidence is journaled by the session manager);
6. after execution, `observe` verifies what actually changed incrementally,
   journals it with its budget consumption, and updates autonomy, safety, and
   budget;
7. the phase follows safety: `DEGRADED`, `QUARANTINED`, and back to `RUNNING`
   after a human recovery.

**Drivers** of `crane session run TASK --agent A`:

| Driver | Behavior |
|---|---|
| `--actions FILE` | actions in the neutral format (`{"tool", "operation", "path", "content"\|"edits"\|"delete", "command"}`, a JSON array or JSONL) are decided and, when permitted, executed by Crane: writes as proposed, commands in the session root; `APPROVAL` runs only with `--approve` (the human running the command, recorded as an approved tool call); `{"operation": "claim"\|"stop", "text"}` records agent claims as untrusted; the run stops at quarantine |
| `-- AGENT_COMMAND...` | the agent process starts in the session root with `CRANE_SESSION` and `CRANE_TASK_ID`; its hooks bring each action through the orchestrator |
| `--detach` | the session is prepared (`SESSION_CREATED`); the human starts the agent and runs `crane session finish ID` |

**Termination** (`crane session finish ID`, or the end of a run):

1. `STOPPING`, which freezes execution;
2. `RECONCILING`: binding check, session finalization (repository
   reconciliation, affected tests, mandatory contract tests, attestation and
   final attestation), and the repository tests of the testing policy
   (`.crane/testing.json` `session_tests`: `affected` by default, `all` to also
   run every organizational and agent-authored test, or `none`).

`VERIFIED` needs all of:

- reconciliation PASS;
- no failed contract test;
- repository tests not failed;
- a final attestation of PASS;
- safety not quarantined;
- the binding intact.

`DELIVERY_READY` also needs changed files. Anything else is `FAILED`, with the
failed checks listed. Finishing a terminated session returns it unchanged
(`already_terminated`), and a `FAILED` session stays `FAILED`. Running the
task again creates a new session (`...-rN`) with its own lifecycle.

**Delivery.** `crane deliver run` refuses a governed session unless it is
`DELIVERY_READY`.

**Invariants.** Each is enforced by an existing component:

| Invariant | Enforced by |
|---|---|
| agents cannot change session authority, policy, or the checkpoint | the metadata guard and the mutating-command guard |
| agents cannot raise their own budget or approve their own exception | refused and quarantined as self-escalation |
| `session run` and `session finish` are a human's | they refuse agent environments, and an agent shell running them is quarantined |

The verdict is based on the repository, never on agent claims.

**API.**

- `POST /api/sessions/run` with `{task, agent, actions?, approve?, autonomy?}`;
- `GET /api/sessions/ID/lifecycle`;
- `POST /api/sessions/ID/finish`.

## Provider attachment (Claude Code, Codex)

The lifecycle runs through these steps:

1. Approved task.
2. Contract session.
3. The agent starts.
4. Pre-action authorization.
5. The action.
6. Post-action verification.
7. Continuation.
8. Final reconciliation.

Providers attach to it through `crane agent hook`.

**Adapters** translate. `crate::adapter` has one adapter per provider, and
each:

- turns its payload into a `ProviderEvent`: session id, the action as an
  `AgentAction`, stop flag, model, source, `hook_event_name`, `cwd`, and
  identity facts (`transcript_path`, `permission_mode`, `turn_id`,
  `tool_use_id`);
- renders Crane's verdicts and reports in the provider's protocol, including
  `respond_failure` when no decision exists;
- describes the provider's hook configuration (file, events, matchers,
  permission rules, timeout).

Adapters never decide whether an action is allowed, whether the budget
suffices, whether a zone permits it, or whether a policy is violated.

**Binding** (`agent_session::bind`, the same for every provider and the same
for the same inputs):

| Order | Source | Condition |
|---|---|---|
| 1 | `CRANE_SESSION` (or `--crane-session`) | must exist, belong to this provider, match `--task`/`CRANE_TASK_ID` if given, and not be cancelled or finalized |
| 2 | `CRANE_TASK_ID` | the provider's resumable session bound to the task's current approved contract (the launched one; one named by the provider's id first, then the newest contract version) |
| 3 | the provider's session id | the session `AGENT-ID`, created on first sight |
| 4 | none | a transient session |

A provider session bound through 1 or 2 is journaled once as
`provider_attached`, with the provider, its session id, how it was bound,
model, cwd, and identity facts.

**Checks before the authority engine.** The payload's `hook_event_name` must
be the event the hook was registered for. A reported `cwd` must lie inside the
session's repository or worktree, which binds the agent to the session's
checkpointed code.

**Failure handling.** When no Society decision or verification can be
obtained, the adapter answers with `respond_failure`, never an allow or a
clean result. Causes include:

- no session can be bound;
- a binding check fails;
- the input is unreadable or invalid JSON on a tool event;
- an internal error or panic.

| Event | Claude Code | Codex |
|---|---|---|
| `PreToolUse` | exit code 2 | exit code 2 |
| `PermissionRequest` | exit code 2 | `decision.behavior: "deny"` |
| `PostToolUse` | exit code 2: "not known to be compliant" | `decision: "block"` |
| `Stop` | held once (exit code 2), then let through so the agent cannot loop | held once (`decision: "block"`), then let through |
| `SessionStart` | exit code 1 | exit code 1 |
| other events | reported on stderr | reported in a `systemMessage` |

Hooks are installed with a 120-second timeout, because providers treat a
timed-out hook as a non-blocking error.

**Hook configuration.**

| Command | Effect |
|---|---|
| `crane agent install --profile claude\|codex` | merges Crane's handler groups (tool events with matcher `*`) and Claude Code's deny rules into `.claude/settings.local.json` or `.codex/hooks.json`; additive, idempotent, never rewrites an unchanged file; Codex hooks registered inline in `.codex/config.toml` count |
| `crane agent hooks --profile …` | validates: the file parses; each event runs Crane exactly once; tool events match every tool; the permission rules are present; `crane` is on PATH; Crane is initialized; exits 1 when not valid |
| `crane agent uninstall --profile …` | removes only Crane's handlers and rules, deleting a file that held nothing else; refused on behalf of an agent, and an agent trying it is quarantined |
| `crane agent status [--profile] [--session ID \| --task ID]` | hook validation, and for each session: attachments, task contract (and whether it is still current), checkpoints, repository, lifecycle, autonomy, safety, budget, last event |

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
