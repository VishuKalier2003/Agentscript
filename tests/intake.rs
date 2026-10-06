use std::fs;
use std::io::Write;
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

/** The payment service */
const PAYMENT: &str = "services/payments/src/main/java/com/acme/payments/PaymentService.java";

/** Repository files: a payments service with a test, and a billing service */
const FILES: &[(&str, &str)] = &[
    ("services/payments/pom.xml", "<project/>\n"),
    (
        PAYMENT,
        "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    public int refund(int amount) {\n        return charge(-amount);\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n",
    ),
    (
        "services/payments/src/test/java/com/acme/payments/PaymentServiceTest.java",
        "package com.acme.payments;\n\nclass PaymentServiceTest {\n    @Test\n    void charges() {\n        new PaymentService().charge(5);\n    }\n}\n",
    ),
    ("services/billing/pom.xml", "<project/>\n"),
    (
        "services/billing/src/main/java/com/acme/billing/InvoiceService.java",
        "package com.acme.billing;\n\npublic class InvoiceService {\n    public double total(double amount) {\n        return amount * 1.0;\n    }\n}\n",
    ),
];

/** Source settings: Jira projects PAY (this repository) and OPS (another one) */
const CONFIG: &str = r#"{
  "sources_format": 1,
  "checkpoint": "baseline",
  "jira": {
    "acceptance_field": "customfield_10050",
    "projects": {
      "PAY": {"repositories": ["acme/shop"], "team": "payments"},
      "OPS": {"repositories": ["acme/ops"], "team": "ops"}
    }
  }
}"#;

/** A Jira issue as the REST API returns it
 * Input
    - key: &str - issue key
    - summary: &str - summary
    - description: &str - description (wiki text)
    - criteria: &[&str] - acceptance criteria field
    - done: bool - status category done
 * Output
    - Value
*/
fn issue(key: &str, summary: &str, description: &str, criteria: &[&str], done: bool) -> Value {
    json!({
        "id": "100",
        "key": key,
        "fields": {
            "summary": summary,
            "project": {"key": key.split('-').next().unwrap()},
            "priority": {"name": "High"},
            "labels": ["payments"],
            "reporter": {"displayName": "Product Manager", "emailAddress": "pm@example.com"},
            "assignee": {"displayName": "Crane Bot", "accountId": "557058:crane-bot"},
            "status": {"name": if done { "Done" } else { "To Do" }, "statusCategory": {"key": if done { "done" } else { "new" }}},
            "description": description,
            "customfield_10050": criteria.join("\n"),
        }
    })
}

/** A connected repository named shop, with Crane initialized, a baseline checkpoint, a payments
 * zone, a permanent policy, the source settings, and Jira issue snapshots, removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Repository {
    root: PathBuf,
}

impl Repository {
    /** Create the fixture repository
     * Input
        - connect: bool - connect it
     * Output
        - Repository
    */
    fn new(connect: bool) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-intake-{suffix}-{}",
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
            vec!["config", "user.name", "Crane Intake"],
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
        repository.write(
            ".crane/zones/org.zone",
            "zone payments {\n    criticality critical;\n    autonomy assisted;\n    select subsystem payments;\n}\n",
        );
        repository.write(
            ".crane/policies/payments_core.crane",
            "policy payments_core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n",
        );
        repository.write(".crane/sources/config.json", CONFIG);
        repository.issue(issue(
            "PAY-1821",
            "Reject negative refunds",
            "Make `PaymentService.refund` reject negative amounts without changing `PaymentService.charge`.",
            &["`PaymentService.refund` throws for amounts below zero"],
            false,
        ));
        repository.issue(issue(
            "PAY-1840",
            "Payments are slow",
            "Customers say payments take too long. Please improve things.",
            &[],
            false,
        ));
        repository.issue(issue(
            "PAY-1700",
            "Old refund fix",
            "Fix `PaymentService.refund`.",
            &[],
            true,
        ));
        repository.issue(issue(
            "OPS-7",
            "Rotate certificates",
            "Rotate `Certs.rotate`.",
            &[],
            false,
        ));
        if connect {
            repository.crane(&["repo", "connect"]);
        }
        repository
    }

    /** Write a file relative to the root, creating parent folders
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

    /** Store a Jira issue snapshot in .crane/sources/jira/issues
     * Input
        - issue: Value - issue
     * Output
        - None
    */
    fn issue(&self, issue: Value) {
        self.write(
            &format!(
                ".crane/sources/jira/issues/{}.json",
                issue["key"].as_str().unwrap()
            ),
            &issue.to_string(),
        );
    }

    /** Run git and require success
     * Input
        - args: &[&str] - arguments
     * Output
        - String stdout
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
        text(&output.stdout)
    }

    /** Run crane with agent markers removed, extra variables, and stdin
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
        for marker in AGENT_MARKERS {
            command.env_remove(marker);
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

    /** Run crane with --json as a human, returning success, the answer (Null without JSON), and
     * stderr
     * Input
        - args: &[&str] - arguments
     * Output
        - (bool, Value, String)
    */
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

    /** Show a task
     * Input
        - task: &str - task id
     * Output
        - Value
    */
    fn show(&self, task: &str) -> Value {
        let (ok, value, error) = self.json(&["task", "show", task]);
        assert!(ok, "{error}");
        value
    }

    /** Approve a task's contract, quoting its digest
     * Input
        - task: &str - task id
     * Output
        - (bool, Value, String)
    */
    fn approve(&self, task: &str) -> (bool, Value, String) {
        let digest = self.show(task)["contract"]["digest"]
            .as_str()
            .unwrap()
            .to_string();
        self.json(&[
            "task",
            "approve",
            task,
            "--approver",
            "payments-lead",
            "--confirm",
            &digest[7..19],
        ])
    }

    /** Send a Claude hook event
     * Input
        - event: &str - hook event
        - session: &str - provider session id
        - payload: Value - fields
     * Output
        - Output
    */
    fn hook(&self, event: &str, session: &str, mut payload: Value) -> Output {
        payload["session_id"] = json!(session);
        self.run(
            &["agent", "hook", "--event", event, "--profile", "claude"],
            &[],
            &payload.to_string(),
        )
    }

    /** Ask the hook about an edit of the payment service
     * Input
        - session: &str - provider session id
     * Output
        - Output
    */
    fn edit_refund(&self, session: &str) -> Output {
        self.hook("pre-tool-use", session, json!({"tool_name": "Edit", "tool_input": {"file_path": self.root.join(PAYMENT).to_string_lossy(), "old_string": "return charge(-amount);", "new_string": "return charge(-Math.abs(amount));"}}))
    }
}

impl Drop for Repository {
    /** Remove the temporary repository
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

/** Return a listed task by id
 * Input
    - list: &Value - task list
    - id: &str - task id
 * Output
    - Value
*/
fn listed(list: &Value, id: &str) -> Value {
    list["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["task_id"] == id)
        .cloned()
        .unwrap_or(Value::Null)
}

/** Acceptance: a user selects a Jira task of the connected repository, Society fetches and
 * validates it, compiles its contract, a human approves it, the user picks Claude, and the task
 * arrives at READY TO RUN with an immutable, verified contract/session binding; repeating any step
 * changes nothing */
#[test]
fn jira_task_reaches_ready_to_run() {
    let repository = Repository::new(true);
    let (ok, list, error) = repository.json(&["task", "list"]);
    assert!(ok, "{error}");
    assert_eq!(listed(&list, "PAY-1821")["state"], "AVAILABLE");
    assert_eq!(listed(&list, "PAY-1821")["source"], "jira");
    assert_eq!(listed(&list, "PAY-1700")["state"], "COMPLETED");
    assert_eq!(
        listed(&list, "OPS-7"),
        Value::Null,
        "another repository's task"
    );
    assert_eq!(list["unmapped"][0]["task_id"], "OPS-7");

    let shown = repository.show("PAY-1821");
    assert_eq!(shown["normalized"]["title"], "Reject negative refunds");
    assert_eq!(shown["normalized"]["repositories"], json!(["acme/shop"]));
    assert_eq!(shown["normalized"]["team"], "payments");
    assert!(shown["assignees"].to_string().contains("Crane Bot"));

    let (ok, planned, error) = repository.json(&["task", "prepare", "PAY-1821"]);
    assert!(ok, "{error}");
    assert_eq!(planned["state"], "CONTRACT_PENDING_APPROVAL");
    assert_eq!(planned["contract"]["contract_id"], "PAY-1821@v1");
    assert_eq!(
        planned["task_contract"]["contract"]["MUST_CHANGE"][0]["qualified"],
        "PaymentService.refund"
    );
    let early = repository.run(
        &["task", "launch", "PAY-1821", "--agent", "claude"],
        &[],
        "",
    );
    assert!(
        text(&early.stderr).contains("is not approved yet"),
        "{}",
        text(&early.stderr)
    );

    let (ok, approved, error) = repository.approve("PAY-1821");
    assert!(ok, "{error}");
    assert_eq!(approved["state"], "READY");
    assert_eq!(approved["ready_to_run"], false);
    let missing = repository.run(&["task", "launch", "PAY-1821"], &[], "");
    assert!(text(&missing.stderr).contains("requires --agent claude|codex|generic"));

    let (ok, launched, error) =
        repository.json(&["task", "launch", "PAY-1821", "--agent", "claude"]);
    assert!(ok, "{error}");
    assert_eq!(launched["launched"], true);
    assert_eq!(launched["state"], "READY");
    assert_eq!(launched["ready_to_run"], true);
    assert!(launched["reason"]
        .as_str()
        .unwrap()
        .starts_with("READY TO RUN"));
    assert_eq!(launched["agent"], "claude");
    assert_eq!(launched["session"]["session_id"], "claude-task-PAY-1821-v1");
    assert_eq!(launched["launch_verified"], "intact");
    let human = repository.crane(&["task", "show", "PAY-1821"]);
    assert!(human.contains("READY TO RUN"), "{human}");

    // The binding
    let binding = &launched["launch"]["binding"];
    let contract = &launched["task_contract"];
    assert_eq!(binding["task_id"], "PAY-1821");
    assert_eq!(binding["contract"]["digest"], contract["digest"]);
    assert_eq!(binding["repository"]["name"], "shop");
    assert!(binding["repository"]["identity"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(binding["checkpoint"]["name"], "baseline");
    assert_eq!(
        binding["checkpoint"]["sha"],
        repository.git(&["rev-parse", "HEAD"]).trim()
    );
    assert_eq!(binding["agent"], "claude");
    assert_eq!(binding["autonomy"]["mode"], "delegated");
    assert_eq!(binding["autonomy"]["safety"], "active");
    assert_eq!(binding["budget"]["mutating_actions"], 300);
    assert!(binding["budget"]["model_version"].is_string());
    assert_eq!(
        binding["zones"]["zone_set_version"],
        contract["bindings"]["zone_set_version"]
    );
    assert_eq!(binding["zones"]["zones"][0]["zone_id"], "payments");
    assert_eq!(
        binding["policy_version"],
        contract["bindings"]["policy_version"]
    );
    assert_eq!(binding["task_contract"]["status"], "approved");
    let session: Value = serde_json::from_str(&repository.crane(&[
        "agent",
        "session",
        "show",
        "claude-task-PAY-1821-v1",
    ]))
    .unwrap();
    assert_eq!(session["task_id"], "PAY-1821");
    assert_eq!(
        session["governance"]["task_contract"]["digest"],
        contract["digest"]
    );
    assert_eq!(
        session["binding_digest"],
        binding["session"]["binding_digest"]
    );

    // Idempotent: launching, planning, and approving again change nothing
    let (_, again, _) = repository.json(&["task", "launch", "PAY-1821", "--agent", "claude"]);
    assert_eq!(again["launched"], false);
    assert_eq!(
        again["launch"]["binding_digest"],
        launched["launch"]["binding_digest"]
    );
    let other = repository.run(&["task", "launch", "PAY-1821", "--agent", "codex"], &[], "");
    assert!(text(&other.stderr)
        .contains("already launched with claude as session claude-task-PAY-1821-v1"));
    let (_, replanned, _) = repository.json(&["task", "prepare", "PAY-1821"]);
    assert_eq!(replanned["contract"]["contract_id"], "PAY-1821@v1");
    assert_eq!(replanned["ready_to_run"], true);
    let (_, reapproved, _) = repository.approve("PAY-1821");
    assert_eq!(reapproved["already_approved"], true);
    let sessions = repository.crane(&["agent", "session", "list"]);
    assert_eq!(sessions.matches("task-PAY-1821").count(), 1, "{sessions}");
    let history = repository.show("PAY-1821")["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["to"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        history,
        [
            "PLANNING",
            "CONTRACT_PENDING_APPROVAL",
            "READY",
            "PLANNING",
            "READY"
        ],
        "planning again is recorded, and changes nothing else"
    );

    // The agent connects: the task is running
    let started = repository.hook(
        "session-start",
        "task-PAY-1821-v1",
        json!({"source": "startup"}),
    );
    assert!(started.status.success(), "{}", text(&started.stderr));
    assert_eq!(repository.show("PAY-1821")["state"], "RUNNING");

    // The binding is immutable: a changed record no longer verifies
    let path = repository.root.join(".crane/runtime/intake/PAY-1821.json");
    let mut record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    record["launch"]["binding"]["autonomy"]["mode"] = json!("autonomous");
    fs::write(&path, record.to_string()).unwrap();
    let tampered = repository.show("PAY-1821");
    assert_eq!(tampered["state"], "BLOCKED");
    assert!(tampered["launch_verified"]
        .as_str()
        .unwrap()
        .contains("no longer matches"));
}

/** A vague Jira task ends in NEEDS_CLARIFICATION and can never launch; listing needs a connected
 * repository */
#[test]
fn vague_tasks_need_clarification() {
    let unconnected = Repository::new(false);
    let refused = unconnected.run(&["task", "list"], &[], "");
    assert!(text(&refused.stderr).contains("connect the repository first"));

    let repository = Repository::new(true);
    let (ok, value, error) = repository.json(&["task", "prepare", "PAY-1840"]);
    assert!(!ok);
    assert!(error.contains("needs clarification"), "{error}");
    assert_eq!(value["state"], "NEEDS_CLARIFICATION");
    assert!(value["reason"]
        .as_str()
        .unwrap()
        .contains("no function or class that must change"));
    assert_eq!(value["task_contract"]["authority"]["granted"], false);
    let launch = repository.run(
        &["task", "launch", "PAY-1840", "--agent", "claude"],
        &[],
        "",
    );
    assert!(text(&launch.stderr).contains("needs clarification; no session can start"));
    let unmapped = repository.run(&["task", "prepare", "OPS-7"], &[], "");
    assert!(text(&unmapped.stderr).contains("is not mapped to this repository"));

    // The tracker is the source of truth: a clarified ticket plans into a contract
    repository.issue(issue(
        "PAY-1840",
        "Payments are slow",
        "Make `PaymentService.fee` cache its result.",
        &["fees are computed once"],
        false,
    ));
    let (ok, clarified, error) = repository.json(&["task", "prepare", "PAY-1840"]);
    assert!(ok, "{error}");
    assert_eq!(clarified["state"], "CONTRACT_PENDING_APPROVAL");
    assert_eq!(clarified["contract"]["contract_id"], "PAY-1840@v2");
}

/** No session starts against an obsolete contract: a ticket changed in Jira, a newer version, or
 * an invalidated contract refuse the launch, and a launched session of an obsolete contract can no
 * longer change anything */
#[test]
fn obsolete_contracts_never_launch() {
    let repository = Repository::new(true);
    repository.crane(&["task", "prepare", "PAY-1821"]);
    assert!(repository.approve("PAY-1821").0);

    // The ticket changed in Jira after it was planned
    repository.issue(issue(
        "PAY-1821",
        "Reject negative refunds",
        "Make `PaymentService.refund` reject negative and zero amounts without changing `PaymentService.charge`.",
        &["`PaymentService.refund` throws for amounts below one"],
        false,
    ));
    let changed = repository.run(
        &["task", "launch", "PAY-1821", "--agent", "claude"],
        &[],
        "",
    );
    assert!(
        text(&changed.stderr)
            .contains("is obsolete: task PAY-1821 changed in jira since it was planned"),
        "{}",
        text(&changed.stderr)
    );
    let (_, second, _) = repository.json(&["task", "prepare", "PAY-1821"]);
    assert_eq!(second["state"], "CONTRACT_PENDING_APPROVAL");
    assert_eq!(second["contract"]["contract_id"], "PAY-1821@v2");
    let pending = repository.run(
        &["task", "launch", "PAY-1821", "--agent", "claude"],
        &[],
        "",
    );
    assert!(
        text(&pending.stderr).contains("PAY-1821@v2 is not approved yet"),
        "{}",
        text(&pending.stderr)
    );
    assert!(repository.approve("PAY-1821").0);
    let (ok, launched, error) =
        repository.json(&["task", "launch", "PAY-1821", "--agent", "codex"]);
    assert!(ok, "{error}");
    assert_eq!(launched["session"]["session_id"], "codex-task-PAY-1821-v2");
    assert_eq!(
        launched["launch"]["binding"]["contract"]["contract_id"],
        "PAY-1821@v2"
    );

    // Zones change under the launched session: its contract is invalidated
    repository.write(
        ".crane/zones/billing.zone",
        "zone billing {\n    criticality sensitive;\n    autonomy delegated;\n    select subsystem billing;\n}\n",
    );
    let codex = repository.run(
        &["agent", "hook", "--event", "pre-tool-use", "--profile", "codex"],
        &[],
        &json!({"session_id": "task-PAY-1821-v2", "hook_event_name": "PreToolUse", "tool_name": "apply_patch", "tool_input": {"command": format!("*** Begin Patch\n*** Update File: {PAYMENT}\n@@\n-        return charge(-amount);\n+        return charge(-Math.abs(amount));\n*** End Patch\n")}}).to_string(),
    );
    assert_eq!(
        codex.status.code(),
        Some(2),
        "{}{}",
        text(&codex.stdout),
        text(&codex.stderr)
    );
    assert!(
        text(&codex.stdout).contains("is invalidated")
            || text(&codex.stderr).contains("is invalidated"),
        "{}{}",
        text(&codex.stdout),
        text(&codex.stderr)
    );
    let blocked = repository.show("PAY-1821");
    assert_eq!(blocked["state"], "BLOCKED");
    assert!(blocked["reason"]
        .as_str()
        .unwrap()
        .contains("zones changed"));
    let refused = repository.run(&["task", "launch", "PAY-1821", "--agent", "codex"], &[], "");
    assert!(
        text(&refused.stderr).contains("PAY-1821@v2 is obsolete: zones changed"),
        "{}",
        text(&refused.stderr)
    );
}

/** A session launched for a contract version loses its authority once a newer version is
 * approved */
#[test]
fn superseded_sessions_lose_authority() {
    let repository = Repository::new(true);
    repository.crane(&["task", "prepare", "PAY-1821"]);
    assert!(repository.approve("PAY-1821").0);
    repository.crane(&["task", "launch", "PAY-1821", "--agent", "claude"]);
    let before = repository.edit_refund("task-PAY-1821-v1");
    assert!(before.status.success(), "{}", text(&before.stderr));
    repository.issue(issue(
        "PAY-1821",
        "Reject negative refunds",
        "Make `PaymentService.refund` reject negative amounts and log them without changing `PaymentService.charge`.",
        &["`PaymentService.refund` throws for amounts below zero"],
        false,
    ));
    repository.crane(&["task", "prepare", "PAY-1821"]);
    assert!(
        repository.edit_refund("task-PAY-1821-v1").status.success(),
        "v1 holds until v2 is approved"
    );
    assert!(repository.approve("PAY-1821").0);
    let after = repository.edit_refund("task-PAY-1821-v1");
    assert_eq!(after.status.code(), Some(2));
    assert!(
        text(&after.stderr).contains("task contract PAY-1821@v1 is superseded"),
        "{}",
        text(&after.stderr)
    );
    assert_eq!(
        repository.show("PAY-1821")["ready_to_run"],
        false,
        "v2 is ready to launch"
    );
}

/** Agents can neither plan, approve, nor launch tasks, directly or through their shell */
#[test]
fn agents_cannot_drive_intake() {
    let repository = Repository::new(true);
    let agent = [("CLAUDECODE", "1")];
    for args in [
        vec!["task", "prepare", "PAY-1821"],
        vec![
            "task",
            "approve",
            "PAY-1821",
            "--approver",
            "me",
            "--confirm",
            "000000000000",
        ],
        vec!["task", "launch", "PAY-1821", "--agent", "claude"],
    ] {
        let output = repository.run(&args, &agent, "");
        assert!(!output.status.success());
        assert!(
            text(&output.stderr).contains("refuses to run in an agent environment"),
            "{}",
            text(&output.stderr)
        );
    }
    assert!(
        repository
            .run(&["task", "list"], &agent, "")
            .status
            .success(),
        "agents may read"
    );
    repository.crane(&[
        "agent",
        "session",
        "start",
        "--profile",
        "claude",
        "--session",
        "a1",
    ]);
    let shell = repository.hook("pre-tool-use", "a1", json!({"tool_name": "Bash", "tool_input": {"command": "crane task launch PAY-1821 --agent claude"}}));
    assert_eq!(shell.status.code(), Some(2));
    let (_, status, _) = repository.json(&["autonomy", "status", "claude-a1"]);
    assert_eq!(status["safety"], "quarantined");
}

/** The dashboard exposes each task with its status, contract, selected agent, checkpoint, and
 * session, and drives the same path */
#[test]
fn dashboard_tasks_api() {
    let repository = Repository::new(true);
    let api = |method: &str, path: &str, body: Option<Value>| -> (bool, Value) {
        let body = body.map(|body| body.to_string());
        let mut args = vec!["dashboard", "api", method, path];
        if let Some(body) = &body {
            args.extend(["--body", body.as_str()]);
        }
        let output = repository.run(&args, &[], "");
        (
            output.status.success(),
            serde_json::from_slice(&output.stdout).unwrap(),
        )
    };
    let (ok, planned) = api(
        "POST",
        "/api/tasks/PAY-1821/prepare",
        Some(json!({"by": "lead"})),
    );
    assert!(ok, "{planned}");
    let confirm = planned["contract"]["digest"].as_str().unwrap()[7..19].to_string();
    let (ok, approved) = api(
        "POST",
        "/api/tasks/PAY-1821/approve",
        Some(json!({"approver": "lead", "confirm": confirm})),
    );
    assert!(ok, "{approved}");
    let (ok, launched) = api(
        "POST",
        "/api/tasks/PAY-1821/launch",
        Some(json!({"agent": "claude"})),
    );
    assert!(ok, "{launched}");
    let (_, list) = api("GET", "/api/tasks", None);
    assert_eq!(
        list,
        repository.json(&["task", "list"]).1,
        "the dashboard and the CLI agree"
    );
    let task = listed(&list, "PAY-1821");
    for field in [
        "task_id",
        "state",
        "contract",
        "agent",
        "checkpoint",
        "session",
    ] {
        assert!(!task[field].is_null(), "{field} missing: {task}");
    }
    assert_eq!(task["ready_to_run"], true);
    assert_eq!(task["checkpoint"]["name"], "baseline");
    assert_eq!(task["session"]["lifecycle"], "active");
    let (_, detail) = api("GET", "/api/tasks/PAY-1821", None);
    assert_eq!(detail["launch_verified"], "intact");
    let (_, screens) = api("GET", "/api/screens", None);
    assert!(screens.to_string().contains("\"Tasks\""));
    let (missing, _) = api("GET", "/api/tasks/NOPE-1", None);
    assert!(!missing);
}
