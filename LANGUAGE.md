# AgentScript v0.1

AgentScript is the policy language consumed by Crane. Version 0.1 has one
contract primitive: `preserve --function`. It is deliberately non-recursive:
there are no imports, inheritance, policy dependencies, macros, composition,
databases, or other policy primitives.

## Policy syntax

```text
policy payment_service {
    checkpoint baseline
    preserve --function PaymentService.charge
}
```

The grammar is:

```text
program := policy
policy := "policy" identifier "{" statement* "}"
statement := checkpoint_statement | preserve_statement
checkpoint_statement := "checkpoint" identifier
preserve_statement := "preserve" "--function" qualified_name
qualified_name := identifier (("." | "::") identifier)*
```

Identifiers contain ASCII letters, digits, `_`, or `-`. A policy contains one
explicit checkpoint and at least one preserve rule. Policy files are stored as
`.crane` files under `.crane/policies/`.

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

The comparison is a canonical token stream derived from the parsed source
node. Whitespace and comment nodes are ignored; identifiers, literals,
operators, punctuation, modifiers, annotations, and the remaining syntax are
retained in source order. Thus formatting-only and comment-only changes pass,
while code-token changes fail. This is not semantic equivalence: different
token streams that happen to behave similarly are still different.

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
- invalid `.crane/config.toml`: FAIL with `verification_error` for the
  configuration-level verification result

Formatting-only changes and comment-only changes pass because v0.1 compares
canonical code tokens. Changes to unrelated functions or unrelated files do
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
      "message": "Protected function was modified."
    }
  ]
}
```

`violations` is empty on success. On failure it is deterministically ordered
by policy path/name and rule order. Every violation contains all six stable
fields; setup-level failures use `policy_id` `crane` and empty target and
checkpoint fields.

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
