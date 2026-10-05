use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/** Counter that keeps fixture directory names unique when tests run in parallel */
static REPOSITORIES: AtomicUsize = AtomicUsize::new(0);

/** Environment variables that mark a process as running for an agent */
const AGENT_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CRANE_AGENT",
];

/** The Slack signing secret the tests use */
const SECRET: &str = "test-signing-secret-8f2a";

/** Path of the payment service, in a critical zone */
const SERVICE: &str = "services/payments/src/main/java/com/acme/payments/PaymentService.java";

/** Payment service source */
const PAYMENT: &str = "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n";

/** The notes file: unzoned, a routine change */
const NOTES: &str = "docs/notes.md";

/** Find a Python interpreter (checks and Slack signatures use it)
 * Input
    - None
 * Output
    - String program name
*/
fn python() -> String {
    for candidate in ["python3", "python"] {
        let works = Command::new(candidate)
            .args(["-c", "print(1)"])
            .output()
            .is_ok_and(|output| output.status.success() && output.stdout.starts_with(b"1"));
        if works {
            return candidate.into();
        }
    }
    panic!("these tests need python3 or python on PATH");
}

/** A temporary repository with Crane initialized, a critical payments zone, and a policy
 * preserving PaymentService.charge, removed on drop
 * Fields
    - root: PathBuf - repository root
    - python: String - Python interpreter
*/
struct Repository {
    root: PathBuf,
    python: String,
}

impl Repository {
    /** Create the fixture repository with a delivery configuration
     * Input
        - delivery: Value - .crane/delivery.json
     * Output
        - Repository
    */
    fn new(delivery: Value) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-delivery-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Self {
            root,
            python: python(),
        };
        repository.write("README.md", "# Shop\n");
        repository.write("services/payments/pom.xml", "<project/>\n");
        repository.write(SERVICE, PAYMENT);
        repository.write(NOTES, "notes\n");
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "lead@example.com"],
            vec!["config", "user.name", "Crane Delivery Test"],
            vec!["config", "core.autocrlf", "false"],
            vec!["add", "."],
            vec!["commit", "-qm", "baseline"],
        ] {
            repository.git(&args);
        }
        repository.human(&["init"]);
        repository.human(&["checkpoint", "--name", "baseline"]);
        repository.write(".crane/zones/org.zone", "zone payments {\n    criticality critical;\n    autonomy assisted;\n    select subsystem payments;\n}\n");
        repository.write(".crane/policies/core.crane", "policy core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n");
        repository.write(".crane/delivery.json", &delivery.to_string());
        // .crane is configuration, not part of the delivered change
        repository.git(&[
            "add",
            ".crane/zones",
            ".crane/policies",
            ".crane/delivery.json",
        ]);
        repository.git(&["commit", "-qm", "crane configuration"]);
        repository
    }

    /** Write a file relative to the root, creating parent folders
     * Input
        - path: &str - relative path
        - content: &str - file content
     * Output
        - None
    */
    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /** Read a file relative to the root
     * Input
        - path: &str - relative path
     * Output
        - String
    */
    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }

    /** Run git in the repository and require success
     * Input
        - args: &[&str] - git arguments
     * Output
        - String trimmed stdout
    */
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout).trim().to_string()
    }

    /** Run crane with agent markers removed and extra variables
     * Input
        - args: &[&str] - crane arguments
        - environment: &[(&str, &str)] - variables
        - stdin: &str - standard input
     * Output
        - Output
    */
    fn crane(&self, args: &[&str], environment: &[(&str, &str)], stdin: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
        command
            .args(args)
            .current_dir(&self.root)
            .env("CRANE_TEST_SLACK_SECRET", SECRET)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for marker in AGENT_MARKERS {
            command.env_remove(marker);
        }
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("crane should execute");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /** Run crane as a human and require success
     * Input
        - args: &[&str] - crane arguments
     * Output
        - String stdout
    */
    fn human(&self, args: &[&str]) -> String {
        let output = self.crane(args, &[], "");
        assert!(
            output.status.success(),
            "crane {args:?}: {}{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /** Run crane as a human with --json and parse the output
     * Input
        - args: &[&str] - crane arguments
     * Output
        - Value
    */
    fn json(&self, args: &[&str]) -> Value {
        let mut full = args.to_vec();
        full.push("--json");
        serde_json::from_str(&self.human(&full)).unwrap()
    }

    /** Send a Claude hook event for session s1
     * Input
        - event: &str - hook event
        - payload: Value - payload
     * Output
        - Output
    */
    fn hook(&self, event: &str, mut payload: Value) -> Output {
        payload["session_id"] = json!("s1");
        self.crane(
            &["agent", "hook", "--event", event, "--profile", "claude"],
            &[],
            &payload.to_string(),
        )
    }

    /** Run a session: a human starts it, the agent writes files (each authorized, written, and
     * reported), and the agent host stops
     * Input
        - options: &[&str] - session start options
        - writes: &[(&str, &str)] - files and their new content
     * Output
        - None
    */
    fn session(&self, options: &[&str], writes: &[(&str, &str)]) {
        let mut args = vec![
            "agent",
            "session",
            "start",
            "--profile",
            "claude",
            "--session",
            "s1",
        ];
        args.extend_from_slice(options);
        self.human(&args);
        for (path, content) in writes {
            let payload = json!({"tool_name": "Write", "tool_input": {"file_path": self.root.join(path).to_string_lossy(), "content": content}});
            let before = self.hook("pre-tool-use", payload.clone());
            assert!(before.status.success(), "{}", text(&before.stderr));
            self.write(path, content);
            assert!(self.hook("post-tool-use", payload).status.success());
        }
        assert!(self
            .hook("stop", json!({"stop_hook_active": false}))
            .status
            .success());
    }

    /** List the outbox messages for a channel
     * Input
        - channel: &str - slack, jira, or asana
     * Output
        - Vec<Value>
    */
    fn outbox(&self, channel: &str) -> Vec<Value> {
        let directory = self.root.join(".crane/runtime/delivery/outbox");
        let mut names = fs::read_dir(&directory)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        names.sort();
        names
            .into_iter()
            .filter(|name| name.ends_with(&format!("-{channel}.json")))
            .map(|name| {
                serde_json::from_str(&fs::read_to_string(directory.join(name)).unwrap()).unwrap()
            })
            .collect()
    }

    /** Build a signed Slack interaction request for a button click
     * Input
        - user: &str - Slack user id
        - action: &str - action id
        - value: Value - button value
        - secret: &str - signing secret
     * Output
        - (String, String, String) body, timestamp, and signature
    */
    fn signed(
        &self,
        user: &str,
        action: &str,
        value: Value,
        secret: &str,
    ) -> (String, String, String) {
        let payload = json!({"type": "block_actions", "user": {"id": user}, "actions": [{"action_id": action, "value": value.to_string()}]});
        let encoded = payload
            .to_string()
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() {
                    (byte as char).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect::<String>();
        let body = format!("payload={encoded}");
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let signed = Command::new(&self.python)
            .args(["-c", "import hmac, hashlib, sys; print('v0=' + hmac.new(sys.argv[1].encode(), ('v0:' + sys.argv[2] + ':' + sys.argv[3]).encode(), hashlib.sha256).hexdigest())", secret, &timestamp, &body])
            .output()
            .unwrap();
        (body, timestamp, text(&signed.stdout).trim().to_string())
    }

    /** Send a Slack button click through crane deliver slack-action, signed with a secret
     * Input
        - user: &str - Slack user id
        - action: &str - action id
        - value: Value - button value
        - secret: &str - signing secret
     * Output
        - Output
    */
    fn click(&self, user: &str, action: &str, value: Value, secret: &str) -> Output {
        let (body, timestamp, signature) = self.signed(user, action, value, secret);
        let file = self.root.join("slack-request.txt");
        fs::write(&file, &body).unwrap();
        self.crane(
            &[
                "deliver",
                "slack-action",
                "--body",
                file.to_str().unwrap(),
                "--timestamp",
                &timestamp,
                "--signature",
                &signature,
                "--json",
            ],
            &[],
            "",
        )
    }

    /** Post a request to the HTTP endpoint (crane task serve --once) and return the response
     * Input
        - path: &str - request path
        - headers: &str - extra header lines
        - body: &str - request body
     * Output
        - String
    */
    fn post(&self, path: &str, headers: &str, body: &str) -> String {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
        command
            .args(["task", "serve", "--addr", "127.0.0.1:0", "--once"])
            .current_dir(&self.root)
            .env("CRANE_WEBHOOK_TOKEN", "test-token-0123456789")
            .env("CRANE_TEST_SLACK_SECRET", SECRET)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for marker in AGENT_MARKERS {
            command.env_remove(marker);
        }
        let mut server = command.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(server.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line
            .split("http://")
            .nth(1)
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .to_string();
        let mut stream = TcpStream::connect(&address).unwrap();
        write!(
            stream,
            "POST {path} HTTP/1.1
Host: {address}
{headers}Content-Type: application/x-www-form-urlencoded
Content-Length: {}

{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        server.wait().unwrap();
        response
    }

    /** Read everything a Slack approval or exception must never change
     * Input
        - None
     * Output
        - Vec<String>
    */
    fn permanent(&self) -> Vec<String> {
        let mut files = Vec::new();
        for directory in [".crane/policies", ".crane/zones"] {
            for entry in fs::read_dir(self.root.join(directory)).unwrap() {
                files.push(fs::read_to_string(entry.unwrap().path()).unwrap());
            }
        }
        files.push(self.read(".crane/delivery.json"));
        files
    }
}

impl Drop for Repository {
    /** Remove the temporary repository and any worktrees
     * Input
        - None (uses self)
     * Output
        - None
    */
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/** Decode process output as text
 * Input
    - bytes: &[u8] - output
 * Output
    - String
*/
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/** Configure a task tracked by orchestration (from Jira) waiting in PR_READY
 * Input
    - repository: &Repository - repository
 * Output
    - None
*/
fn tracked_task(repository: &Repository) {
    repository.write(".crane/tasks/PAY-1.json", "{\"task_format\": 1, \"task_id\": \"PAY-1\", \"title\": \"Lower the payment fee\", \"description\": \"Change `PaymentService.fee` without changing `PaymentService.charge`.\", \"acceptance_criteria\": [\"fee is lower\"], \"repositories\": [\"acme/shop\"]}");
    repository.write(".crane/runtime/tasks/PAY-1/state.json", &json!({
        "orchestration_format": 1, "task_id": "PAY-1", "source": "jira", "external_id": "PAY-1", "state": "PR_READY",
        "reason": null, "contract_version": 1, "task_digest": null, "proposal": null, "active_proposal": null,
        "sessions": ["claude-s1"], "processed_events": [], "versions": [], "history": [], "validated_attestation": null,
    }).to_string());
}

/** Autonomous + routine + every check passing: the delivery branch, the final contract tests,
 * the repository tests, and the lint and security checks run, the pull request carries the
 * contract and attestation, Slack is notified, and the change merges automatically; the merge
 * commit becomes a trusted checkpoint and the attestation is re-finalized with it
 */
#[test]
fn autonomous_routine_change_merges_automatically() {
    let python = python();
    let repository = Repository::new(json!({
        "checks": [
            {"name": "lint", "kind": "lint", "command": [python, "-c", "print('lint ok')"]},
            {"name": "audit", "kind": "security", "command": [python, "-c", "print('no findings')"]},
        ],
        "slack": {"notify": ["#payments-delivery", "@release-lead"]},
    }));
    let before = repository.permanent();
    repository.session(&["--autonomy", "autonomous"], &[(NOTES, "updated notes\n")]);
    let delivered = repository.json(&["deliver", "run", "claude-s1"]);
    let merged = &delivered["merged"];
    assert!(!merged.is_null(), "{delivered}");
    let sha = merged["sha"].as_str().unwrap();
    assert_eq!(repository.git(&["rev-parse", "main"]), sha);
    assert_eq!(repository.git(&["branch", "--show-current"]), "main");
    assert_eq!(repository.read(NOTES), "updated notes\n");
    assert_eq!(merged["by"], "crane (auto-merge policy)");
    assert_eq!(merged["rule"], "autonomous-routine");
    let commit = repository.git(&["log", "-1", "--format=%B", "crane/claude-s1"]);
    assert!(
        commit.contains("Delivered by Crane from contract session claude-s1"),
        "{commit}"
    );
    assert_eq!(
        repository.git(&["diff", "--name-only", "main~1", "crane/claude-s1"]),
        NOTES,
        "only the session's change is delivered"
    );
    assert_eq!(delivered["criticality"], "routine");
    assert_eq!(delivered["contract_tests"]["failed"], 0);
    let checks = delivered["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| {
            format!(
                "{}:{}",
                check["name"].as_str().unwrap(),
                check["status"].as_str().unwrap()
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        checks,
        [
            "repository_tests:not_configured",
            "lint:passed",
            "audit:passed"
        ]
    );

    let trusted: Value =
        serde_json::from_str(&repository.read(".crane/checkpoints/trusted_claude_s1.json"))
            .unwrap();
    assert_eq!(
        trusted["commit"], sha,
        "the merge commit is the new trusted state"
    );
    let body = repository.read(".crane/runtime/delivery/claude-s1/pull_request.md");
    assert!(body.contains("## Contract"), "{body}");
    assert!(
        body.contains("`core`: preserve function `PaymentService.charge`"),
        "{body}"
    );
    let attestation: Value = serde_json::from_str(
        &repository.read(".crane/runtime/sessions/claude-s1/final_attestation.json"),
    )
    .unwrap();
    assert_eq!(
        attestation["delivery"]["merged"]["merge_sha"], sha,
        "the attestation is re-finalized with the merge"
    );
    assert_eq!(
        attestation["delivery"]["merged"]["trusted_checkpoint"],
        "trusted_claude_s1"
    );
    assert!(body.contains("## Attestation"));
    let slack = repository.outbox("slack");
    assert_eq!(
        slack
            .iter()
            .filter(|message| !message["request"]["blocks"].is_null())
            .count(),
        2,
        "one announcement per target"
    );
    assert!(slack.iter().any(|message| message["request"]["text"]
        .as_str()
        .unwrap()
        .contains(&format!("merged into main as {sha}"))));
    assert!(
        repository.outbox("jira").is_empty(),
        "no tracked task, no tracker completion"
    );
    assert_eq!(delivered["chain"]["status"], "verified");
    assert_eq!(repository.permanent(), before);
    let inspected = repository.human(&["session", "inspect", "claude-s1"]);
    assert!(inspected.contains("chain verified"), "{inspected}");
}

/** Critical changes wait for the configured approvals: signed Slack clicks from mapped approvers
 * count, unsigned, unmapped, and stale ones do not; once eligible a human merges; only then is the
 * task completed and the Jira issue closed; nothing permanent changes
 */
#[test]
fn critical_change_requires_configured_approval() {
    let repository = Repository::new(json!({
        "merge_policy": {"rules": [{"name": "payments-critical", "min_criticality": "critical", "approvals": 2, "approvers": ["payments-lead", "security-lead"]}]},
        "slack": {"notify": ["#payments"], "signing_secret_env": "CRANE_TEST_SLACK_SECRET", "users": {"U100": "payments-lead", "U200": "security-lead"}, "pr_url_template": "https://git.example/acme/shop/pull/{number}"},
        "trackers": {"jira": {"done_transition": "31"}},
    }));
    tracked_task(&repository);
    let before = repository.permanent();
    repository.session(
        &["--autonomy", "autonomous", "--task", "PAY-1"],
        &[(SERVICE, &PAYMENT.replace("amount / 10", "amount / 20"))],
    );
    let delivered = repository.json(&["deliver", "run", "claude-s1"]);
    assert!(delivered["merged"].is_null());
    assert_eq!(delivered["criticality"], "critical");
    assert_eq!(
        delivered["eligibility"]["rule"]["name"],
        "payments-critical"
    );
    assert_eq!(
        delivered["eligibility"]["missing"],
        json!(["0 of 2 required approvals from payments-lead, security-lead"])
    );
    assert_eq!(
        delivered["pull_request"]["url"],
        "https://git.example/acme/shop/pull/1"
    );
    let task: Value =
        serde_json::from_str(&repository.read(".crane/runtime/tasks/PAY-1/state.json")).unwrap();
    assert_eq!(
        task["state"], "REVIEW",
        "opening the pull request moves the task to review"
    );

    let announcement = repository
        .outbox("slack")
        .into_iter()
        .find(|message| !message["request"]["blocks"].is_null())
        .unwrap();
    let buttons = announcement["request"]["blocks"][1]["elements"]
        .as_array()
        .unwrap()
        .clone();
    let actions = buttons
        .iter()
        .map(|button| button["action_id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        actions,
        [
            "view_pr",
            "view_contract",
            "approve",
            "reject",
            "request_changes"
        ],
        "no exception button where none is allowed"
    );
    assert_eq!(buttons[0]["url"], "https://git.example/acme/shop/pull/1");
    let value: Value = serde_json::from_str(buttons[2]["value"].as_str().unwrap()).unwrap();
    assert_eq!(value["head"], delivered["head"]);

    let forged = repository.click("U100", "approve", value.clone(), "not-the-secret");
    assert!(!forged.status.success());
    assert!(text(&forged.stderr).contains("signature does not match"));
    let stranger = repository.click("U999", "approve", value.clone(), SECRET);
    assert!(text(&stranger.stderr).contains("not mapped to an approver"));
    let stale = repository.click(
        "U100",
        "approve",
        json!({"delivery": "claude-s1", "head": "0000000"}),
        SECRET,
    );
    assert!(text(&stale.stderr).contains("earlier commit"));
    let viewed = repository.click("U200", "view_contract", value.clone(), SECRET);
    assert!(viewed.status.success(), "{}", text(&viewed.stderr));
    let approved = repository.click("U100", "approve", value.clone(), SECRET);
    assert!(approved.status.success(), "{}", text(&approved.stderr));
    let status: Value = serde_json::from_slice(&approved.stdout).unwrap();
    assert_eq!(
        status["eligibility"]["approvals"]["by"],
        json!(["payments-lead"])
    );
    let refused = repository.crane(&["deliver", "merge", "claude-s1"], &[], "");
    assert!(
        text(&refused.stderr).contains("1 of 2 required approvals"),
        "{}",
        text(&refused.stderr)
    );
    assert!(
        repository.outbox("jira").is_empty(),
        "nothing reaches Jira before a verified merge"
    );

    let ready = repository.json(&[
        "deliver",
        "approve",
        "claude-s1",
        "--approver",
        "security-lead",
        "--reason",
        "reviewed the fee change",
    ]);
    assert_eq!(ready["eligibility"]["eligible"], true);
    assert!(
        ready["merged"].is_null(),
        "critical changes are never merged automatically"
    );
    let merged = repository.json(&["deliver", "merge", "claude-s1", "--by", "release-manager"]);
    let sha = merged["merged"]["sha"].as_str().unwrap().to_string();
    assert_eq!(repository.git(&["rev-parse", "main"]), sha);
    assert_eq!(
        merged["merged"]["approvals"]["by"],
        json!(["payments-lead", "security-lead"])
    );
    let task: Value =
        serde_json::from_str(&repository.read(".crane/runtime/tasks/PAY-1/state.json")).unwrap();
    assert_eq!(task["state"], "COMPLETED");
    assert_eq!(task["merge_sha"], sha);
    let jira = repository.outbox("jira");
    assert_eq!(jira.len(), 2);
    assert_eq!(
        jira[0]["request"]["path"],
        "/rest/api/3/issue/PAY-1/comment"
    );
    assert!(jira[0]["request"]["body"].to_string().contains(&sha));
    assert_eq!(jira[1]["request"]["body"]["transition"]["id"], "31");
    assert_eq!(merged["task_completion"]["tracker"], "jira");
    let journal = fs::read_to_string(
        repository
            .root
            .join(".crane/runtime/delivery/claude-s1/journal.jsonl"),
    )
    .unwrap();
    assert!(journal.contains("\"unauthorized_action\""));
    assert!(journal.contains("\"viewed\""));
    assert_eq!(
        repository.permanent(),
        before,
        "approvals never modify permanent policy"
    );
}

/** Rejections and change requests block the merge, whatever the approvals */
#[test]
fn rejection_and_change_requests_block_merging() {
    let repository = Repository::new(json!({}));
    repository.session(&["--autonomy", "delegated"], &[(NOTES, "draft\n")]);
    let delivered = repository.json(&["deliver", "run", "claude-s1"]);
    assert_eq!(
        delivered["eligibility"]["missing"],
        json!(["0 of 1 required approvals"])
    );
    repository.human(&[
        "deliver",
        "request-changes",
        "claude-s1",
        "--approver",
        "lead",
        "--reason",
        "add a changelog entry",
    ]);
    let approved = repository.json(&[
        "deliver",
        "approve",
        "claude-s1",
        "--approver",
        "other-lead",
    ]);
    assert_eq!(
        approved["eligibility"]["missing"],
        json!(["lead requested changes"])
    );
    repository.human(&[
        "deliver",
        "reject",
        "claude-s1",
        "--approver",
        "other-lead",
        "--reason",
        "not now",
    ]);
    let refused = repository.crane(&["deliver", "merge", "claude-s1"], &[], "");
    assert!(
        text(&refused.stderr).contains("other-lead rejected the change"),
        "{}",
        text(&refused.stderr)
    );
    assert_eq!(repository.read(NOTES), "notes\n", "main is untouched");
}

/** Exceptions are scoped to one allowed check of one commit and session, temporary, and audited;
 * the contract tests and checks not allowed in the configuration can never be excepted
 */
#[test]
fn scoped_exceptions() {
    let python = python();
    let repository = Repository::new(json!({
        "checks": [{"name": "lint", "kind": "lint", "command": [python, "-c", "import sys; sys.exit(1)"]}],
        "exceptions": {"allowed_checks": ["lint"], "max_duration_seconds": 7200},
    }));
    let before = repository.permanent();
    repository.session(
        &["--autonomy", "autonomous"],
        &[(NOTES, "with a lint problem\n")],
    );
    let delivered = repository.json(&["deliver", "run", "claude-s1"]);
    assert!(delivered["merged"].is_null());
    assert_eq!(
        delivered["eligibility"]["missing"],
        json!(["check lint is failed"])
    );
    for (check, message) in [
        ("contract_tests", "can never be excepted"),
        ("repository_tests", "cannot be excepted"),
        ("audit", "cannot be excepted"),
    ] {
        let refused = repository.crane(
            &[
                "deliver",
                "exception",
                "claude-s1",
                "--check",
                check,
                "--approver",
                "lead",
                "--reason",
                "x",
            ],
            &[],
            "",
        );
        assert!(
            text(&refused.stderr).contains(message),
            "{check}: {}",
            text(&refused.stderr)
        );
    }
    let too_long = repository.crane(
        &[
            "deliver",
            "exception",
            "claude-s1",
            "--check",
            "lint",
            "--approver",
            "lead",
            "--reason",
            "x",
            "--expires",
            "3h",
        ],
        &[],
        "",
    );
    assert!(text(&too_long.stderr).contains("between 1 and 7200 seconds"));

    let excepted = repository.json(&[
        "deliver",
        "exception",
        "claude-s1",
        "--check",
        "lint",
        "--approver",
        "lead",
        "--reason",
        "linter crash, tracked in LINT-4",
        "--expires",
        "1h",
    ]);
    assert!(
        !excepted["merged"].is_null(),
        "the exception completes the auto-merge policy: {excepted}"
    );
    let exception = &excepted["exceptions"][0];
    assert_eq!(exception["check"], "lint");
    assert_eq!(exception["session"], "claude-s1");
    assert_eq!(exception["head"], delivered["head"]);
    assert!(exception["expires_at"].as_u64().unwrap() > 0);
    assert!(exception["scope"]
        .as_str()
        .unwrap()
        .contains("check lint of delivery claude-s1"));
    assert_eq!(excepted["merged"]["excepted"][0]["check"], "lint");
    let exported: Value =
        serde_json::from_str(&repository.human(&["session", "export", "claude-s1", "--json"]))
            .unwrap();
    assert!(exported["attestation"]["human_interventions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["intervention"]["action"] == "exception"
            && entry["intervention"]["check"] == "lint"));
    assert_eq!(
        repository.permanent(),
        before,
        "an exception never changes a policy"
    );
}

/** Agents cannot deliver, approve, except, or merge: refused in an agent environment and denied to
 * agent shells, and the attempt quarantines the session
 */
#[test]
fn agents_cannot_deliver_or_approve() {
    let repository = Repository::new(json!({}));
    repository.session(&["--autonomy", "delegated"], &[(NOTES, "draft\n")]);
    repository.human(&["deliver", "run", "claude-s1"]);
    let attempt = repository.crane(
        &["deliver", "approve", "claude-s1", "--approver", "me"],
        &[("CLAUDECODE", "1")],
        "",
    );
    assert!(!attempt.status.success());
    assert!(text(&attempt.stderr).contains("refuses to run in an agent environment"));
    let status: Value =
        serde_json::from_str(&repository.human(&["autonomy", "status", "claude-s1", "--json"]))
            .unwrap();
    assert_eq!(status["safety"], "quarantined");
    let denied = repository.hook(
        "pre-tool-use",
        json!({"tool_name": "Bash", "tool_input": {"command": "crane deliver merge claude-s1"}}),
    );
    assert_eq!(denied.status.code(), Some(2));
    let read = repository.crane(
        &["deliver", "status", "claude-s1", "--json"],
        &[("CLAUDECODE", "1")],
        "",
    );
    assert!(read.status.success(), "status only reads");
}

/** A session that did not reconcile is never delivered */
#[test]
fn unreconciled_sessions_are_not_delivered() {
    let repository = Repository::new(json!({}));
    repository.session(&["--autonomy", "autonomous"], &[]);
    repository.write(SERVICE, &PAYMENT.replace("fee(amount) + amount", "amount"));
    let refused = repository.crane(&["deliver", "run", "claude-s1"], &[], "");
    assert!(!refused.status.success());
    assert!(
        text(&refused.stderr).contains("did not reconcile (FAIL)"),
        "{}",
        text(&refused.stderr)
    );
    assert_eq!(
        repository.git(&["branch", "--list", "crane/*"]),
        "",
        "no branch was created"
    );
}

/** The HTTP endpoint takes Slack's button clicks: a forged signature is refused with 401, a signed
 * approval from a mapped approver is recorded and completes the merge policy
 */
#[test]
fn slack_endpoint_verifies_signatures() {
    let repository = Repository::new(
        json!({"slack": {"notify": ["#shop"], "signing_secret_env": "CRANE_TEST_SLACK_SECRET", "users": {"U100": "lead"}}}),
    );
    repository.session(
        &["--autonomy", "delegated"],
        &[(
            NOTES, "draft
",
        )],
    );
    let delivered = repository.json(&["deliver", "run", "claude-s1"]);
    let value = json!({"delivery": "claude-s1", "head": delivered["head"]});
    let (body, timestamp, _) = repository.signed("U100", "approve", value.clone(), "forged");
    let forged = repository.post(
        "/slack/actions",
        &format!(
            "X-Slack-Request-Timestamp: {timestamp}
X-Slack-Signature: v0=0000
"
        ),
        &body,
    );
    assert!(forged.starts_with("HTTP/1.1 401"), "{forged}");
    let (body, timestamp, signature) = repository.signed("U100", "approve", value, SECRET);
    let accepted = repository.post(
        "/slack/actions",
        &format!(
            "X-Slack-Request-Timestamp: {timestamp}
X-Slack-Signature: {signature}
"
        ),
        &body,
    );
    assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");
    let status = repository.json(&["deliver", "status", "claude-s1"]);
    assert_eq!(status["eligibility"]["eligible"], true);
    assert_eq!(status["approvals"][0]["via"], "slack");
    assert_eq!(status["approvals"][0]["by"], "lead");
}
