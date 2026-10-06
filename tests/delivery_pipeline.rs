use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/** Counter that keeps fixture directory names unique when tests run in parallel */
static REPOSITORIES: AtomicUsize = AtomicUsize::new(0);

/** Environment variables that mark a process as an agent or bind a hook */
const CLEARED: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CRANE_AGENT",
    "CRANE_SESSION",
    "CRANE_TASK_ID",
];

/** Slack signing secret the tests sign interactions with */
const SECRET: &str = "test-signing-secret-0123456789";

/** The invoice service, outside every zone */
const INVOICE: &str = "services/billing/src/main/java/com/acme/billing/InvoiceService.java";

/** The payment service, in the Critical payments zone */
const PAYMENT: &str = "services/payments/src/main/java/com/acme/payments/PaymentService.java";

/** The invoice change the billing task asks for */
const ROUNDED: &str = "return Math.round(amount * 100.0) / 100.0;";

/** The governed session of the billing task */
const SESSION: &str = "codex-task-PAY-1830-v1";

/** Repository files */
const FILES: &[(&str, &str)] = &[
    ("services/payments/pom.xml", "<project/>\n"),
    (
        PAYMENT,
        "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    public int refund(int amount) {\n        return charge(-amount);\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n",
    ),
    ("services/billing/pom.xml", "<project/>\n"),
    (
        INVOICE,
        "package com.acme.billing;\n\npublic class InvoiceService {\n    public double total(double amount) {\n        return amount * 1.0;\n    }\n}\n",
    ),
];

/** Find a Python interpreter (used only to sign Slack requests the way Slack does)
 * Input
    - None
 * Output
    - String
*/
fn python() -> String {
    for candidate in ["python3", "python"] {
        if Command::new(candidate)
            .args(["-c", "print(1)"])
            .output()
            .is_ok_and(|output| output.status.success() && output.stdout.starts_with(b"1"))
        {
            return candidate.into();
        }
    }
    panic!("these tests need python3 or python on PATH");
}

/** A connected repository with a payments zone, a permanent policy, Jira tasks, Slack approvers,
 * and a merge policy (one approval by payments-lead by default, two for critical changes), whose
 * billing task already ran through a governed session to DELIVERY_READY; removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Repository {
    root: PathBuf,
}

impl Repository {
    /** Create the fixture with a delivery configuration
     * Input
        - delivery: Value - .crane/delivery.json
     * Output
        - Repository
    */
    fn new(delivery: Value) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-pipeline-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Self { root };
        for (path, content) in FILES {
            repository.write(path, content);
        }
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "lead@example.com"],
            vec!["config", "user.name", "Crane Pipeline"],
            vec!["config", "core.autocrlf", "false"],
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/acme/shop.git",
            ],
            vec!["add", "."],
            vec!["commit", "-qm", "baseline"],
        ] {
            repository.git(&args);
        }
        repository.crane(&["init"]);
        repository.crane(&["checkpoint", "--name", "baseline"]);
        repository.write(".crane/zones/org.zone", "zone payments {\n    criticality critical;\n    autonomy assisted;\n    select subsystem payments;\n}\n");
        repository.write(".crane/policies/payments_core.crane", "policy payments_core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n");
        repository.write(".crane/sources/config.json", r#"{"sources_format": 1, "checkpoint": "baseline", "jira": {"acceptance_field": "customfield_10050", "projects": {"PAY": {"repositories": ["acme/shop"], "team": "payments"}}}}"#);
        repository.write(
            ".crane/sources/jira/issues/PAY-1830.json",
            &json!({"id": "100", "key": "PAY-1830", "fields": {"summary": "Round invoice totals", "project": {"key": "PAY"}, "assignee": {"displayName": "Crane Bot"}, "status": {"name": "To Do", "statusCategory": {"key": "new"}}, "description": "Make `InvoiceService.total` round to cents.", "customfield_10050": "totals have two decimals"}}).to_string(),
        );
        repository.write(".crane/delivery.json", &delivery.to_string());
        repository.crane(&["repo", "connect"]);
        // The billing task runs through the governed lifecycle to DELIVERY_READY
        repository.crane(&["task", "prepare", "PAY-1830"]);
        let shown: Value =
            serde_json::from_str(&repository.crane(&["task", "show", "PAY-1830", "--json"]))
                .unwrap();
        let digest = shown["contract"]["digest"].as_str().unwrap().to_string();
        repository.crane(&[
            "task",
            "approve",
            "PAY-1830",
            "--approver",
            "lead",
            "--confirm",
            &digest[7..19],
        ]);
        let actions = repository.root.join(".crane/runtime/actions.json");
        fs::create_dir_all(actions.parent().unwrap()).unwrap();
        fs::write(&actions, json!([{"tool": "Edit", "operation": "write", "path": INVOICE, "edits": [{"old": "return amount * 1.0;", "new": ROUNDED}]}]).to_string()).unwrap();
        let run: Value = serde_json::from_str(&repository.crane(&[
            "session",
            "run",
            "PAY-1830",
            "--agent",
            "codex",
            "--actions",
            &actions.to_string_lossy(),
            "--json",
        ]))
        .unwrap();
        assert_eq!(run["record"]["phase"], "DELIVERY_READY", "{run}");
        repository
    }

    /** The usual configuration: Slack approvers and a merge policy */
    fn standard() -> Self {
        Self::new(json!({
            "merge_policy": {"rules": [{"name": "critical", "min_criticality": "critical", "approvals": 2, "approvers": ["payments-lead", "security-lead"]}], "default": {"name": "default", "approvals": 1, "approvers": ["payments-lead"]}},
            "slack": {"notify": ["#shop"], "signing_secret_env": "CRANE_TEST_SLACK_SECRET", "users": {"U100": "payments-lead", "U200": "security-lead"}},
            "trackers": {"jira": {"done_transition": "31"}},
        }))
    }

    /** Write a file relative to the root */
    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /** Read a file relative to the root */
    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }

    /** Run git and require success, returning stdout */
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

    /** Run crane with cleared markers, the Slack secret, and stdin */
    fn run(&self, args: &[&str], environment: &[(&str, &str)], stdin: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
        command
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in CLEARED {
            command.env_remove(name);
        }
        command.env("CRANE_TEST_SLACK_SECRET", SECRET);
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /** Run crane as a human and require success */
    fn crane(&self, args: &[&str]) -> String {
        let output = self.run(args, &[], "");
        assert!(
            output.status.success(),
            "crane {args:?}: {}{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /** Run crane with --json, returning success, the answer, and stderr */
    fn json(&self, args: &[&str]) -> (bool, Value, String) {
        let mut full = args.to_vec();
        full.push("--json");
        let output = self.run(&full, &[], "");
        (
            output.status.success(),
            serde_json::from_slice(&output.stdout).unwrap_or(Value::Null),
            text(&output.stderr),
        )
    }

    /** Deliver the governed session and return its status */
    fn deliver(&self) -> Value {
        let (ok, value, error) = self.json(&["deliver", "run", SESSION]);
        assert!(ok, "{error}");
        value
    }

    /** The delivery status */
    fn status(&self) -> Value {
        self.json(&["deliver", "status", SESSION]).1
    }

    /** The delivery journal */
    fn journal(&self) -> Vec<Value> {
        self.read(&format!(".crane/runtime/delivery/{SESSION}/journal.jsonl"))
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /** The Slack messages in the outbox, oldest first */
    fn slack(&self) -> Vec<Value> {
        let folder = self.root.join(".crane/runtime/delivery/outbox");
        let mut names = fs::read_dir(&folder)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.ends_with("-slack.json"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        names.sort();
        names
            .iter()
            .map(|name| {
                serde_json::from_str::<Value>(&fs::read_to_string(folder.join(name)).unwrap())
                    .unwrap()["request"]
                    .clone()
            })
            .collect()
    }

    /** The value of a button of the latest announcement */
    fn button(&self, action: &str) -> Value {
        let announcement = self
            .slack()
            .into_iter()
            .rev()
            .find(|message| message["blocks"].is_array())
            .expect("an announcement");
        let button = announcement["blocks"][1]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|button| button["action_id"] == action)
            .unwrap()
            .clone();
        serde_json::from_str(button["value"].as_str().unwrap()).unwrap()
    }

    /** Send a Slack button click, signed the way Slack signs it (or with a wrong secret) */
    fn click(&self, user: &str, action: &str, value: Value, secret: &str) -> Output {
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
        let signed = Command::new(python())
            .args(["-c", "import hmac, hashlib, sys; print('v0=' + hmac.new(sys.argv[1].encode(), ('v0:' + sys.argv[2] + ':' + sys.argv[3]).encode(), hashlib.sha256).hexdigest())", secret, &timestamp, &body])
            .output()
            .unwrap();
        let file = self.root.join(".crane/runtime/slack-request.txt");
        fs::write(&file, &body).unwrap();
        self.run(
            &[
                "deliver",
                "slack-action",
                "--body",
                &file.to_string_lossy(),
                "--timestamp",
                &timestamp,
                "--signature",
                text(&signed.stdout).trim(),
                "--json",
            ],
            &[],
            "",
        )
    }
}

impl Drop for Repository {
    /** Remove the repository */
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/** Decode process output */
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/** Count journal events of a kind */
fn count(journal: &[Value], kind: &str) -> usize {
    journal.iter().filter(|event| event["kind"] == kind).count()
}

/** verified -> PR: only a verified session is delivered; the pull request shows the task, the
 * contract digest, the checkpoint, the affected zones, the validation results, the attestation,
 * the autonomy mode, and the exceptions, and is bound to the exact checked commit and tree (the
 * commit carries the binding in its trailers) */
#[test]
fn verified_session_produces_a_bound_pull_request() {
    let repository = Repository::standard();
    let delivered = repository.deliver();
    assert_eq!(
        delivered["delivery_state"], "AWAITING_APPROVAL",
        "{delivered}"
    );
    assert_eq!(delivered["verification"]["by"], "orchestrator");
    assert_eq!(delivered["verification"]["phase"], "DELIVERY_READY");
    let head = delivered["head"].as_str().unwrap();
    assert_eq!(
        repository.git(&["rev-parse", "refs/heads/crane/codex-task-PAY-1830-v1"]),
        head
    );
    assert_eq!(
        repository.git(&["rev-parse", &format!("{head}^{{tree}}")]),
        delivered["tree"].as_str().unwrap()
    );
    let binding = &delivered["binding"];
    assert_eq!(binding["head"], head);
    assert_eq!(binding["tree"], delivered["tree"]);
    let contract: Value = serde_json::from_str(
        &repository.crane(&["task", "contract", "show", "PAY-1830", "--json"]),
    )
    .unwrap();
    assert_eq!(binding["contract_digest"], contract["digest"]);
    assert_eq!(binding["checkpoint"], contract["bindings"]["checkpoint"]);
    assert!(delivered["binding_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));

    let message = repository.git(&["log", "-1", "--format=%B", head]);
    assert!(
        message.contains(&format!("Crane-Session: {SESSION}")),
        "{message}"
    );
    assert!(message.contains(&format!(
        "Crane-Contract-Digest: {}",
        contract["digest"].as_str().unwrap()
    )));
    assert!(message.contains("Crane-Task: PAY-1830"));
    assert!(message.contains(&format!(
        "Crane-Attestation: {}",
        delivered["attestation_digest"].as_str().unwrap()
    )));

    let body = repository.read(&format!(
        ".crane/runtime/delivery/{SESSION}/pull_request.md"
    ));
    for expected in [
        "PAY-1830".to_string(),
        contract["digest"].as_str().unwrap().to_string(),
        contract["bindings"]["checkpoint"]["sha"]
            .as_str()
            .unwrap()
            .to_string(),
        "## Affected zones".to_string(),
        "## Validation".to_string(),
        "repository_tests".to_string(),
        delivered["attestation_digest"]
            .as_str()
            .unwrap()
            .to_string(),
        "autonomy mode **delegated**".to_string(),
        "## Exceptions\n\nNone.".to_string(),
        delivered["binding_digest"].as_str().unwrap().to_string(),
        head.to_string(),
        delivered["tree"].as_str().unwrap().to_string(),
    ] {
        assert!(
            body.contains(&expected),
            "{expected} missing from the pull request:\n{body}"
        );
    }
    assert!(delivered["eligibility"]["missing"]
        .to_string()
        .contains("0 of 1 required approvals"));

    // A governed session that is not verified is never delivered
    repository.crane(&[
        "session", "run", "PAY-1830", "--agent", "claude", "--detach",
    ]);
    let refused = repository.run(&["deliver", "run", "claude-task-PAY-1830-v1"], &[], "");
    assert!(!refused.status.success());
    assert!(
        text(&refused.stderr)
            .contains("is SESSION_CREATED; only a DELIVERY_READY session can be delivered"),
        "{}",
        text(&refused.stderr)
    );
}

/** verified -> PR -> Slack approval, duplicate approval, stale clicks: the Slack message and its
 * buttons identify the repository, task, commit, contract, and attestation; signatures are
 * verified; an approval is bound to the round's binding and counted once */
#[test]
fn slack_approval_is_bound_and_idempotent() {
    let repository = Repository::standard();
    let delivered = repository.deliver();
    let announcement = repository
        .slack()
        .into_iter()
        .find(|message| message["blocks"].is_array())
        .unwrap();
    let text_shown = announcement["text"].as_str().unwrap();
    for expected in [
        "acme/shop",
        "PAY-1830",
        delivered["head"].as_str().unwrap(),
        delivered["binding"]["contract_digest"].as_str().unwrap(),
        delivered["attestation_digest"].as_str().unwrap(),
        delivered["binding_digest"].as_str().unwrap(),
    ] {
        assert!(
            text_shown.contains(expected),
            "{expected} missing from Slack: {text_shown}"
        );
    }
    let approve = repository.button("approve");
    assert_eq!(approve["binding"], delivered["binding_digest"]);
    assert_eq!(approve["head"], delivered["head"]);
    assert_eq!(approve["task"], "PAY-1830");
    assert_eq!(approve["contract"], delivered["binding"]["contract_digest"]);
    assert_eq!(approve["attestation"], delivered["attestation_digest"]);

    let forged = repository.click("U100", "approve", approve.clone(), "not-the-secret");
    assert!(!forged.status.success());
    assert!(
        text(&forged.stderr).to_lowercase().contains("signature"),
        "{}",
        text(&forged.stderr)
    );
    let mut stale = approve.clone();
    stale["binding"] =
        json!("sha256:0000000000000000000000000000000000000000000000000000000000000000");
    let refused = repository.click("U100", "approve", stale, SECRET);
    assert!(
        text(&refused.stderr).contains("earlier commit or check result"),
        "{}",
        text(&refused.stderr)
    );

    let first = repository.click("U100", "approve", approve.clone(), SECRET);
    assert!(first.status.success(), "{}", text(&first.stderr));
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["delivery_state"], "APPROVED");
    let again: Value =
        serde_json::from_slice(&repository.click("U100", "approve", approve, SECRET).stdout)
            .unwrap();
    assert_eq!(again["already_recorded"], true);
    let journal = repository.journal();
    assert_eq!(
        count(&journal, "approval"),
        1,
        "a duplicate approval is counted once"
    );
    let approval = journal
        .iter()
        .find(|event| event["kind"] == "approval")
        .unwrap();
    assert_eq!(approval["binding"], delivered["binding_digest"]);
    assert_eq!(
        approval["checks_digest"],
        delivered["binding"]["checks_digest"]
    );
    assert_eq!(approval["via"], "slack");
    assert_eq!(
        count(&journal, "unauthorized_action"),
        1,
        "the stale click is recorded"
    );
}

/** verified -> PR -> rejected: a rejection bound to the round blocks the merge */
#[test]
fn rejection_blocks_the_merge() {
    let repository = Repository::standard();
    repository.deliver();
    let reject = repository.button("reject");
    let rejected: Value =
        serde_json::from_slice(&repository.click("U100", "reject", reject, SECRET).stdout).unwrap();
    assert_eq!(rejected["delivery_state"], "REJECTED");
    repository.crane(&["deliver", "approve", SESSION, "--approver", "payments-lead"]);
    let (ok, _, error) = repository.json(&["deliver", "merge", SESSION]);
    assert!(!ok);
    assert!(
        error.contains("payments-lead rejected the change"),
        "{error}"
    );
    assert!(repository.status()["merged"].is_null());
}

/** verified -> PR -> merge, duplicate merge, duplicate merge callback: merging needs the policy's
 * approval; the merge commit becomes the trusted checkpoint and the connected repository's trusted
 * state; the delivery is COMPLETE; merging or reporting the same merge again changes nothing */
#[test]
fn merge_completes_the_delivery_once() {
    let repository = Repository::standard();
    repository.deliver();
    let (ok, _, error) = repository.json(&["deliver", "merge", SESSION]);
    assert!(
        !ok && error.contains("0 of 1 required approvals"),
        "{error}"
    );
    repository.crane(&["deliver", "approve", SESSION, "--approver", "security-lead"]);
    let (ok, _, error) = repository.json(&["deliver", "merge", SESSION]);
    assert!(
        !ok && error.contains("from payments-lead"),
        "only the rule's approvers count: {error}"
    );
    repository.crane(&["deliver", "approve", SESSION, "--approver", "payments-lead"]);
    let (ok, merged, error) =
        repository.json(&["deliver", "merge", SESSION, "--by", "release-lead"]);
    assert!(ok, "{error}");
    assert_eq!(merged["delivery_state"], "COMPLETE");
    let sha = merged["merged"]["sha"].as_str().unwrap().to_string();
    assert_eq!(repository.git(&["rev-parse", "main"]), sha);
    assert_eq!(
        repository.git(&[
            "merge-base",
            "--is-ancestor",
            merged["head"].as_str().unwrap(),
            &sha
        ]),
        ""
    );
    let checkpoint: Value = serde_json::from_str(
        &repository.read(".crane/checkpoints/trusted_codex_task_PAY_1830_v1.json"),
    )
    .unwrap();
    assert_eq!(checkpoint["commit"], sha.as_str());
    let connection: Value =
        serde_json::from_str(&repository.read(".crane/connection.json")).unwrap();
    assert_eq!(
        connection["trusted_checkpoint"],
        "trusted_codex_task_PAY_1830_v1"
    );
    let lifecycle: Value =
        serde_json::from_str(&repository.crane(&["session", "lifecycle", SESSION, "--json"]))
            .unwrap();
    assert_eq!(lifecycle["record"]["delivery"]["state"], "COMPLETE");
    assert_eq!(lifecycle["record"]["delivery"]["merge_sha"], sha.as_str());

    let (ok, again, _) = repository.json(&["deliver", "merge", SESSION]);
    assert!(ok);
    assert_eq!(again["already_merged"], true);
    let (ok, callback, _) = repository.json(&["deliver", "merged", SESSION, "--sha", &sha]);
    assert!(ok);
    assert_eq!(callback["already_merged"], true);
    let baseline = repository.git(&["rev-parse", "main~1"]);
    let (ok, _, error) = repository.json(&["deliver", "merged", SESSION, "--sha", &baseline]);
    assert!(!ok && error.contains("already merged as"), "{error}");
    let journal = repository.journal();
    assert_eq!(count(&journal, "merged"), 1);
    assert_eq!(count(&journal, "attestation_finalized"), 1);
    let (ok, _, error) = repository.json(&["deliver", "run", SESSION]);
    assert!(!ok && error.contains("already merged"), "{error}");
}

/** merge failure: a merge that cannot be made is recorded; the delivery is MERGE_FAILED, the
 * session and its task are not completed, and nothing is trusted */
#[test]
fn merge_failure_completes_nothing() {
    let repository = Repository::standard();
    repository.deliver();
    repository.crane(&["deliver", "approve", SESSION, "--approver", "payments-lead"]);
    // Someone changed the same line on main in the meantime
    repository.write(
        INVOICE,
        &repository
            .read(INVOICE)
            .replace("return amount * 1.0;", "return amount * 1.5;"),
    );
    repository.git(&["commit", "-qam", "conflicting change"]);
    let (ok, _, error) = repository.json(&["deliver", "merge", SESSION]);
    assert!(!ok);
    assert!(
        error.contains("failed") && error.contains("not completed"),
        "{error}"
    );
    let status = repository.status();
    assert_eq!(status["delivery_state"], "MERGE_FAILED");
    assert!(status["merged"].is_null());
    let journal = repository.journal();
    assert_eq!(count(&journal, "merge_failed"), 1);
    assert_eq!(
        count(&journal, "task_completed"),
        0,
        "the task is not closed"
    );
    assert!(!repository
        .root
        .join(".crane/checkpoints/trusted_codex_task_PAY_1830_v1.json")
        .exists());
    let lifecycle: Value =
        serde_json::from_str(&repository.crane(&["session", "lifecycle", SESSION, "--json"]))
            .unwrap();
    assert!(
        lifecycle["record"]["delivery"].is_null(),
        "the session is not marked delivered"
    );
    let session = repository.read(&format!(".crane/runtime/sessions/{SESSION}/journal.jsonl"));
    assert!(session.contains("delivery_merge_failed"));
    assert!(
        repository
            .git(&["status", "--porcelain", "--untracked-files=no"])
            .lines()
            .all(|line| line.contains(".crane/")),
        "the failed merge was aborted"
    );

    // A merge made on the host without meeting the policy is never completed either
    let other = Repository::standard();
    other.deliver();
    other.git(&[
        "merge",
        "--no-ff",
        "-q",
        "-m",
        "merged by hand",
        "crane/codex-task-PAY-1830-v1",
    ]);
    let sha = other.git(&["rev-parse", "HEAD"]);
    let (ok, _, error) = other.json(&["deliver", "merged", SESSION, "--sha", &sha]);
    assert!(
        !ok && error.contains("without meeting the merge policy"),
        "{error}"
    );
    assert_eq!(other.status()["delivery_state"], "MERGE_FAILED");
    assert_eq!(count(&other.journal(), "task_completed"), 0);
}

/** stale checked commit: a commit added to the delivery branch after the checks makes the round
 * stale; approvals of the old round never count for the new one, and old Slack buttons are refused */
#[test]
fn stale_checked_commit_is_never_merged() {
    let repository = Repository::standard();
    let first = repository.deliver();
    let old_button = repository.button("approve");
    repository.crane(&["deliver", "approve", SESSION, "--approver", "payments-lead"]);
    repository.git(&["switch", "-q", "crane/codex-task-PAY-1830-v1"]);
    repository.write(
        INVOICE,
        &repository.read(INVOICE).replace(ROUNDED, "return amount;"),
    );
    repository.git(&["commit", "-qam", "unchecked change"]);
    repository.git(&["switch", "-q", "main"]);
    let status = repository.status();
    assert_eq!(status["eligibility"]["eligible"], false);
    assert!(status["eligibility"]["missing"]
        .to_string()
        .contains("the branch moved since its checks ran"));
    let (ok, _, error) = repository.json(&["deliver", "merge", SESSION]);
    assert!(!ok && error.contains("branch moved"), "{error}");

    // Delivering again checks the new commit: a new binding, and the old approval does not carry over
    let second = repository.deliver();
    assert_ne!(second["head"], first["head"]);
    assert_ne!(second["binding_digest"], first["binding_digest"]);
    assert!(second["eligibility"]["missing"]
        .to_string()
        .contains("0 of 1 required approvals"));
    let refused = repository.click("U100", "approve", old_button, SECRET);
    assert!(!refused.status.success());
    assert!(
        text(&refused.stderr).contains("earlier commit"),
        "{}",
        text(&refused.stderr)
    );
}

/** Approvals are time-bound where configured, and merge eligibility follows the policies that must
 * still hold: an approval past its lifetime no longer counts, and a task contract invalidated after
 * approval blocks the merge */
#[test]
fn approvals_expire_and_policy_must_still_hold() {
    let repository = Repository::new(json!({
        "merge_policy": {"approval_ttl_seconds": 2, "default": {"name": "default", "approvals": 1, "approvers": ["payments-lead"]}},
        "slack": {"notify": ["#shop"], "signing_secret_env": "CRANE_TEST_SLACK_SECRET", "users": {"U100": "payments-lead"}},
    }));
    repository.deliver();
    repository.crane(&["deliver", "approve", SESSION, "--approver", "payments-lead"]);
    assert_eq!(repository.status()["delivery_state"], "APPROVED");
    std::thread::sleep(std::time::Duration::from_secs(3));
    let expired = repository.status();
    assert_eq!(expired["delivery_state"], "AWAITING_APPROVAL");
    assert!(
        expired["eligibility"]["missing"]
            .to_string()
            .contains("expired"),
        "{}",
        expired["eligibility"]
    );
    let (_, renewed, _) =
        repository.json(&["deliver", "approve", SESSION, "--approver", "payments-lead"]);
    assert!(
        renewed["already_recorded"].is_null(),
        "an expired approval can be given again"
    );
    assert_eq!(renewed["delivery_state"], "APPROVED");

    // The zones change: the task contract the delivery was bound to is invalidated
    repository.write(".crane/zones/billing.zone", "zone billing {\n    criticality sensitive;\n    autonomy delegated;\n    select subsystem billing;\n}\n");
    let (ok, _, error) = repository.json(&["deliver", "merge", SESSION]);
    assert!(!ok);
    assert!(error.contains("invalidated"), "{error}");
}

/** The github provider is used through the provider abstraction: the branch is pushed, the pull
 * request is created and merged with the configured CLI, and the merge is bound to the checked
 * commit (--match-head-commit) */
#[test]
fn github_provider_goes_through_the_abstraction() {
    let folder = std::env::temp_dir().join(format!(
        "crane-gh-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        REPOSITORIES.fetch_add(1, Ordering::SeqCst)
    ));
    let remote = folder.join("remote.git");
    fs::create_dir_all(&remote).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q", "--bare", "-b", "main"])
        .current_dir(&remote)
        .status()
        .unwrap()
        .success());
    let log = folder.join("gh.log");
    let cli = if cfg!(windows) {
        let path = folder.join("gh.cmd");
        fs::write(&path, format!("@echo off\r\necho %* >> \"{}\"\r\nif \"%2\"==\"create\" echo https://github.com/acme/shop/pull/7\r\nif \"%2\"==\"merge\" git push -q origin crane/{SESSION}:main\r\nexit /b 0\r\n", log.display())).unwrap();
        vec![path.to_string_lossy().into_owned()]
    } else {
        let path = folder.join("gh.sh");
        fs::write(&path, format!("echo \"$@\" >> '{}'\nif [ \"$2\" = create ]; then echo https://github.com/acme/shop/pull/7; fi\nif [ \"$2\" = merge ]; then git push -q origin crane/{SESSION}:main; fi\n", log.display())).unwrap();
        vec!["sh".to_string(), path.to_string_lossy().into_owned()]
    };
    let repository = Repository::new(json!({
        "provider": "github",
        "github_cli": cli,
        "merge_policy": {"default": {"name": "default", "approvals": 1, "approvers": ["payments-lead"]}},
    }));
    repository.git(&["remote", "set-url", "origin", &remote.to_string_lossy()]);
    repository.git(&["push", "-q", "origin", "main"]);
    let delivered = repository.deliver();
    assert_eq!(delivered["pull_request"]["number"], 7);
    assert_eq!(delivered["pull_request"]["provider"], "github");
    assert_eq!(
        delivered["pull_request"]["url"],
        "https://github.com/acme/shop/pull/7"
    );
    repository.crane(&["deliver", "approve", SESSION, "--approver", "payments-lead"]);
    let (ok, merged, error) = repository.json(&["deliver", "merge", SESSION]);
    let calls = fs::read_to_string(&log).unwrap_or_default();
    let _ = fs::remove_dir_all(&folder);
    assert!(ok, "{error}\n{calls}");
    assert_eq!(merged["delivery_state"], "COMPLETE");
    assert!(
        calls.contains("pr create --base main --head crane/codex-task-PAY-1830-v1"),
        "{calls}"
    );
    assert!(
        calls.contains(&format!(
            "pr merge 7 --merge --match-head-commit {}",
            delivered["head"].as_str().unwrap()
        )),
        "{calls}"
    );
    assert_eq!(
        merged["merged"]["sha"], delivered["head"],
        "the host's merge is the checked commit"
    );
}
