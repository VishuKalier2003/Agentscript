use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/** Repository files: payments and billing services, and an auth module */
const FILES: &[(&str, &str)] = &[
    ("README.md", "# Shop\n"),
    ("services/payments/pom.xml", "<project/>\n"),
    (
        SERVICE,
        "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    public int refund(int amount) {\n        return charge(-amount);\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n",
    ),
    ("services/billing/pom.xml", "<project/>\n"),
    (
        "services/billing/src/main/java/com/acme/billing/InvoiceService.java",
        "package com.acme.billing;\n\npublic class InvoiceService {\n    public double total(double amount) {\n        return amount * 1.0;\n    }\n\n    public int cancel(int amount) {\n        return refund(amount);\n    }\n}\n",
    ),
    ("auth/login.py", "def authenticate(user):\n    return user\n"),
];

/** Zones: Payments is Critical, Authentication is Restricted */
const ZONES: &str = "zone payments {\n    criticality critical;\n    autonomy assisted;\n    select subsystem payments;\n}\n\nzone authentication {\n    criticality restricted;\n    autonomy observe;\n    select subsystem auth;\n}\n";

/** Active policies: the permanent one and the approved task contract */
const POLICIES: &[(&str, &str)] = &[
    ("payments_core", "policy payments_core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n"),
    ("task_pay_1821", "policy task_pay_1821 {\n    checkpoint baseline;\n    target --function PaymentService.refund;\n    preserve --function InvoiceService.cancel;\n}\n"),
];

/** The task the sessions work on */
const TASK: &str = "{\"task_format\": 1, \"task_id\": \"PAY-1821\", \"title\": \"Reject negative refunds\", \"description\": \"Make `PaymentService.refund` reject negative amounts without changing `PaymentService.charge`.\", \"acceptance_criteria\": [\"`PaymentService.refund` throws for amounts below zero\"], \"repositories\": [\"acme/shop\"]}";

/** A temporary repository with Crane initialized, zones, policies, and a task, removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Repository {
    root: PathBuf,
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
            "crane-sessions-{suffix}-{}",
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
            vec!["config", "user.name", "Crane Sessions Test"],
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
        repository.write(".crane/zones/org.zone", ZONES);
        for (name, policy) in POLICIES {
            repository.write(&format!(".crane/policies/{name}.crane"), policy);
        }
        repository.write(".crane/tasks/PAY-1821.json", TASK);
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

    /** Send one hook event for a session
     * Input
        - profile: &str - claude, codex, or generic
        - event: &str - hook event
        - session: &str - provider session id
        - payload: Value - event payload (session_id is added)
        - options: &[&str] - session options such as --task or --autonomy
     * Output
        - Output
    */
    fn hook(
        &self,
        profile: &str,
        event: &str,
        session: &str,
        mut payload: Value,
        options: &[&str],
    ) -> Output {
        payload["session_id"] = json!(session);
        let mut args = vec!["agent", "hook", "--event", event, "--profile", profile];
        args.extend_from_slice(options);
        self.crane(&args, &[], &payload.to_string())
    }

    /** Ask the Claude hook about an edit of the billing service (unzoned, outside the task)
     * Input
        - session: &str - Claude session id
        - options: &[&str] - session options
        - new: &str - replacement text
     * Output
        - Output
    */
    fn billing_edit(&self, session: &str, options: &[&str], new: &str) -> Output {
        self.hook(
            "claude",
            "pre-tool-use",
            session,
            json!({"tool_name": "Edit", "tool_input": {
                "file_path": self.root.join("services/billing/src/main/java/com/acme/billing/InvoiceService.java").to_string_lossy(),
                "old_string": "        return amount * 1.0;",
                "new_string": new,
            }}),
            options,
        )
    }

    /** Read a session's description
     * Input
        - id: &str - Crane session id
     * Output
        - Value
    */
    fn show(&self, id: &str) -> Value {
        let output = self.crane(&["agent", "session", "show", id], &[], "");
        assert!(output.status.success(), "{}", text(&output.stderr));
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /** Read a session's journal
     * Input
        - id: &str - Crane session id
     * Output
        - Vec<Value>
    */
    fn journal(&self, id: &str) -> Vec<Value> {
        fs::read_to_string(
            self.root
                .join(format!(".crane/runtime/sessions/{id}/journal.jsonl")),
        )
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
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

/** Replace {root} in every string of a JSON value
 * Input
    - value: &mut Value - JSON value
    - root: &str - repository root
 * Output
    - None
*/
fn substitute(value: &mut Value, root: &str) {
    match value {
        Value::String(text) => *text = text.replace("{root}", root),
        Value::Array(items) => items.iter_mut().for_each(|item| substitute(item, root)),
        Value::Object(fields) => fields.values_mut().for_each(|item| substitute(item, root)),
        _ => {}
    }
}

/** Replay a recorded session: send each event to the provider's hook, check its exit code and
 * output against the recording's expectations, and apply the file changes the tool made
 * Input
    - repository: &Repository - repository
    - profile: &str - claude or codex
    - fixture: &str - JSONL file under tests/fixtures/sessions
    - session: &str - provider session id
 * Output
    - None (panics on any mismatch)
*/
fn replay(repository: &Repository, profile: &str, fixture: &str, session: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sessions")
        .join(fixture);
    let root = repository.root.to_string_lossy().replace('\\', "/");
    for line in fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let mut step: Value = serde_json::from_str(line).unwrap();
        substitute(&mut step, &root);
        let name = step["step"].as_str().unwrap().to_string();
        let output = repository.hook(
            profile,
            step["event"].as_str().unwrap(),
            session,
            step["payload"].clone(),
            &["--task", "PAY-1821"],
        );
        let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
        let expect = &step["expect"];
        assert_eq!(
            output.status.code(),
            expect["exit"].as_i64().map(|code| code as i32),
            "{name}\nstdout: {stdout}\nstderr: {stderr}"
        );
        for (stream, key) in [(&stdout, "stdout"), (&stderr, "stderr")] {
            for wanted in expect[key].as_array().into_iter().flatten() {
                assert!(
                    stream.contains(wanted.as_str().unwrap()),
                    "{name}: {key} lacks {wanted}\n{stream}"
                );
            }
        }
        if expect["stdout_empty"] == true {
            assert!(stdout.trim().is_empty(), "{name}: {stdout}");
        }
        for change in step["apply"].as_array().into_iter().flatten() {
            let file = repository.root.join(change["path"].as_str().unwrap());
            let content = fs::read_to_string(&file).unwrap();
            fs::write(
                &file,
                content.replacen(
                    change["old"].as_str().unwrap(),
                    change["new"].as_str().unwrap(),
                    1,
                ),
            )
            .unwrap();
        }
    }
}

/** A complete Claude Code session replays end to end: concise context at start, every tool call
 * through the common engine (contract, zones, autonomy, task scope), approval requirements asked
 * of the user, a passing reconciliation, and a closed session holding task, contract, checkpoint,
 * zones, autonomy, safety, budget, identity, and timestamps
 */
#[test]
fn claude_session_replays_end_to_end() {
    let repository = Repository::new();
    replay(&repository, "claude", "claude-session.jsonl", "c-replay");
    let session = repository.show("claude-c-replay");
    assert_eq!(session["lifecycle"], "closed");
    assert_eq!(session["task_id"], "PAY-1821");
    assert_eq!(session["agent"], "claude");
    assert_eq!(
        session["contracts"]["contracts"].as_array().unwrap().len(),
        2
    );
    assert!(
        session["contracts"]["contracts"][0]["checkpoint_sha"]
            .as_str()
            .unwrap()
            .len()
            == 40
    );
    let governance = &session["governance"];
    assert_eq!(governance["autonomy"], "delegated");
    assert_eq!(governance["task_title"], "Reject negative refunds");
    assert_eq!(
        governance["scope_modules"],
        json!(["module:java:com.acme.payments"])
    );
    assert_eq!(
        governance["budget"],
        json!({"mutating_actions": 300, "files": 100})
    );
    assert_eq!(governance["zones"][0]["zone_id"], "payments");
    let activity = &session["activity"];
    assert_eq!(activity["model"], "claude-opus-5-5");
    assert_eq!(activity["safety_state"], "active");
    assert_eq!(
        activity["budget"]["mutating_actions"]["used"], 3,
        "two approvals asked and one shell command"
    );
    assert_eq!(activity["budget"]["files"]["used"], 2);
    assert!(activity["started_at"].as_u64().unwrap() >= session["created_at"].as_u64().unwrap());
    assert!(session["expires_at"].as_u64().unwrap() > session["created_at"].as_u64().unwrap());
    let attestation = &repository.show("claude-c-replay")["attestation"];
    assert_eq!(attestation["final_status"], "PASS");
    assert_eq!(
        attestation["session"]["governance"]["autonomy"],
        "delegated"
    );
    assert_eq!(
        attestation["session"]["activity"]["model"],
        "claude-opus-5-5"
    );
    let decisions = repository
        .journal("claude-c-replay")
        .into_iter()
        .filter(|event| event["event"] == "pre_tool_use")
        .map(|event| event["decision"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        decisions,
        [
            "allow",
            "approval_required",
            "deny",
            "deny",
            "approval_required",
            "allow"
        ]
    );
}

/** A complete Codex session replays through the same engine: the same context, the same
 * decisions, rendered in Codex's protocol (an approval requirement blocks and the permission
 * request is left to the user)
 */
#[test]
fn codex_session_replays_end_to_end() {
    let repository = Repository::new();
    replay(&repository, "codex", "codex-session.jsonl", "x-replay");
    let session = repository.show("codex-x-replay");
    assert_eq!(session["lifecycle"], "closed");
    assert_eq!(session["activity"]["model"], "gpt-5-codex");
    assert_eq!(session["attestation"]["final_status"], "PASS");
    let decisions = repository
        .journal("codex-x-replay")
        .into_iter()
        .filter(|event| event["event"] == "pre_tool_use" || event["event"] == "permission_request")
        .map(|event| {
            format!(
                "{} {}",
                event["event"].as_str().unwrap(),
                event["decision"].as_str().unwrap()
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        decisions,
        [
            "pre_tool_use approval_required",
            "permission_request approval_required",
            "pre_tool_use deny",
            "pre_tool_use allow"
        ]
    );
}

/** Resume and cancellation: a closed session resumes from the agent host's session-start or a
 * human's resume; a cancelled session never acts or resumes again
 */
#[test]
fn sessions_resume_and_cancel() {
    let repository = Repository::new();
    let options = ["--autonomy", "autonomous"];
    assert!(repository
        .hook(
            "claude",
            "session-start",
            "r1",
            json!({"source": "startup"}),
            &options
        )
        .status
        .success());
    assert!(repository
        .billing_edit("r1", &options, "        return amount * 2.0;")
        .status
        .success());
    assert!(repository
        .hook("claude", "session-end", "r1", json!({}), &options)
        .status
        .success());
    let closed = repository.billing_edit("r1", &options, "        return amount * 2.0;");
    assert_eq!(closed.status.code(), Some(2));
    assert!(text(&closed.stderr).contains("is closed"));
    let resumed = repository.hook(
        "claude",
        "session-start",
        "r1",
        json!({"source": "resume"}),
        &options,
    );
    assert!(resumed.status.success());
    assert!(repository
        .billing_edit("r1", &options, "        return amount * 2.0;")
        .status
        .success());
    assert!(repository
        .journal("claude-r1")
        .iter()
        .any(|event| event["event"] == "session_start" && event["resumed"] == true));

    assert!(repository
        .hook("claude", "session-end", "r1", json!({}), &options)
        .status
        .success());
    assert!(repository
        .crane(&["agent", "session", "resume", "claude-r1"], &[], "")
        .status
        .success());
    assert_eq!(repository.show("claude-r1")["lifecycle"], "active");

    let cancelled = repository.crane(
        &[
            "agent",
            "session",
            "cancel",
            "claude-r1",
            "--reason",
            "task withdrawn",
        ],
        &[],
        "",
    );
    assert!(cancelled.status.success(), "{}", text(&cancelled.stderr));
    let denied = repository.billing_edit("r1", &options, "        return amount * 2.0;");
    assert!(text(&denied.stderr).contains("was cancelled"));
    let start = repository.hook(
        "claude",
        "session-start",
        "r1",
        json!({"source": "resume"}),
        &options,
    );
    assert!(text(&start.stdout).contains("This session has ended"));
    assert_eq!(repository.show("claude-r1")["lifecycle"], "cancelled");
    let refused = repository.crane(&["agent", "session", "resume", "claude-r1"], &[], "");
    assert!(text(&refused.stderr).contains("is cancelled; it cannot be resumed"));
    assert!(repository.show("claude-r1")["activity"]["ended_at"].is_u64());
}

/** Timeouts: an idle session's authority lapses and stays lapsed until the user or a human
 * resumes it; sweep closes idle sessions; an expired session cannot be resumed
 */
#[test]
fn interrupted_agents_lose_authority() {
    let repository = Repository::new();
    let options = ["--autonomy", "autonomous", "--idle-timeout", "1"];
    assert!(repository
        .hook("claude", "session-start", "t1", json!({}), &options)
        .status
        .success());
    std::thread::sleep(Duration::from_millis(2100));
    let lapsed = repository.billing_edit("t1", &options, "        return amount * 2.0;");
    assert_eq!(lapsed.status.code(), Some(2));
    assert!(
        text(&lapsed.stderr).contains("lapsed"),
        "{}",
        text(&lapsed.stderr)
    );
    let still = repository.billing_edit("t1", &options, "        return amount * 2.0;");
    assert!(
        text(&still.stderr).contains("lapsed"),
        "the lapse holds until a human or the host resumes"
    );
    assert!(repository
        .hook(
            "claude",
            "user-prompt-submit",
            "t1",
            json!({"prompt": "continue"}),
            &options
        )
        .status
        .success());
    assert!(repository
        .billing_edit("t1", &options, "        return amount * 2.0;")
        .status
        .success());
    assert_eq!(
        repository
            .journal("claude-t1")
            .iter()
            .filter(|event| event["event"] == "authority_lapsed")
            .count(),
        1
    );

    assert!(repository
        .hook("claude", "session-start", "t2", json!({}), &options)
        .status
        .success());
    std::thread::sleep(Duration::from_millis(2100));
    let sweep = text(
        &repository
            .crane(&["agent", "session", "sweep"], &[], "")
            .stdout,
    );
    assert!(
        sweep.contains("Timed out contract session claude-t2"),
        "{sweep}"
    );
    assert_eq!(repository.show("claude-t2")["lifecycle"], "closed");

    assert!(repository
        .hook("claude", "session-start", "t3", json!({}), &["--ttl", "0"])
        .status
        .success());
    let expired = repository.billing_edit("t3", &[], "        return amount * 2.0;");
    assert!(text(&expired.stderr).contains("expired"));
    let refused = repository.crane(&["agent", "session", "resume", "claude-t3"], &[], "");
    assert!(text(&refused.stderr).contains("expired"));
}

/** Finalization reconciles a session one last time, writes its final attestation, and ends it
 * for good; finalizing again returns the same outcome
 */
#[test]
fn sessions_finalize() {
    let repository = Repository::new();
    let options = ["--task", "PAY-1821"];
    assert!(repository
        .hook("generic", "session-start", "f1", json!({}), &options)
        .status
        .success());
    let finalized = repository.crane(&["agent", "session", "finalize", "generic-f1"], &[], "");
    assert!(
        text(&finalized.stdout).contains("Finalized contract session generic-f1: FAIL"),
        "{}",
        text(&finalized.stdout)
    );
    let session = repository.show("generic-f1");
    assert_eq!(session["lifecycle"], "finalized");
    assert_eq!(
        session["attestation"]["session"]["lifecycle"], "closed",
        "reconciled before it was marked final"
    );
    let denied = repository.hook(
        "generic",
        "pre-tool-use",
        "f1",
        json!({"tool": "Bash", "operation": "execute", "command": "ls"}),
        &options,
    );
    assert!(text(&denied.stdout).contains("was finalized"));
    let again = repository.crane(&["agent", "session", "finalize", "generic-f1"], &[], "");
    assert!(text(&again.stdout).contains("FAIL"));
    assert_eq!(
        repository
            .journal("generic-f1")
            .iter()
            .filter(|event| event["event"] == "session_finalized")
            .count(),
        1
    );
}

/** Autonomy modes and the budget: observe only reads, assisted asks for every change, an
 * exhausted budget quarantines the session until a human extends and resumes it
 */
#[test]
fn autonomy_modes_and_budget() {
    let repository = Repository::new();
    let observe = ["--autonomy", "observe"];
    let edit = repository.billing_edit("o1", &observe, "        return amount * 2.0;");
    assert_eq!(edit.status.code(), Some(2));
    assert!(text(&edit.stderr).contains("autonomy mode is observe"));
    let shell = repository.hook(
        "claude",
        "pre-tool-use",
        "o1",
        json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf build"}}),
        &observe,
    );
    assert_eq!(shell.status.code(), Some(2));
    let read = repository.hook(
        "claude",
        "pre-tool-use",
        "o1",
        json!({"tool_name": "Read", "tool_input": {"file_path": "README.md"}}),
        &observe,
    );
    assert!(read.status.success());

    let assisted = repository.billing_edit(
        "a1",
        &["--autonomy", "assisted"],
        "        return amount * 2.0;",
    );
    assert!(text(&assisted.stdout).contains("\"permissionDecision\":\"ask\""));
    assert!(text(&assisted.stdout).contains("autonomy mode is assisted"));

    let budget = ["--autonomy", "autonomous", "--max-actions", "2"];
    for _ in 0..2 {
        assert!(repository
            .billing_edit("b1", &budget, "        return amount * 2.0;")
            .status
            .success());
    }
    let exhausted = repository.billing_edit("b1", &budget, "        return amount * 2.0;");
    assert_eq!(exhausted.status.code(), Some(2));
    assert!(
        text(&exhausted.stderr).contains("autonomy budget exhausted: 2 of 2 mutating actions used")
    );
    assert_eq!(
        repository.show("claude-b1")["activity"]["safety_state"],
        "quarantined"
    );
    assert!(repository
        .crane(
            &["agent", "session", "extend", "claude-b1", "--actions", "5"],
            &[],
            ""
        )
        .status
        .success());
    let quarantined = repository.billing_edit("b1", &budget, "        return amount * 2.0;");
    assert!(
        text(&quarantined.stderr).contains("quarantined"),
        "{}",
        text(&quarantined.stderr)
    );
    assert!(repository
        .crane(&["agent", "session", "resume", "claude-b1"], &[], "")
        .status
        .success());
    assert!(repository
        .billing_edit("b1", &budget, "        return amount * 2.0;")
        .status
        .success());
    assert_eq!(
        repository.show("claude-b1")["activity"]["budget"]["mutating_actions"]["limit"],
        7
    );
}

/** A session whose contract drifts on disk is degraded: changes then need human approval */
#[test]
fn drifted_session_is_degraded() {
    let repository = Repository::new();
    let options = ["--autonomy", "autonomous"];
    assert!(repository
        .billing_edit("d1", &options, "        return amount * 2.0;")
        .status
        .success());
    repository.write(".crane/policies/extra.crane", "policy extra {\n    checkpoint baseline;\n    preserve --function InvoiceService.total;\n}\n");
    let post = repository.hook(
        "claude",
        "post-tool-use",
        "d1",
        json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
        &options,
    );
    assert!(post.status.success(), "{}", text(&post.stdout));
    assert_eq!(
        repository.show("claude-d1")["activity"]["safety_state"],
        "degraded"
    );
    let asked = repository.billing_edit("d1", &options, "        return amount * 3.0;");
    assert!(
        text(&asked.stdout).contains("\"permissionDecision\":\"ask\""),
        "{}",
        text(&asked.stdout)
    );
    assert!(text(&asked.stdout).contains("the session is degraded"));
}

/** Agents cannot manage sessions: session commands that change authority refuse to run in an agent
 * environment where they would extend authority, and the pre-tool hook denies them all
 */
#[test]
fn agents_cannot_manage_sessions() {
    let repository = Repository::new();
    assert!(repository
        .hook("claude", "session-start", "g1", json!({}), &[])
        .status
        .success());
    let agent = [("CLAUDECODE", "1")];
    for args in [
        vec!["agent", "session", "resume", "claude-g1"],
        vec![
            "agent",
            "session",
            "extend",
            "claude-g1",
            "--actions",
            "100",
        ],
        vec!["agent", "session", "start", "--session", "new"],
    ] {
        let refused = repository.crane(&args, &agent, "");
        assert!(
            text(&refused.stderr).contains("refuses to run in an agent environment"),
            "{args:?}"
        );
    }
    for command in [
        "crane agent session resume claude-g1",
        "crane agent session extend claude-g1 --actions 9",
        "crane agent session start --session x",
    ] {
        let denied = repository.hook(
            "claude",
            "pre-tool-use",
            "g1",
            json!({"tool_name": "Bash", "tool_input": {"command": command}}),
            &[],
        );
        assert_eq!(denied.status.code(), Some(2), "{command}");
    }
    // Trying to resume or extend its own session is self-escalation: the session is quarantined
    assert_eq!(
        repository.show("claude-g1")["activity"]["safety_state"],
        "quarantined"
    );
    let read = repository.hook("codex", "pre-tool-use", "g2", json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "crane agent session show claude-g1"}}), &[]);
    assert!(read.status.success(), "{}", text(&read.stderr));
}

/** A session started by the manager (for an orchestrator) gets the same binding and context, and
 * the agent host then works in it with the same provider session id
 */
#[test]
fn manager_starts_sessions_for_the_agent_host() {
    let repository = Repository::new();
    let started = repository.crane(
        &[
            "agent",
            "session",
            "start",
            "--profile",
            "claude",
            "--session",
            "m1",
            "--task",
            "PAY-1821",
            "--max-files",
            "1",
        ],
        &[],
        "",
    );
    assert!(started.status.success(), "{}", text(&started.stderr));
    let context = text(&started.stdout);
    assert!(context.contains("Contract session claude-m1 is ready."));
    assert!(context.contains("task: PAY-1821 - Reject negative refunds"));
    assert!(
        context.contains("300 of 300 mutating actions and 1 of 1 files left"),
        "{context}"
    );
    // The zone map adds at most one line per file (15 at most) after the zones line
    let zone_map = context
        .lines()
        .skip_while(|line| !line.contains("per-symbol decisions"))
        .skip(1)
        .take_while(|line| line.starts_with("- "))
        .count();
    assert!(
        context.lines().count() - zone_map < 40 && zone_map <= 16,
        "the context stays concise: {context}"
    );
    assert!(
        context.contains("PaymentService.charge: DENY preserve(payments_core)"),
        "{context}"
    );
    assert!(!context.contains("grants") && !context.contains("return fee(amount)"));
    let first = repository.billing_edit("m1", &[], "        return amount * 2.0;");
    assert!(first.status.success(), "{}", text(&first.stderr));
    let session = repository.show("claude-m1");
    assert_eq!(session["governance"]["budget"]["files"], 1);
    assert_eq!(session["task_id"], "PAY-1821");
}
