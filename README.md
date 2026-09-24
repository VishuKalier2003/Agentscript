# Crane MVP

Crane is a deliberately small, modular contract language for protecting trusted code from unintended AI-agent changes.

This MVP implements one policy primitive:

- `preserve --function`

It also implements the Git-backed workflow discussed during design:

- `crane init`
- `crane checkpoint`
- `crane protect --function ...`
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
    checkpoint baseline
    preserve --function GatewayService.call
}
```

The verifier compares the protected function in the checkpoint commit with the current working tree. The model is never the authority that decides whether the contract passed.

## Trust model

The developer creates a reviewed Git commit and explicitly records it as a
checkpoint. Crane then independently compares protected source nodes in that
commit and the current worktree. An agent may edit files, but it cannot create
or move a checkpoint through `crane check`, and it cannot turn an uncertain
resolution into a pass.

`preserve` guarantees equality of the protected function's canonical parsed
token stream: formatting and comments may change, while code tokens,
identifiers, literals, operators, modifiers, annotations, and structure remain
protected. It does not guarantee semantic equivalence, runtime behavior,
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

- `SessionStart` prints the deterministic policy context to Claude.
- `UserPromptSubmit` verifies the repository whenever the user submits a
  prompt, before Claude performs new edits.
- `PostToolUse` verifies after file-editing tools and returns JSON violations.
- `Stop` verifies again before Claude finishes and reports any final failure.

The hooks invoke `crane` from `PATH`, so install a pinned Crane release before
starting Claude Code. On Windows, restart Claude Code after adding the Crane
directory to `PATH`. Claude Code provides the host lifecycle; Crane remains
the independent verifier. Hook output and its non-zero exit status are passed
back to Claude Code, which can repair the reported violation and retry without
the user repeating a Crane instruction.

A blocked prompt/edit/stop hook exits with status `2`, the Claude Code hook
convention for feedback that must be shown to the agent; a passing hook exits
`0`.

Crane does not overwrite an existing `.claude/settings.local.json`; merge the
generated hook entries deliberately if that file already exists. The hooks
never create or move checkpoints, modify policies, or edit source code, and
they work for every language supported by the verifier.

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
      "message": "Protected function was modified."
    }
  ]
}
```

`status` is `passed` or `failed`; `violations` is always present and is
deterministically ordered by policy and rule. A failed resolution, missing
checkpoint, missing commit, or missing protected function is also reported as
a violation, so verification fails closed.

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
file extension and compares the complete syntax node for the function, so
formatting and comments inside the protected node remain protected as well.

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
- The check compares canonical parsed tokens from the extracted tree-sitter
  function region; formatting and comments are ignored.
- Duplicate target matches fail closed.
- Git commit SHA is the immutable baseline; branch is recorded only as metadata.
- Claude Code project hooks are supported through `crane agent install`; other
  agent hosts still require their own hook, MCP, or wrapper integration.
- `deny-read`, `deny-write`, `require-read`, `require-write`, and `static-database` are intentionally not implemented yet.
