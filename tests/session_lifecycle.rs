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

/** The payment service, in the Critical payments zone */
const PAYMENT: &str = "services/payments/src/main/java/com/acme/payments/PaymentService.java";

/** The invoice service, outside every zone */
const INVOICE: &str = "services/billing/src/main/java/com/acme/billing/InvoiceService.java";

/** The invoice change the billing task asks for */
const ROUNDED: &str = "return Math.round(amount * 100.0) / 100.0;";

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

/** A Jira issue as the REST API returns it
 * Input
    - key: &str - issue key
    - summary: &str - summary
    - description: &str - description
    - criteria: &str - acceptance criteria
 * Output
    - Value
*/
fn issue(key: &str, summary: &str, description: &str, criteria: &str) -> Value {
    json!({
        "id": "100",
        "key": key,
        "fields": {
            "summary": summary,
            "project": {"key": "PAY"},
            "priority": {"name": "High"},
            "reporter": {"displayName": "Product Manager"},
            "assignee": {"displayName": "Crane Bot"},
            "status": {"name": "To Do", "statusCategory": {"key": "new"}},
            "description": description,
            "customfield_10050": criteria,
        }
    })
}

/** A connected repository with a payments zone, a permanent policy, and two Jira tasks (a billing
 * change outside every zone, a refund change in the payments zone), removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Repository {
    root: PathBuf,
}

impl Repository {
    /** Create the fixture
     * Input
        - None
     * Output
        - Repository
    */
    fn new() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-lifecycle-{suffix}-{}",
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
            vec!["config", "user.name", "Crane Lifecycle"],
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
            assert!(Command::new("git")
                .args(&args)
                .current_dir(&repository.root)
                .output()
                .unwrap()
                .status
                .success());
        }
        repository.crane(&["init"]);
        repository.crane(&["checkpoint", "--name", "baseline"]);
        repository.write(
            ".crane/zones/org.zone",
            "zone payments {\n    criticality critical;\n    autonomy assisted;\n    select subsystem payments;\n}\n",
        );
        repository.write(
            ".crane/policies/payments_core.crane",
            "policy payments_core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n",
        );
        repository.write(
            ".crane/sources/config.json",
            r#"{"sources_format": 1, "checkpoint": "baseline", "jira": {"acceptance_field": "customfield_10050", "projects": {"PAY": {"repositories": ["acme/shop"], "team": "payments"}}}}"#,
        );
        for value in [
            issue("PAY-1821", "Reject negative refunds", "Make `PaymentService.refund` reject negative amounts without changing `PaymentService.charge`.", "refunds below zero throw"),
            issue("PAY-1830", "Round invoice totals", "Make `InvoiceService.total` round to cents.", "totals have two decimals"),
        ] {
            repository.write(
                &format!(".crane/sources/jira/issues/{}.json", value["key"].as_str().unwrap()),
                &value.to_string(),
            );
        }
        repository.crane(&["repo", "connect"]);
        repository
    }

    /** Write a file relative to the root
     * Input
        - path: &str - relative path
        - content: &str - content
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

    /** Run crane with cleared markers, extra variables, and stdin
     * Input
        - args: &[&str] - arguments
        - environment: &[(&str, &str)] - variables
        - stdin: &str - standard input
     * Output
        - Output
    */
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

    /** Run crane as a human and require success
     * Input
        - args: &[&str] - arguments
     * Output
        - String stdout
    */
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

    /** Plan and approve a task (a human, before running it)
     * Input
        - task: &str - task id
     * Output
        - None
    */
    fn approve(&self, task: &str) {
        self.crane(&["task", "prepare", task]);
        let shown: Value =
            serde_json::from_str(&self.crane(&["task", "show", task, "--json"])).unwrap();
        let digest = shown["contract"]["digest"].as_str().unwrap();
        self.crane(&[
            "task",
            "approve",
            task,
            "--approver",
            "lead",
            "--confirm",
            &digest[7..19],
        ]);
    }

    /** Run a task with an action script in one command
     * Input
        - task: &str - task id
        - actions: Value - neutral actions
        - extra: &[&str] - extra arguments
     * Output
        - (bool, Value, String) success, the JSON answer, and stderr
    */
    fn session_run(&self, task: &str, actions: Value, extra: &[&str]) -> (bool, Value, String) {
        let file = self.root.join(format!(
            "../{}-actions.json",
            self.root.file_name().unwrap().to_string_lossy()
        ));
        fs::write(&file, actions.to_string()).unwrap();
        let file = file.to_string_lossy().into_owned();
        let mut args = vec!["session", "run", task, "--actions", file.as_str(), "--json"];
        args.extend_from_slice(extra);
        let output = self.run(&args, &[], "");
        (
            output.status.success(),
            serde_json::from_slice(&output.stdout).unwrap_or(Value::Null),
            text(&output.stderr),
        )
    }

    /** Read a session's journal
     * Input
        - session: &str - session id
     * Output
        - Vec<Value>
    */
    fn journal(&self, session: &str) -> Vec<Value> {
        self.read(&format!(".crane/runtime/sessions/{session}/journal.jsonl"))
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /** Return a governed session's lifecycle record
     * Input
        - session: &str - session id
     * Output
        - Value
    */
    fn lifecycle(&self, session: &str) -> Value {
        serde_json::from_str::<Value>(&self.crane(&["session", "lifecycle", session, "--json"]))
            .unwrap()["record"]
            .clone()
    }
}

impl Drop for Repository {
    /** Remove the repository and its action script
     * Input
        - None (uses self)
     * Output
        - None
    */
    fn drop(&mut self) {
        let _ = fs::remove_file(self.root.join(format!(
            "../{}-actions.json",
            self.root.file_name().unwrap().to_string_lossy()
        )));
        let _ = fs::remove_dir_all(&self.root);
    }
}

/** Decode process output
 * Input
    - bytes: &[u8] - output
 * Output
    - String
*/
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/** List a record's phases in order
 * Input
    - record: &Value - lifecycle record
 * Output
    - Vec<String>
*/
fn phases(record: &Value) -> Vec<String> {
    let history = record["history"].as_array().unwrap();
    std::iter::once(history[0]["from"].as_str().unwrap().to_string())
        .chain(
            history
                .iter()
                .map(|entry| entry["to"].as_str().unwrap().to_string()),
        )
        .collect()
}

/** List the orchestrator outcomes of a run's actions
 * Input
    - answer: &Value - run answer
 * Output
    - Vec<String>
*/
fn outcomes(answer: &Value) -> Vec<String> {
    answer["actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|line| line["outcome"].as_str().map(String::from))
        .collect()
}

/** The billing edit the task asks for, in Crane's neutral action format */
fn rounding() -> Value {
    json!({"tool": "Edit", "operation": "write", "path": INVOICE, "edits": [{"old": "return amount * 1.0;", "new": ROUNDED}]})
}

/** Acceptance: one command launches an approved task and drives it through the whole governed
 * lifecycle: the session is created and bound, the agent's actions are decided by the authority
 * engine, executed where permitted, observed, and verified; termination freezes, reconciles the
 * repository, runs the tests, attests, and marks the session VERIFIED and DELIVERY_READY */
#[test]
fn one_command_runs_the_governed_lifecycle() {
    let repository = Repository::new();
    repository.approve("PAY-1830");
    let (ok, answer, error) = repository.session_run(
        "PAY-1830",
        json!([
            {"tool": "Read", "operation": "read", "path": INVOICE},
            rounding(),
            {"operation": "stop", "text": "Done: totals are rounded and all tests pass."},
        ]),
        &["--agent", "codex"],
    );
    assert!(ok, "{error}\n{answer}");
    let record = &answer["record"];
    assert_eq!(record["phase"], "DELIVERY_READY", "{record}");
    assert_eq!(
        phases(record),
        [
            "TASK_READY",
            "SESSION_CREATED",
            "AGENT_CONNECTED",
            "RUNNING",
            "STOPPING",
            "RECONCILING",
            "VERIFIED",
            "DELIVERY_READY"
        ]
    );
    assert_eq!(outcomes(&answer), ["ALLOW", "ALLOW"]);
    assert!(
        repository.read(INVOICE).contains(ROUNDED),
        "the permitted edit was executed"
    );
    let session = answer["session_id"].as_str().unwrap();
    assert_eq!(session, "codex-task-PAY-1830-v1");

    // The immutable binding
    let binding = &record["binding"];
    for field in [
        "repository",
        "task",
        "contract",
        "contract_digest",
        "policy_version",
        "zone_version",
        "checkpoint",
        "agent",
        "autonomy_mode",
        "safety_state",
        "budget",
        "organization",
    ] {
        assert!(!binding[field].is_null(), "{field} missing: {binding}");
    }
    assert_eq!(binding["task"], "PAY-1830");
    assert_eq!(binding["agent"], "codex");
    assert_eq!(binding["safety_state"], "active");
    assert!(record["binding_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));

    // Termination was decided from the repository
    let termination = &record["termination"];
    assert_eq!(termination["final_status"], "PASS");
    for check in termination["checks"].as_array().unwrap() {
        assert_eq!(check["passed"], true, "{check}");
    }
    assert_eq!(termination["files_changed"], 1);
    let journal = repository.journal(session);
    let claim = journal
        .iter()
        .find(|event| event["event"] == "agent_claim")
        .unwrap();
    assert_eq!(claim["trusted"], false);
    assert!(journal
        .iter()
        .any(|event| event["event"] == "post_tool_use" && event["verification"] == "pass"));
    assert!(journal
        .iter()
        .any(|event| event["event"] == "session_finalized" && event["final_status"] == "PASS"));

    // Finishing again changes nothing, and no action runs after the freeze
    let again: Value =
        serde_json::from_str(&repository.crane(&["session", "finish", session, "--json"])).unwrap();
    assert_eq!(again["record"]["already_terminated"], true);
    let late = repository.run(
        &["agent", "hook", "--event", "pre-tool-use", "--profile", "codex"],
        &[("CRANE_SESSION", session)],
        &json!({"session_id": "0199a3c2-0000-7000-8000-000000000001", "hook_event_name": "PreToolUse", "tool_name": "apply_patch", "tool_input": {"command": format!("*** Begin Patch\n*** Update File: {INVOICE}\n@@\n-        {ROUNDED}\n+        return amount;\n*** End Patch\n")}}).to_string(),
    );
    assert_eq!(late.status.code(), Some(2), "{}", text(&late.stdout));
    let human = repository.crane(&["session", "lifecycle", session]);
    assert!(human.contains("DELIVERY_READY"), "{human}");
}

/** The invariants hold during a governed run: the agent cannot change policy, session authority,
 * or the checkpoint (denied), and trying to raise its own budget or approve its own exception
 * quarantines the session, which stops the run and fails it; such a session is never delivered */
#[test]
fn agents_cannot_escalate_and_quarantine_fails_the_run() {
    let repository = Repository::new();
    repository.approve("PAY-1830");
    let checkpoint = repository.read(".crane/checkpoints/baseline.json");
    let (ok, answer, error) = repository.session_run(
        "PAY-1830",
        json!([
            {"tool": "Write", "operation": "write", "path": ".crane/policies/mine.crane", "content": "policy mine {\n    checkpoint baseline;\n}\n"},
            {"tool": "Write", "operation": "write", "path": ".crane/runtime/sessions/codex-task-PAY-1830-v1/session.json", "content": "{}"},
            {"tool": "Bash", "operation": "execute", "command": "crane checkpoint --name baseline"},
            {"tool": "Bash", "operation": "execute", "command": "crane autonomy refill codex-task-PAY-1830-v1 --amount 1000 --reason more"},
            rounding(),
        ]),
        &["--agent", "codex"],
    );
    assert!(!ok);
    assert!(error.contains("FAILED"), "{error}");
    assert_eq!(
        outcomes(&answer),
        ["DENY", "DENY", "DENY", "QUARANTINE"],
        "the run stops at the quarantine"
    );
    let record = &answer["record"];
    assert_eq!(record["phase"], "FAILED");
    assert!(phases(record).contains(&"QUARANTINED".to_string()));
    assert!(record["termination"]["failures"]
        .to_string()
        .contains("safety did not pass"));
    assert!(!repository.root.join(".crane/policies/mine.crane").exists());
    assert_eq!(
        repository.read(".crane/checkpoints/baseline.json"),
        checkpoint
    );
    assert!(
        !repository.read(INVOICE).contains(ROUNDED),
        "nothing ran after the quarantine"
    );
    let delivery = repository.run(&["deliver", "run", "codex-task-PAY-1830-v1"], &[], "");
    assert!(
        text(&delivery.stderr).contains("only a DELIVERY_READY session can be delivered"),
        "{}",
        text(&delivery.stderr)
    );

    // Approving its own exception is self-escalation too
    let other = Repository::new();
    other.approve("PAY-1830");
    let (_, exception, _) = other.session_run(
        "PAY-1830",
        json!([{"tool": "Bash", "operation": "execute", "command": "crane deliver exception codex-task-PAY-1830-v1 --path services/billing --approver me --reason mine"}, rounding()]),
        &["--agent", "codex"],
    );
    assert_eq!(outcomes(&exception), ["QUARANTINE"]);
    assert_eq!(exception["record"]["phase"], "FAILED");
}

/** A failed verification never becomes success: a session that did not make the contracted change
 * fails whatever the agent claims, finishing again or fixing the repository afterwards leaves it
 * FAILED, and the repository tests of the testing policy count */
#[test]
fn failed_verification_stays_failed() {
    let repository = Repository::new();
    repository.approve("PAY-1830");
    let (ok, answer, error) = repository.session_run(
        "PAY-1830",
        json!([
            {"tool": "Write", "operation": "write", "path": "services/billing/src/main/java/com/acme/billing/Notes.java", "content": "package com.acme.billing;\n\nclass Notes {}\n"},
            {"operation": "claim", "text": "The contract is satisfied and every test passes."},
            {"operation": "stop"},
        ]),
        &["--agent", "codex"],
    );
    assert!(!ok);
    assert!(
        error.contains("session codex-task-PAY-1830-v1 FAILED"),
        "{error}"
    );
    let record = &answer["record"];
    assert_eq!(record["phase"], "FAILED");
    assert_eq!(record["termination"]["final_status"], "FAIL");
    assert!(record["termination"]["failures"]
        .to_string()
        .contains("reconciliation did not pass"));

    // A human fixes the code afterwards: the run stays FAILED
    repository.write(
        INVOICE,
        &repository
            .read(INVOICE)
            .replace("return amount * 1.0;", ROUNDED),
    );
    let again = repository.run(
        &["session", "finish", "codex-task-PAY-1830-v1", "--json"],
        &[],
        "",
    );
    let again: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(again["record"]["phase"], "FAILED");
    assert_eq!(again["record"]["already_terminated"], true);
    // Running the task again is a new session with its own lifecycle; the failed one stays FAILED
    let rerun: Value = serde_json::from_str(&repository.crane(&[
        "session", "run", "PAY-1830", "--agent", "codex", "--detach", "--json",
    ]))
    .unwrap();
    assert_eq!(rerun["session_id"], "codex-task-PAY-1830-v1-r2");
    assert_eq!(rerun["record"]["phase"], "SESSION_CREATED");
    assert_eq!(
        repository.lifecycle("codex-task-PAY-1830-v1")["phase"],
        "FAILED"
    );

    // Repository tests required by the testing policy fail the run when they fail
    let strict = Repository::new();
    strict.write("services/billing/src/test/java/com/acme/billing/InvoiceServiceTest.java", "package com.acme.billing;\n\nclass InvoiceServiceTest {\n    @Test\n    void totals() {\n        new InvoiceService().total(1.0);\n    }\n}\n");
    strict.write(".crane/testing.json", r#"{"session_tests": "all", "suite": {"java": ["git", "rev-parse", "--verify", "refs/heads/no-such-branch"]}}"#);
    strict.approve("PAY-1830");
    let (ok, failing, _) =
        strict.session_run("PAY-1830", json!([rounding()]), &["--agent", "codex"]);
    assert!(!ok);
    let checks = &failing["record"]["termination"]["checks"];
    let repository_tests = checks
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["check"] == "repository_tests")
        .unwrap();
    assert_eq!(repository_tests["passed"], false, "{checks}");
    assert_eq!(failing["record"]["phase"], "FAILED");
}

/** Actions that need approval run only when the human running the command approves them up
 * front; the approval is evidence, and without it the contracted change is never made */
#[test]
fn approvals_come_from_the_human_running_the_command() {
    let refund = json!({"tool": "Edit", "operation": "write", "path": PAYMENT, "edits": [{"old": "        return charge(-amount);", "new": "        if (amount < 0) {\n            throw new IllegalArgumentException(\"negative\");\n        }\n        return charge(-amount);"}]});
    let waiting = Repository::new();
    waiting.approve("PAY-1821");
    let (ok, answer, _) =
        waiting.session_run("PAY-1821", json!([refund.clone()]), &["--agent", "claude"]);
    assert!(!ok);
    assert_eq!(outcomes(&answer), ["APPROVAL"]);
    assert_eq!(answer["actions"][0]["executed"], false);
    assert_eq!(
        answer["record"]["phase"], "FAILED",
        "the refund was never changed"
    );

    let approved = Repository::new();
    approved.approve("PAY-1821");
    let (ok, answer, error) = approved.session_run(
        "PAY-1821",
        json!([refund]),
        &["--agent", "claude", "--approve"],
    );
    assert!(ok, "{error}\n{answer}");
    assert_eq!(outcomes(&answer), ["APPROVAL"]);
    assert_eq!(answer["actions"][0]["executed"], true);
    assert_eq!(answer["record"]["phase"], "DELIVERY_READY");
    let inspected: Value = serde_json::from_str(&approved.crane(&[
        "session",
        "inspect",
        "claude-task-PAY-1821-v1",
        "--json",
    ]))
    .unwrap();
    assert!(inspected["evidence"]
        .to_string()
        .contains("approved_tool_call"));
}

/** A real agent process is driven by the same command: it is started with CRANE_SESSION, its
 * hooks bring every action through the orchestrator, and when it exits the session is terminated
 * and verified */
#[test]
fn agent_process_is_driven_through_its_hooks() {
    let repository = Repository::new();
    repository.approve("PAY-1830");
    let folder = repository.root.join("../").join(format!(
        "{}-agent",
        repository.root.file_name().unwrap().to_string_lossy()
    ));
    fs::create_dir_all(&folder).unwrap();
    let invoice = repository.root.join(INVOICE);
    let payload = |event: &str, extra: Value| {
        let mut value = json!({"session_id": "6e1c9d2a-1111-4222-8333-444455556666", "cwd": repository.root.to_string_lossy(), "hook_event_name": event, "transcript_path": "/home/dev/.claude/projects/shop/t.jsonl", "permission_mode": "default"});
        for (key, field) in extra.as_object().unwrap() {
            value[key] = field.clone();
        }
        value.to_string()
    };
    let edit = json!({"tool_name": "Edit", "tool_input": {"file_path": invoice.to_string_lossy(), "old_string": "return amount * 1.0;", "new_string": ROUNDED}, "tool_use_id": "toolu_1"});
    fs::write(
        folder.join("start.json"),
        payload("SessionStart", json!({"source": "startup"})),
    )
    .unwrap();
    fs::write(folder.join("pre.json"), payload("PreToolUse", edit.clone())).unwrap();
    let mut post = edit;
    post["tool_response"] = json!({"success": true});
    fs::write(folder.join("post.json"), payload("PostToolUse", post)).unwrap();
    fs::write(
        folder.join("Invoice.java"),
        repository
            .read(INVOICE)
            .replace("return amount * 1.0;", ROUNDED),
    )
    .unwrap();
    let script = if cfg!(windows) {
        let path = folder.join("agent.cmd");
        fs::write(&path, "@echo off\r\n\"%CRANE_BIN%\" agent hook --event session-start --profile claude < \"%~dp0start.json\"\r\n\"%CRANE_BIN%\" agent hook --event pre-tool-use --profile claude < \"%~dp0pre.json\"\r\nif errorlevel 1 exit /b 1\r\ncopy /Y \"%~dp0Invoice.java\" \"%CRANE_TARGET%\" > nul\r\n\"%CRANE_BIN%\" agent hook --event post-tool-use --profile claude < \"%~dp0post.json\"\r\n").unwrap();
        vec![path.to_string_lossy().into_owned()]
    } else {
        let path = folder.join("agent.sh");
        fs::write(&path, "set -e\nhere=$(dirname \"$0\")\n\"$CRANE_BIN\" agent hook --event session-start --profile claude < \"$here/start.json\"\n\"$CRANE_BIN\" agent hook --event pre-tool-use --profile claude < \"$here/pre.json\"\ncp \"$here/Invoice.java\" \"$CRANE_TARGET\"\n\"$CRANE_BIN\" agent hook --event post-tool-use --profile claude < \"$here/post.json\"\n").unwrap();
        vec!["sh".to_string(), path.to_string_lossy().into_owned()]
    };
    let mut args = vec![
        "session", "run", "PAY-1830", "--agent", "claude", "--json", "--",
    ];
    args.extend(script.iter().map(String::as_str));
    let invoice_path = invoice.to_string_lossy().into_owned();
    let output = repository.run(
        &args,
        &[
            ("CRANE_BIN", env!("CARGO_BIN_EXE_crane")),
            ("CRANE_TARGET", invoice_path.as_str()),
        ],
        "",
    );
    let _ = fs::remove_dir_all(&folder);
    assert!(
        output.status.success(),
        "{}{}",
        text(&output.stdout),
        text(&output.stderr)
    );
    let answer: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(answer["agent_process"]["exit"], 0);
    let record = &answer["record"];
    assert_eq!(record["phase"], "DELIVERY_READY", "{record}");
    assert_eq!(record["actions"]["ALLOW"], 1);
    assert_eq!(record["actions"]["executed"], 1);
    let journal = repository.journal("claude-task-PAY-1830-v1");
    assert!(journal
        .iter()
        .any(|event| event["event"] == "provider_attached" && event["via"] == "CRANE_SESSION"));
    assert!(phases(record).contains(&"AGENT_CONNECTED".to_string()));
}

/** A detached run waits for the agent; a changed lifecycle binding is caught on the next action,
 * which is denied and fails the session */
#[test]
fn a_changed_binding_fails_the_session() {
    let repository = Repository::new();
    repository.approve("PAY-1830");
    let detached = repository.crane(&[
        "session", "run", "PAY-1830", "--agent", "claude", "--detach",
    ]);
    assert!(detached.contains("SESSION_CREATED"), "{detached}");
    assert!(detached.contains("CRANE_SESSION=claude-task-PAY-1830-v1"));
    let path = repository
        .root
        .join(".crane/runtime/orchestrator/claude-task-PAY-1830-v1.json");
    let mut record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    record["binding"]["autonomy_mode"] = json!("autonomous");
    fs::write(&path, record.to_string()).unwrap();
    let edit = json!({"session_id": "6e1c9d2a-1111-4222-8333-444455556666", "hook_event_name": "PreToolUse", "tool_name": "Edit", "tool_input": {"file_path": repository.root.join(INVOICE).to_string_lossy(), "old_string": "return amount * 1.0;", "new_string": ROUNDED}});
    let denied = repository.run(
        &[
            "agent",
            "hook",
            "--event",
            "pre-tool-use",
            "--profile",
            "claude",
        ],
        &[("CRANE_SESSION", "claude-task-PAY-1830-v1")],
        &edit.to_string(),
    );
    assert_eq!(denied.status.code(), Some(2));
    assert!(
        text(&denied.stderr).contains("lifecycle binding"),
        "{}",
        text(&denied.stderr)
    );
    assert_eq!(
        repository.lifecycle("claude-task-PAY-1830-v1")["phase"],
        "FAILED"
    );
}

/** Agents can neither run nor finish governed sessions; the dashboard API runs a task in one call */
#[test]
fn agents_cannot_run_sessions_and_the_api_can() {
    let repository = Repository::new();
    repository.approve("PAY-1830");
    for args in [
        vec!["session", "run", "PAY-1830", "--agent", "codex", "--detach"],
        vec!["session", "finish", "codex-task-PAY-1830-v1"],
    ] {
        let output = repository.run(&args, &[("CODEX_SANDBOX", "seatbelt")], "");
        assert!(!output.status.success());
        assert!(
            text(&output.stderr).contains("refuses to run in an agent environment"),
            "{}",
            text(&output.stderr)
        );
    }
    let body = json!({"task": "PAY-1830", "agent": "codex", "actions": [rounding()], "by": "lead"})
        .to_string();
    let output = repository.run(
        &[
            "dashboard",
            "api",
            "POST",
            "/api/sessions/run",
            "--body",
            &body,
        ],
        &[],
        "",
    );
    assert!(
        output.status.success(),
        "{}{}",
        text(&output.stdout),
        text(&output.stderr)
    );
    let answer: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(answer["record"]["phase"], "DELIVERY_READY");
    let lifecycle = repository.run(
        &[
            "dashboard",
            "api",
            "GET",
            "/api/sessions/codex-task-PAY-1830-v1/lifecycle",
        ],
        &[],
        "",
    );
    let lifecycle: Value = serde_json::from_slice(&lifecycle.stdout).unwrap();
    assert_eq!(lifecycle["phase"], "DELIVERY_READY");
}
