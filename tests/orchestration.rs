use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
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

/** Path of the payment service */
const SERVICE: &str = "services/payments/src/main/java/com/acme/payments/PaymentService.java";

/** Payment service source */
const PAYMENT: &str = "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    public int refund(int amount) {\n        return charge(-amount);\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n";

/** Repository files: payments and billing services */
const FILES: &[(&str, &str)] = &[
    ("README.md", "# Shop\n"),
    ("services/payments/pom.xml", "<project/>\n"),
    (SERVICE, PAYMENT),
    ("services/billing/pom.xml", "<project/>\n"),
    (
        "services/billing/src/main/java/com/acme/billing/InvoiceService.java",
        "package com.acme.billing;\n\npublic class InvoiceService {\n    public double total(double amount) {\n        return amount * 1.0;\n    }\n\n    public double issue(double amount) {\n        return total(amount);\n    }\n\n    public int cancel(int amount) {\n        return refund(amount);\n    }\n}\n",
    ),
];

/** A permanent organization policy: charge never changes */
const POLICY: &str = "policy payments_core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n";

/** A temporary repository named shop with Crane initialized, a baseline checkpoint, a permanent
 * policy, and the orchestration configuration, removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Repository {
    root: PathBuf,
}

/** Return the fixture directory
 * Input
    - None
 * Output
    - PathBuf
*/
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/orchestration")
}

impl Repository {
    /** Create the fixture repository
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
            "crane-orchestration-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Self { root };
        for (path, content) in FILES {
            repository.write(path, content);
        }
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "lead@example.com"],
            vec!["config", "user.name", "Crane Orchestration Test"],
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
            let output = Command::new("git")
                .args(&args)
                .current_dir(&repository.root)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}");
        }
        assert!(repository.crane(&["init"], &[], "").status.success());
        assert!(repository
            .crane(&["checkpoint", "--name", "baseline"], &[], "")
            .status
            .success());
        repository.write(".crane/policies/payments_core.crane", POLICY);
        repository.write(
            ".crane/sources/config.json",
            &fs::read_to_string(fixtures().join("config.json")).unwrap(),
        );
        repository.write(
            ".crane/sources/asana/tasks/1208100000000001.json",
            &fs::read_to_string(fixtures().join("asana/tasks/1208100000000001.json")).unwrap(),
        );
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

    /** Run crane with agent markers removed, extra variables set, and stdin
     * Input
        - args: &[&str] - crane arguments
        - environment: &[(&str, &str)] - variables to set
        - stdin: &str - text written to stdin
     * Output
        - Output
    */
    fn crane(&self, args: &[&str], environment: &[(&str, &str)], stdin: &str) -> Output {
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
        let mut child = command.spawn().expect("crane should execute");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /** Replay fixture deliveries and return the outcomes
     * Input
        - source: &str - jira or asana
        - files: &[&str] - fixture files relative to the source's fixture folder
        - extra: &[&str] - extra arguments such as --delivery
     * Output
        - Vec<Value> outcomes
    */
    fn ingest(&self, source: &str, files: &[&str], extra: &[&str]) -> Vec<Value> {
        let paths = files
            .iter()
            .map(|file| {
                fixtures()
                    .join(source)
                    .join(file)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        let mut args = vec!["task", "ingest", "--source", source, "--json"];
        args.extend_from_slice(extra);
        args.extend(paths.iter().map(String::as_str));
        let output = self.crane(&args, &[], "");
        assert!(
            output.status.success(),
            "ingest failed: {}",
            text(&output.stderr)
        );
        serde_json::from_slice::<Vec<Value>>(&output.stdout).unwrap()
    }

    /** Read one task record
     * Input
        - id: &str - task id
     * Output
        - Value
    */
    fn record(&self, id: &str) -> Value {
        let output = self.crane(&["task", "status", id, "--json"], &[], "");
        assert!(output.status.success(), "{}", text(&output.stderr));
        serde_json::from_slice::<Vec<Value>>(&output.stdout)
            .unwrap()
            .remove(0)
    }

    /** Synchronize one task and return its record
     * Input
        - id: &str - task id
     * Output
        - Value
    */
    fn sync(&self, id: &str) -> Value {
        let output = self.crane(&["task", "sync", id, "--json"], &[], "");
        assert!(output.status.success(), "{}", text(&output.stderr));
        serde_json::from_slice::<Vec<Value>>(&output.stdout)
            .unwrap()
            .remove(0)
    }

    /** Approve a proposal as a human, quoting its digest
     * Input
        - name: &str - proposal id
     * Output
        - None (panics if approval fails)
    */
    fn approve(&self, name: &str) {
        let shown: Value = serde_json::from_slice(
            &self
                .crane(&["policy", "show", name, "--json"], &[], "")
                .stdout,
        )
        .unwrap();
        let digest = shown["policy_digest"].as_str().unwrap()[7..19].to_string();
        let output = self.crane(
            &[
                "policy",
                "approve",
                name,
                "--approver",
                "lead",
                "--confirm",
                &digest,
            ],
            &[],
            "",
        );
        assert!(output.status.success(), "{}", text(&output.stderr));
    }

    /** Send a generic agent hook event for a session
     * Input
        - event: &str - hook event
        - payload: Value - neutral action JSON (session_id is added)
        - session: &str - provider session id
     * Output
        - Output
    */
    fn hook(&self, event: &str, mut payload: Value, session: &str) -> Output {
        payload["session_id"] = json!(session);
        self.crane(
            &["agent", "hook", "--event", event, "--profile", "generic"],
            &[],
            &payload.to_string(),
        )
    }

    /** Read a proposal's status
     * Input
        - name: &str - proposal id
     * Output
        - String
    */
    fn proposal_status(&self, name: &str) -> String {
        let shown: Value = serde_json::from_slice(
            &self
                .crane(&["policy", "show", name, "--json"], &[], "")
                .stdout,
        )
        .unwrap();
        shown["status"].as_str().unwrap_or_default().to_string()
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

/** List the states a record passed through
 * Input
    - record: &Value - task record
 * Output
    - Vec<String>
*/
fn states(record: &Value) -> Vec<String> {
    record["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["to"].as_str().unwrap().to_string())
        .collect()
}

/** An edit of refund that makes it reject negative amounts */
fn refund_edit() -> Value {
    json!({"tool": "Edit", "operation": "write", "path": SERVICE,
        "edits": [{"old": "        return charge(-amount);", "new": "        if (amount < 0) {\n            throw new IllegalArgumentException();\n        }\n        return charge(-amount);"}]})
}

/** A Jira ticket runs the whole lifecycle: received, analyzed, contract proposed, approved by a
 * human, executed in one contract session (never two), validated, ready for review, merged, and
 * completed when closed, which stops the session and retires the task contract
 */
#[test]
fn jira_ticket_runs_the_whole_lifecycle() {
    let repository = Repository::new();
    let outcome = repository.ingest("jira", &["01-created-PAY-1821.json"], &[]);
    assert_eq!(outcome[0]["result"], "proposed task_pay_1821_v1");
    assert_eq!(outcome[0]["state"], "CONTRACT_PROPOSED");
    let task: Value = serde_json::from_str(
        &fs::read_to_string(repository.root.join(".crane/tasks/PAY-1821.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(task["title"], "Reject negative refunds");
    assert_eq!(task["description"], "Make `PaymentService.refund` reject negative amounts without changing `PaymentService.charge`.");
    assert_eq!(
        task["acceptance_criteria"],
        json!(["`PaymentService.refund` throws for amounts below zero"])
    );
    assert_eq!(task["repositories"], json!(["acme/shop"]));
    assert_eq!(task["team"], "payments");
    assert_eq!(task["priority"], "High");
    assert_eq!(task["requester"], "pm@example.com");
    assert_eq!(task["labels"], json!(["payments", "component:refunds"]));
    assert_eq!(repository.proposal_status("task_pay_1821_v1"), "pending");

    // Replaying the same delivery changes nothing
    let replay = repository.ingest("jira", &["01-created-PAY-1821.json"], &[]);
    assert_eq!(replay[0]["result"], "duplicate event ignored");
    assert_eq!(repository.record("PAY-1821")["contract_version"], 1);
    assert_eq!(
        repository.sync("PAY-1821")["state"],
        "CONTRACT_PROPOSED",
        "nothing happens before approval"
    );

    // A human approves; the session starts once, however often the task is synchronized
    repository.approve("task_pay_1821_v1");
    let executing = repository.sync("PAY-1821");
    assert_eq!(executing["state"], "EXECUTING");
    assert_eq!(executing["sessions"], json!(["generic-task-PAY-1821-v1"]));
    assert_eq!(
        repository.sync("PAY-1821")["sessions"],
        json!(["generic-task-PAY-1821-v1"])
    );
    let session: Value = serde_json::from_slice(
        &repository
            .crane(
                &["agent", "session", "show", "generic-task-PAY-1821-v1"],
                &[],
                "",
            )
            .stdout,
    )
    .unwrap();
    assert_eq!(session["task_id"], "PAY-1821");

    // The agent stops before finishing: the contract is not satisfied, so it keeps executing
    let early = repository.hook("stop", json!({}), "task-PAY-1821-v1");
    assert_eq!(
        early.status.code(),
        Some(2),
        "the unmet target blocks the first stop"
    );
    let unfinished = repository.sync("PAY-1821");
    assert_eq!(unfinished["state"], "EXECUTING");
    assert!(states(&unfinished).contains(&"VALIDATING".to_string()));

    // The agent does the work and stops again: the contract passes
    let edit = repository.hook("pre-tool-use", refund_edit(), "task-PAY-1821-v1");
    assert!(edit.status.success(), "{}", text(&edit.stdout));
    let source = fs::read_to_string(repository.root.join(SERVICE)).unwrap();
    repository.write(SERVICE, &source.replace("        return charge(-amount);", "        if (amount < 0) {\n            throw new IllegalArgumentException();\n        }\n        return charge(-amount);"));
    repository.hook("stop", json!({}), "task-PAY-1821-v1");
    assert_eq!(repository.sync("PAY-1821")["state"], "PR_READY");

    // Review and merge are human or CI steps; closing the ticket completes the task
    for state in ["REVIEW", "MERGED"] {
        let output = repository.crane(&["task", "advance", "PAY-1821", "--to", state], &[], "");
        assert!(output.status.success(), "{}", text(&output.stderr));
    }
    let done = repository.ingest("jira", &["05-done-PAY-1821.json"], &[]);
    assert_eq!(done[0]["result"], "completed");
    let record = repository.record("PAY-1821");
    assert_eq!(
        states(&record),
        [
            "RECEIVED",
            "ANALYZING",
            "CONTRACT_PROPOSED",
            "APPROVED",
            "EXECUTING",
            "VALIDATING",
            "EXECUTING",
            "VALIDATING",
            "PR_READY",
            "REVIEW",
            "MERGED",
            "COMPLETED"
        ]
    );
    assert_eq!(repository.proposal_status("task_pay_1821_v1"), "retired");
    assert!(!repository
        .root
        .join(".crane/policies/task_pay_1821_v1.crane")
        .exists());
    assert!(repository
        .root
        .join(".crane/retired/task_pay_1821_v1.crane")
        .exists());
    let closed: Value = serde_json::from_slice(
        &repository
            .crane(
                &["agent", "session", "show", "generic-task-PAY-1821-v1"],
                &[],
                "",
            )
            .stdout,
    )
    .unwrap();
    assert_eq!(closed["lifecycle"], "finalized");
    assert_eq!(closed["attestation"]["final_status"], "PASS");
    let invalid = repository.crane(
        &["task", "advance", "PAY-1821", "--to", "EXECUTING"],
        &[],
        "",
    );
    assert!(text(&invalid.stderr).contains("cannot move from COMPLETED to EXECUTING"));
}

/** Task updates version the contract: a changed ticket supersedes the pending contract with a
 * new version, an identical update creates nothing, and a change while executing stops the
 * session of the outdated contract until the new version is approved, which retires the old one
 */
#[test]
fn task_updates_version_the_contract() {
    let repository = Repository::new();
    repository.ingest("jira", &["01-created-PAY-1821.json"], &[]);
    let updated = repository.ingest("jira", &["02-updated-PAY-1821.json"], &[]);
    assert_eq!(updated[0]["result"], "proposed task_pay_1821_v2");
    assert_eq!(repository.proposal_status("task_pay_1821_v1"), "superseded");
    let task: Value = serde_json::from_str(
        &fs::read_to_string(repository.root.join(".crane/tasks/PAY-1821.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(task["acceptance_criteria"].as_array().unwrap().len(), 2);
    let same = repository.ingest("jira", &["03-updated-same-PAY-1821.json"], &[]);
    assert!(same[0]["result"].as_str().unwrap().starts_with("unchanged"));
    let record = repository.record("PAY-1821");
    assert_eq!(record["contract_version"], 2);
    assert_eq!(record["versions"].as_array().unwrap().len(), 2);
    let archive = repository.root.join(".crane/runtime/tasks/PAY-1821");
    assert!(archive.join("task.v1.json").is_file() && archive.join("task.v2.json").is_file());

    repository.approve("task_pay_1821_v2");
    assert_eq!(repository.sync("PAY-1821")["state"], "EXECUTING");
    let changed = repository.ingest("jira", &["04-updated-again-PAY-1821.json"], &[]);
    assert_eq!(changed[0]["result"], "proposed task_pay_1821_v3");
    let stopped = repository.hook("pre-tool-use", refund_edit(), "task-PAY-1821-v2");
    assert_eq!(stopped.status.code(), Some(2));
    assert!(
        text(&stopped.stdout).contains("was cancelled"),
        "{}",
        text(&stopped.stdout)
    );
    assert_eq!(
        repository.proposal_status("task_pay_1821_v2"),
        "approved",
        "active until the next version is approved"
    );

    repository.approve("task_pay_1821_v3");
    let record = repository.sync("PAY-1821");
    assert_eq!(record["state"], "EXECUTING");
    assert_eq!(
        record["sessions"],
        json!(["generic-task-PAY-1821-v2", "generic-task-PAY-1821-v3"])
    );
    assert_eq!(record["active_proposal"], "task_pay_1821_v3");
    assert_eq!(repository.proposal_status("task_pay_1821_v2"), "retired");
    let v3 = repository.hook("pre-tool-use", refund_edit(), "task-PAY-1821-v3");
    assert!(v3.status.success(), "{}", text(&v3.stdout));
}

/** A cancelled ticket stops its active session: the session is closed (further mutating tool calls
 * are denied), the contract is retired, and replays change nothing
 */
#[test]
fn cancelled_ticket_stops_its_session() {
    let repository = Repository::new();
    repository.ingest("jira", &["01-created-PAY-1821.json"], &[]);
    repository.approve("task_pay_1821_v1");
    assert_eq!(repository.sync("PAY-1821")["state"], "EXECUTING");
    assert!(repository
        .hook("pre-tool-use", refund_edit(), "task-PAY-1821-v1")
        .status
        .success());

    let cancelled = repository.ingest("jira", &["06-wont-do-PAY-1821.json"], &[]);
    assert_eq!(cancelled[0]["event"], "cancelled");
    assert_eq!(cancelled[0]["result"], "cancelled");
    assert_eq!(cancelled[0]["state"], "CANCELLED");
    let denied = repository.hook("pre-tool-use", refund_edit(), "task-PAY-1821-v1");
    assert_eq!(denied.status.code(), Some(2));
    assert!(text(&denied.stdout).contains("was cancelled"));
    assert_eq!(repository.proposal_status("task_pay_1821_v1"), "retired");
    let replay = repository.ingest("jira", &["06-wont-do-PAY-1821.json"], &[]);
    assert_eq!(replay[0]["result"], "duplicate event ignored");
    let late = repository.ingest("jira", &["02-updated-PAY-1821.json"], &[]);
    assert_eq!(late[0]["result"], "ignored: the task is finished");
    assert_eq!(repository.record("PAY-1821")["state"], "CANCELLED");
}

/** Asana tasks go through the same lifecycle: compact events are completed from the local task
 * snapshot, delivery ids make replays idempotent, a missing snapshot blocks the task, and
 * completing it before merge cancels it
 */
#[test]
fn asana_task_runs_through_the_same_lifecycle() {
    let repository = Repository::new();
    let added = repository.ingest(
        "asana",
        &["01-added.json"],
        &["--delivery", "asana-delivery-1"],
    );
    assert_eq!(added.len(), 1, "the comment event is not a task event");
    assert_eq!(added[0]["task_id"], "ASANA-1208100000000001");
    assert_eq!(
        added[0]["result"],
        "proposed task_asana_1208100000000001_v1"
    );
    let task: Value = serde_json::from_str(
        &fs::read_to_string(
            repository
                .root
                .join(".crane/tasks/ASANA-1208100000000001.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(task["title"], "Round invoice totals to cents");
    assert_eq!(task["description"], "Change `InvoiceService.total` so that it rounds to two decimals. `InvoiceService.issue` must not change.");
    assert_eq!(
        task["acceptance_criteria"],
        json!(["InvoiceService.total returns 10.01 for 10.005"])
    );
    assert_eq!(
        (
            task["team"].as_str(),
            task["priority"].as_str(),
            task["requester"].as_str()
        ),
        (Some("billing"), Some("High"), Some("Ada Lovelace"))
    );
    assert_eq!(task["labels"], json!(["billing"]));
    let replay = repository.ingest(
        "asana",
        &["01-added.json"],
        &["--delivery", "asana-delivery-1"],
    );
    assert_eq!(replay[0]["result"], "duplicate event ignored");

    let missing = repository.ingest("asana", &["03-added-no-snapshot.json"], &[]);
    assert_eq!(missing[0]["state"], "BLOCKED");
    assert!(missing[0]["result"]
        .as_str()
        .unwrap()
        .contains("live Asana fetching needs API credentials"));

    let snapshot = ".crane/sources/asana/tasks/1208100000000001.json";
    let completed = fs::read_to_string(repository.root.join(snapshot))
        .unwrap()
        .replace("\"completed\": false", "\"completed\": true");
    repository.write(snapshot, &completed);
    let closed = repository.ingest("asana", &["02-completed.json"], &[]);
    assert_eq!(closed[0]["result"], "cancelled");
    let record = repository.record("ASANA-1208100000000001");
    assert_eq!(record["state"], "CANCELLED");
    assert!(record["reason"]
        .as_str()
        .unwrap()
        .contains("before the change was merged"));
    assert_eq!(
        repository.proposal_status("task_asana_1208100000000001_v1"),
        "superseded"
    );
}

/** Tickets the society should not act on: an unassigned ticket is ignored, a vague one is blocked
 * with the planner's reasons until it is clarified, an unmapped project is blocked, and unknown
 * sources are refused
 */
#[test]
fn unassigned_vague_and_unmapped_tickets() {
    let repository = Repository::new();
    let ignored = repository.ingest("jira", &["10-created-unassigned-PAY-1830.json"], &[]);
    assert_eq!(
        ignored[0]["result"],
        "ignored: not a task assigned to the society"
    );
    assert!(!repository
        .crane(&["task", "status", "PAY-1830"], &[], "")
        .status
        .success());

    let vague = repository.ingest("jira", &["11-created-vague-PAY-1840.json"], &[]);
    assert_eq!(vague[0]["state"], "BLOCKED");
    let record = repository.record("PAY-1840");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .starts_with("task_needs_clarification: no function or class"),
        "{}",
        record["reason"]
    );
    assert_eq!(record["proposal"], Value::Null);
    let clarified = repository.ingest("jira", &["12-clarified-PAY-1840.json"], &[]);
    assert_eq!(clarified[0]["result"], "proposed task_pay_1840_v2");
    assert_eq!(clarified[0]["state"], "CONTRACT_PROPOSED");

    let unmapped = repository.ingest("jira", &["13-created-unmapped-OPS-7.json"], &[]);
    assert_eq!(unmapped[0]["state"], "BLOCKED");
    assert!(unmapped[0]["result"]
        .as_str()
        .unwrap()
        .contains("no repository mapping for jira project 'OPS'"));

    let linear = repository.crane(&["task", "ingest", "--source", "linear"], &[], "{}");
    assert!(text(&linear.stderr).contains("not implemented yet"));
    let summary = text(&repository.crane(&["task", "status"], &[], "").stdout);
    assert!(
        summary.contains("PAY-1840 [jira] CONTRACT_PROPOSED contract v2 task_pay_1840_v2"),
        "{summary}"
    );
}

/** Agents cannot drive the lifecycle: ingest, sync, advance, and serve refuse to run on behalf of
 * an agent, and the pre-tool hook denies those commands
 */
#[test]
fn agents_cannot_drive_the_lifecycle() {
    let repository = Repository::new();
    repository.ingest("jira", &["01-created-PAY-1821.json"], &[]);
    let agent = [("CLAUDECODE", "1")];
    let event = fixtures()
        .join("jira/02-updated-PAY-1821.json")
        .to_string_lossy()
        .into_owned();
    for args in [
        vec!["task", "ingest", "--source", "jira", event.as_str()],
        vec!["task", "sync", "PAY-1821"],
        vec!["task", "advance", "PAY-1821", "--to", "CANCELLED"],
        vec!["task", "serve", "--once"],
    ] {
        let refused = repository.crane(&args, &agent, "");
        assert!(
            text(&refused.stderr).contains("refuses to run in an agent environment"),
            "{args:?}: {}",
            text(&refused.stderr)
        );
    }
    assert_eq!(repository.record("PAY-1821")["contract_version"], 1);
    for command in [
        "crane task advance PAY-1821 --to MERGED",
        "crane task sync",
        "crane task ingest --source jira x.json",
    ] {
        let denied = repository.hook(
            "pre-tool-use",
            json!({"tool": "Bash", "operation": "execute", "command": command}),
            "agent-1",
        );
        assert_eq!(denied.status.code(), Some(2), "{command}");
    }
    let allowed = repository.hook(
        "pre-tool-use",
        json!({"tool": "Bash", "operation": "execute", "command": "crane task status PAY-1821"}),
        "agent-1",
    );
    assert!(allowed.status.success());
}

/** The HTTP endpoint accepts a Jira delivery with the shared token, rejects a wrong token, and
 * answers Asana's handshake
 */
#[test]
fn http_endpoint_ingests_webhooks() {
    let repository = Repository::new();
    let token = "test-token-0123456789";
    let body = fs::read_to_string(fixtures().join("jira/01-created-PAY-1821.json")).unwrap();
    let request = |path: &str, given: &str, extra: &str, body: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
        command
            .args(["task", "serve", "--addr", "127.0.0.1:0", "--once"])
            .current_dir(&repository.root)
            .env("CRANE_WEBHOOK_TOKEN", token)
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
            "POST {path} HTTP/1.1\r\nHost: {address}\r\nX-Crane-Token: {given}\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        server.wait().unwrap();
        response
    };
    let accepted = request(
        "/webhooks/jira",
        token,
        "X-Atlassian-Webhook-Identifier: delivery-77\r\n",
        &body,
    );
    assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");
    assert!(
        accepted.contains("\"result\":\"proposed task_pay_1821_v1\""),
        "{accepted}"
    );
    assert!(repository.record("PAY-1821")["processed_events"]
        .to_string()
        .contains("delivery-77"));
    let rejected = request("/webhooks/jira", "wrong-token-0123456789", "", &body);
    assert!(rejected.starts_with("HTTP/1.1 401"), "{rejected}");
    let handshake = request("/webhooks/asana", token, "X-Hook-Secret: abc123\r\n", "");
    assert!(
        handshake.starts_with("HTTP/1.1 200") && handshake.contains("X-Hook-Secret: abc123"),
        "{handshake}"
    );
    let missing = repository.crane(&["task", "serve", "--once"], &[], "");
    assert!(text(&missing.stderr).contains("CRANE_WEBHOOK_TOKEN"));
}

/** A Jira task's contract is a versioned task contract: approving it through the policy proposal
 * approves the contract; when a zone changes under an executing task its contract is invalidated,
 * the session is cancelled, and the task is blocked until a human compiles and approves the
 * contract again, which the next sync adopts as a new session
 */
#[test]
fn invalidated_contract_blocks_the_task_until_recompiled() {
    let repository = Repository::new();
    repository.ingest("jira", &["01-created-PAY-1821.json"], &[]);
    repository.approve("task_pay_1821_v1");
    assert_eq!(repository.sync("PAY-1821")["state"], "EXECUTING");
    let contract: Value = serde_json::from_slice(
        &repository
            .crane(&["task", "contract", "show", "PAY-1821", "--json"], &[], "")
            .stdout,
    )
    .unwrap();
    assert_eq!(contract["status"], "approved", "{contract}");
    assert_eq!(contract["contract_id"], "PAY-1821@v1");

    repository.write(
        ".crane/zones/billing.zone",
        "zone billing {\n    criticality sensitive;\n    autonomy delegated;\n    select subsystem billing;\n}\n",
    );
    let blocked = repository.sync("PAY-1821");
    assert_eq!(blocked["state"], "BLOCKED");
    assert!(blocked["reason"]
        .as_str()
        .unwrap()
        .contains("contract PAY-1821@v1 invalidated: zones changed"));
    let stopped = repository.hook("pre-tool-use", refund_edit(), "task-PAY-1821-v1");
    assert_eq!(stopped.status.code(), Some(2));
    assert!(text(&stopped.stdout).contains("was cancelled"));

    let compiled = repository.crane(&["task", "contract", "compile", "PAY-1821"], &[], "");
    assert!(compiled.status.success(), "{}", text(&compiled.stderr));
    repository.approve("task_pay_1821_v2");
    let resumed = repository.sync("PAY-1821");
    assert_eq!(resumed["state"], "EXECUTING");
    assert_eq!(resumed["contract_version"], 2);
    assert_eq!(
        resumed["sessions"],
        json!(["generic-task-PAY-1821-v1", "generic-task-PAY-1821-v2"])
    );
}
