# Crane

Crane enforces **AgentScript** contracts on AI coding agents. You mark lines of code an agent must
**preserve** or must **change** (target); Crane checks every agent action at the hook boundary,
verifies what actually changed on disk, and records tamper-evident security evidence that the
**Foxx** dashboard turns into metrics, action graphs, and reports.

- Works with any language that has comments: selections are marked with anchor comments such as
  `# @crane:selection:K7M2P9RX:start` that move with the code.
- A signed registry (Ed25519, SHA-512) is the authority, so forged, copied, or edited markers and
  registry files are detected.
- Claude Code and Codex hooks deny edits to protected code, `.crane/`, and the markers before they
  run, and detect shell bypasses after they run.
- Hash-chained events, an autonomy-credit ledger, and metrics that say how each value is known.

## Install

```bash
cargo install --path .        # requires Rust 1.85+ and Git
crane --version
```

`gh` is needed for `crane github`; `curl` for Slack, Jira, and WhatsApp delivery.

## Quick start

```bash
crane repo --https https://github.com/OWNER/REPO    # connect the clone to its GitHub repository
crane init                                          # create .crane/ and a signing key
crane checkpoint baseline                           # trusted baseline = current HEAD

crane protect app/payments.py start-line 12 end-line 30
crane target  app/refunds.py  start-line 5 end-line 18 change_type logical_bn

crane validate          # integrity, syntax, and markers
crane test .            # run every preserve/target command
crane agent install --profile claude
crane dashboard
```

Commit `.crane/` with your code. Policies live in `.crane/policies/*.crane`:

```text
policy payments {
    preserve K7M2P9RX;
    target 4QZ8M2LD change_type logical_bn;
}
```

A policy passes only if every command passes. `change_type` is one of `logical_bn` (business
logic), `logical_cn` (loops or branches), `logical_sn` (structure), or `semantic` (names or
comments); whitespace-only changes never satisfy a target.

## Commands

| Area | Commands |
| --- | --- |
| Basic | `--version`, `init`, `checkpoint [NAME]`, `protect FILE ...`, `target FILE ...`, `--set default policy\|checkpoint NAME`, `parse file FILE`, `validate`, `test .`, `add policy-context file FILE [policy NAME]`, `policy-context NAME --view` |
| Repository | `repo --ssh\|--https\|--gh-cli REMOTE [privileges]`, `repo status`, `repo --view`, `repo del`, `github`, `github days DAYS` |
| Policies | `create policy NAME FILENAME`, `add policy NAME marker MARKER`, `policy NAME status` |
| Runtime | `task current [ID]`, `task .`, `session current [ID]`, `session .`, `dashboard` |
| Agents | `agent install\|uninstall\|hooks --profile claude\|codex` |
| Integrations | `integrate jira\|slack\|whatsapp\|github\|mongodb` |

`protect` and `target` options: `policy NAME`, `checkpoint NAME`, `start-line N`, `end-line N`,
`name LABEL`. Run `crane help` for details. Governance commands refuse to run inside an AI agent
session, and the hooks deny them.

## Zones, flows, and context packs

Optional `.crane/governance.yaml` adds boundaries that only ever restrict:

```yaml
version: 1
zones:
  - id: payment-core
    selectors: { paths: ["app/payments/**"] }
    criticality: critical          # routine | sensitive | critical | restricted
    autonomy_ceiling: assisted     # observe | assisted | delegated | autonomous
flows:
  - id: checkout
    entry_points: ["app/service.py:submit"]   # files reached by its calls are covered
    autonomy_ceiling: delegated
context_packs:
  - id: payment-invariants
    version: 1
    content: ["Payment processing must be idempotent."]
```

## Integrations, alerts, and MongoDB

- `crane integrate slack|jira|whatsapp|github|mongodb` stores settings outside the repository and can test
  the connection. Security alerts go to Slack and WhatsApp, and to the Jira issue in
  `CRANE_TASK_ID`.
- `.github/workflows/pr-notify.yml` posts pull-request activity to Slack and comments on the Jira
  issue named in the PR title or branch. Set the repository secrets `SLACK_WEBHOOK_URL`,
  `JIRA_BASE_URL`, `JIRA_EMAIL`, and `JIRA_API_TOKEN`.
- MongoDB: build with `cargo install --path . --features mongodb`, then run
  `crane integrate mongodb` (Atlas or self-managed). The connection string is stored owner-only
  in `~/.crane-trust`, never in the repository or an agent's environment. Data is copied by a
  background sync when a tool call ends, when a session ends, after every command, and
  periodically; the dashboard then reads from MongoDB. Hooks never wait for the database.

## Environment variables

All optional. Crane does not read `.env` files; set them in your shell or CI. Credentials for
MongoDB, Slack, Jira, WhatsApp, and GitHub are set with `crane integrate NAME` instead.

| Variable | Purpose | Default |
| --- | --- | --- |
| `CRANE_HOME` | Trust directory (keys, evidence, secrets); use an absolute path | `~/.crane-trust` |
| `CRANE_TRUSTED_PUBLIC_KEYS` | Extra registry public keys (hex, comma separated) for CI or teammates | none |
| `CRANE_GITHUB_HOSTS` | GitHub Enterprise hosts accepted by `crane repo` | `github.com` only |
| `CRANE_GH` | Path of the GitHub CLI | `gh` |
| `CRANE_TASK_ID`, `CRANE_TASK_NAME` | Link agent sessions to a tracker task (Jira alerts use the ID) | none |
| `CRANE_SESSION_ID` | Session id when the agent reports none | generated |
| `CRANE_ENVIRONMENT` | Environment label on events | `local` (`ci` when `CI` is set) |
| `CRANE_DASHBOARD_PORT`, `CRANE_DASHBOARD_NO_BROWSER` | Dashboard port; `1` skips opening a browser | free port, opens browser |
| `CRANE_ALERTS_DRY_RUN` | `1` records alerts without delivering them | `0` |
| `CRANE_MONGODB_URI`, `CRANE_MONGODB_DB`, `CRANE_MONGODB_EVENT_RETENTION_DAYS` | MongoDB fallback for CI only | unset, `foxx`, `30` |
| `CRANE_ALLOW_LOCAL_REMOTE`, `CRANE_TEST_MONGODB_URI` | Offline mirrors and the live MongoDB test suite | unset |

## Security notes

Signing keys and runtime evidence live in `CRANE_HOME` (default `~/.crane-trust`), never in the
repository. An agent with an unrestricted shell running as your user can still read that directory;
run agents in a sandbox or separate account for a hard boundary. Network, CPU, memory, and token
usage are not observed by hooks and are reported as unsupported.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
bash scripts/smoke_test.sh
```

The VS Code extension lives in [extensions/foxx](extensions/foxx).

## License

Apache-2.0
