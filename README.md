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

## Quick start

Build:

```bash
cargo build --release
```

Create a test repository:

```bash
mkdir demo
cd demo
git init
git config user.email "crane@example.com"
git config user.name "Crane Demo"
```

Create `GatewayService.java`:

```java
class GatewayService {
    public void call() {
        System.out.println("payment");
    }
}
```

Then:

```bash
git add GatewayService.java
git commit -m "trusted baseline"

crane init
crane checkpoint --name baseline
crane protect --function GatewayService.call --policy payment_gateway
crane check
```

Expected:

```text
PASS payment_gateway: preserve --function GatewayService.call
Crane check: PASS
```

Now modify the function and run `crane check` again. It will report the policy violation.

For machine consumption:

```bash
crane check --json
```

For a compact LLM-facing context:

```bash
crane context
```

## Installation

Prebuilt binaries for Linux, macOS (Intel and Apple Silicon), and Windows are
published on the [GitHub Releases page](https://github.com/VishuKalier2003/Agentscript/releases).
Download the archive for your platform, extract it, and put the `crane` binary
(`crane.exe` on Windows) somewhere on your `PATH`.

Verify the installation:

```bash
crane --version
```

Rust users can install directly from the repository:

```bash
cargo install --git https://github.com/VishuKalier2003/Agentscript.git --bin crane
```

## Publishing a release

Create and push a semantic-version tag after merging the desired changes:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The [release workflow](./.github/workflows/release.yml) builds archives for
Linux, macOS Intel, macOS Apple Silicon, and Windows, publishes SHA-256
checksums, and creates a GitHub Release with generated release notes. The
workflow can also be started manually from GitHub Actions by supplying an
existing tag.

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
- The check is exact source comparison of the extracted tree-sitter function
  region.
- Git commit SHA is the immutable baseline; branch is recorded only as metadata.
- No agent-specific hooks are installed yet.
- `deny-read`, `deny-write`, `require-read`, `require-write`, and `static-database` are intentionally not implemented yet.
