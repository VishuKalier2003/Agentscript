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
The covered code is the rule's item plus its scope (the file, the folder, the
flow, or the whole repository).

For each policy, the IR records the policy id, its version (the SHA-256 of the
policy file), the checkpoint name and commit, and its clauses. The contract
version is a SHA-256 over every policy version and checkpoint commit, so editing
any policy or re-baselining any checkpoint produces a new version. A malformed
policy stays in the IR, marked malformed, so that it fails closed.

A contract session (`.crane/runtime/sessions/<agent>-<session id>/`) binds one
agent session to one contract version. It is created on the first hook event
that carries a provider session id, and it is never replaced or refreshed:

- `session.json` holds the session id, the agent, the provider session id, the
  creation time and optional expiry, the repository root, the bound IR, and the
  runtime grants derived from the checkpoint (where each item lives, and each
  flow's members). It is written once.
- `state` is `active` or `closed`. It is the only thing that changes.
- `journal.jsonl` has one line per event: sequence, time, session, agent,
  contract version, checkpoints, tool, normalized operation and resources,
  decision, reasons, result, and a SHA-256 of the tool arguments. The
  arguments themselves are never stored.
- `attestation.json` holds the latest reconciled outcome.

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
   - While the contract is incomplete (a malformed policy, a missing
     checkpoint, a missing or ambiguous item), or the session has expired or
     is closed, every mutating action is denied.
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
     the bound version;
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

- the session and agent ids;
- the bound, verification, and repository contract versions;
- each contract's id, version, checkpoint, and commit;
- the authorized and denied actions;
- a result for each `preserve` and `target` clause, plus any other findings;
- answers to the reconciliation questions: `forbidden_mutations_attempted`,
  `required_targets_changed`, `preserve_invariants_satisfied`,
  `change_types_satisfied`, `same_contract_version`, `fully_reconciled`;
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
