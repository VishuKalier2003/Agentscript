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

/** The invoice service, outside every zone */
const INVOICE: &str = "services/billing/src/main/java/com/acme/billing/InvoiceService.java";

/** The governed session of the billing task */
const SESSION: &str = "codex-task-PAY-1830-v1";

/** A Jira stand-in reached through the configured transport: it reads Crane's request on stdin,
 * keeps the issue's status, comments, and calls in a state file, and can be unavailable, answer an
 * error, lose the answer to a comment, or fail transitions */
const FAKE_JIRA: &str = r#"import json, sys
path = sys.argv[1]
state = json.load(open(path))
request = json.load(sys.stdin)
state["calls"].append(request["method"] + " " + request["path"])
def save():
    json.dump(state, open(path, "w"))
def answer(status, body=None):
    save()
    print(json.dumps({"status": status, "body": body}))
    sys.exit(0)
if not state["available"]:
    save()
    sys.stderr.write("connection refused")
    sys.exit(7)
if state.get("error_status"):
    answer(state["error_status"], {"errorMessages": ["Jira is down"]})
method, target = request["method"], request["path"]
if method == "GET" and target.endswith("/comment"):
    answer(200, {"comments": state["comments"]})
if method == "GET":
    answer(200, {"fields": {"status": {"name": state["status"], "statusCategory": {"key": "done" if state["status"] == "Done" else "new"}}}})
if method == "POST" and target.endswith("/comment"):
    comment = {"id": str(len(state["comments"]) + 1), "body": request["body"]["body"]}
    state["comments"].append(comment)
    if state.get("lose_comment_answer"):
        state["lose_comment_answer"] = False
        save()
        sys.exit(9)
    answer(201, {"id": comment["id"]})
if method == "POST" and target.endswith("/transitions"):
    if state.get("fail_transitions", 0) > 0:
        state["fail_transitions"] -= 1
        save()
        sys.stderr.write("timeout")
        sys.exit(7)
    state["transitions"].append(request["body"]["transition"]["id"])
    state["status"] = "Done"
    answer(204)
answer(404, {"errorMessages": ["unknown"]})
"#;

/** Find a Python interpreter (the Jira stand-in is a Python script) */
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

/** A connected repository whose Jira task PAY-1830 ran through a governed session to
 * DELIVERY_READY, with Jira reached through the stand-in transport; removed on drop
 * Fields
    - root: PathBuf - repository root
    - jira: PathBuf - the stand-in's state file
*/
struct Repository {
    root: PathBuf,
    jira: PathBuf,
}

impl Repository {
    /** Create the fixture */
    fn new() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-completion-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let jira = root.join(".crane-jira/state.json");
        let repository = Self { root, jira };
        repository.write("services/billing/pom.xml", "<project/>\n");
        repository.write(INVOICE, "package com.acme.billing;\n\npublic class InvoiceService {\n    public double total(double amount) {\n        return amount * 1.0;\n    }\n}\n");
        repository.write(".gitignore", ".crane-jira/\n");
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "lead@example.com"],
            vec!["config", "user.name", "Crane Completion"],
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
        repository.write(".crane-jira/fake_jira.py", FAKE_JIRA);
        repository.jira_state(json!({"available": true, "status": "To Do", "comments": [], "transitions": [], "calls": []}));
        repository.crane(&["init"]);
        repository.crane(&["checkpoint", "--name", "baseline"]);
        repository.write(".crane/sources/config.json", r#"{"sources_format": 1, "checkpoint": "baseline", "jira": {"acceptance_field": "customfield_10050", "projects": {"PAY": {"repositories": ["acme/shop"], "team": "billing"}}}}"#);
        repository.write(".crane/sources/jira/issues/PAY-1830.json", &json!({"id": "100", "key": "PAY-1830", "fields": {"summary": "Round invoice totals", "project": {"key": "PAY"}, "assignee": {"displayName": "Crane Bot"}, "status": {"name": "To Do", "statusCategory": {"key": "new"}}, "description": "Make `InvoiceService.total` round to cents.", "customfield_10050": "totals have two decimals"}}).to_string());
        repository.write(".crane/delivery.json", &json!({
            "merge_policy": {"default": {"name": "default", "approvals": 1, "approvers": ["lead"]}},
            "trackers": {"jira": {"done_transition": "31", "base_url": "https://acme.atlassian.net", "transport": [python(), repository.root.join(".crane-jira/fake_jira.py").to_string_lossy(), repository.jira.to_string_lossy()]}},
        }).to_string());
        repository.crane(&["repo", "connect"]);
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
        fs::write(&actions, json!([{"tool": "Edit", "operation": "write", "path": INVOICE, "edits": [{"old": "return amount * 1.0;", "new": "return Math.round(amount * 100.0) / 100.0;"}]}, {"operation": "stop", "text": "Done, all tests pass."}]).to_string()).unwrap();
        repository.crane(&[
            "session",
            "run",
            "PAY-1830",
            "--agent",
            "codex",
            "--actions",
            &actions.to_string_lossy(),
        ]);
        repository
    }

    /** Write a file relative to the root */
    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /** Run git and require success */
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

    /** Run crane with cleared markers */
    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
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
        child.stdin.take().unwrap().write_all(b"").unwrap();
        child.wait_with_output().unwrap()
    }

    /** Run crane as a human and require success */
    fn crane(&self, args: &[&str]) -> String {
        let output = self.run(args, &[]);
        assert!(
            output.status.success(),
            "crane {args:?}: {}{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /** Run crane with --json and require success */
    fn json(&self, args: &[&str]) -> Value {
        let mut full = args.to_vec();
        full.push("--json");
        serde_json::from_str(&self.crane(&full)).unwrap()
    }

    /** Replace the Jira stand-in's state */
    fn jira_state(&self, state: Value) {
        fs::create_dir_all(self.jira.parent().unwrap()).unwrap();
        fs::write(&self.jira, state.to_string()).unwrap();
    }

    /** Change some fields of the Jira stand-in's state */
    fn jira_set(&self, fields: Value) {
        let mut state = self.jira();
        for (key, value) in fields.as_object().unwrap() {
            state[key] = value.clone();
        }
        self.jira_state(state);
    }

    /** Read the Jira stand-in's state */
    fn jira(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(&self.jira).unwrap()).unwrap()
    }

    /** The task's intake state */
    fn task_state(&self) -> String {
        self.json(&["task", "show", "PAY-1830"])["state"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /** Deliver, approve, and merge the governed change; return the merged delivery */
    fn merge(&self) -> Value {
        self.crane(&["deliver", "run", SESSION]);
        self.crane(&["deliver", "approve", SESSION, "--approver", "lead"]);
        self.json(&["deliver", "merge", SESSION, "--by", "release-lead"])
    }

    /** The completion events */
    fn events(&self) -> Vec<Value> {
        self.json(&["task", "completions", "list"])["completions"]
            .as_array()
            .unwrap()
            .clone()
    }

    /** Send the due completion events (ignoring the backoff) */
    fn send(&self) -> Value {
        self.json(&["task", "completions", "send", "--now"])
    }

    /** The session's journal length (to prove the engineering session never reruns) */
    fn session_events(&self) -> usize {
        fs::read_to_string(
            self.root
                .join(format!(".crane/runtime/sessions/{SESSION}/journal.jsonl")),
        )
        .unwrap()
        .lines()
        .count()
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

/** Successful completion: nothing completes the task before the verified merge (not the agent
 * stopping, not passing tests, not the pull request, not an approval); after the merge one
 * completion event is queued with the task, repository, merge, attestation, and contract digest;
 * the dispatcher comments once and transitions once, and the task is COMPLETED */
#[test]
fn completion_follows_the_verified_merge_exactly_once() {
    let repository = Repository::new();
    assert_eq!(
        repository.task_state(),
        "DELIVERY_PENDING",
        "the agent stopped and the tests passed: not complete"
    );
    repository.crane(&["deliver", "run", SESSION]);
    assert_eq!(
        repository.task_state(),
        "PR_REVIEW",
        "a pull request is not a completion"
    );
    repository.crane(&["deliver", "approve", SESSION, "--approver", "lead"]);
    assert_eq!(
        repository.task_state(),
        "PR_REVIEW",
        "an approval alone is not a completion"
    );
    assert!(repository.events().is_empty());
    assert!(repository.jira()["calls"].as_array().unwrap().is_empty());

    let merged = repository.json(&["deliver", "merge", SESSION, "--by", "release-lead"]);
    let sha = merged["merged"]["sha"].as_str().unwrap().to_string();
    assert_eq!(merged["task_completion"]["status"], "pending");
    assert_eq!(repository.task_state(), "TASK_COMPLETION_PENDING");
    let id = merged["task_completion"]["completion_event"]
        .as_str()
        .unwrap()
        .to_string();
    let event = repository.json(&["task", "completions", "show", &id]);
    assert_eq!(event["task_id"], "PAY-1830");
    assert_eq!(event["external_id"], "PAY-1830");
    assert_eq!(event["repository"]["name"], "shop");
    assert!(event["repository"]["identity"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(event["merge_sha"], sha.as_str());
    assert_eq!(
        event["attestation"], merged["binding"]["attestation"],
        "the attestation the merge relied on"
    );
    assert_eq!(
        event["contract_digest"],
        merged["binding"]["contract_digest"]
    );
    assert!(event["contract_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(
        event["outbox"].as_array().unwrap().len(),
        2,
        "the Jira requests are in the outbox"
    );

    let sent = repository.send();
    assert_eq!(sent["sent"][0]["status"], "completed");
    let jira = repository.jira();
    assert_eq!(jira["status"], "Done");
    assert_eq!(jira["transitions"], json!(["31"]));
    assert_eq!(jira["comments"].as_array().unwrap().len(), 1);
    assert!(jira["comments"]
        .to_string()
        .contains(&format!("[crane-completion:{id}]")));
    assert!(jira["comments"].to_string().contains(&sha));
    assert_eq!(repository.task_state(), "COMPLETED");
    let journal = fs::read_to_string(
        repository
            .root
            .join(format!(".crane/runtime/delivery/{SESSION}/journal.jsonl")),
    )
    .unwrap();
    assert!(journal.contains("\"kind\":\"task_completed\""));
}

/** Duplicate completion events: sending again, reconciling, and reporting the merge again never
 * queue a second event or touch Jira twice */
#[test]
fn duplicate_completion_events_change_nothing() {
    let repository = Repository::new();
    let merged = repository.merge();
    repository.send();
    let calls = repository.jira()["calls"].as_array().unwrap().len();
    assert!(repository.send()["sent"].as_array().unwrap().is_empty());
    let reconciled = repository.json(&["task", "completions", "reconcile", "--send"]);
    assert_eq!(reconciled["queued"], json!([]));
    assert_eq!(reconciled["sent"], json!([]));
    let again = repository.json(&[
        "deliver",
        "merged",
        SESSION,
        "--sha",
        merged["merged"]["sha"].as_str().unwrap(),
    ]);
    assert_eq!(again["already_merged"], true);
    assert_eq!(repository.events().len(), 1);
    let jira = repository.jira();
    assert_eq!(jira["calls"].as_array().unwrap().len(), calls);
    assert_eq!(jira["transitions"], json!(["31"]));
    assert_eq!(jira["comments"].as_array().unwrap().len(), 1);
}

/** Jira unavailable: the event is kept, the task is COMPLETION_RETRY_PENDING (never FAILED), the
 * next attempt waits for the backoff, the engineering session is not rerun, and once Jira is back
 * the task completes exactly once */
#[test]
fn jira_unavailable_is_retried() {
    let repository = Repository::new();
    repository.merge();
    let journal = repository.session_events();
    repository.jira_set(json!({"available": false}));
    let first = repository.send();
    assert_eq!(first["sent"][0]["status"], "retry_pending");
    assert_eq!(first["sent"][0]["state"], "COMPLETION_RETRY_PENDING");
    assert!(first["sent"][0]["last_error"]
        .as_str()
        .unwrap()
        .contains("connection refused"));
    assert_eq!(repository.task_state(), "COMPLETION_RETRY_PENDING");
    let event = &repository.events()[0];
    assert_eq!(event["attempts"], 1);
    assert!(
        event["next_attempt_at"].as_u64().unwrap()
            > SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
    );
    let waiting = repository.json(&["task", "completions", "send"]);
    assert!(
        waiting["sent"].as_array().unwrap().is_empty(),
        "the backoff is respected"
    );

    repository.jira_set(json!({"available": true, "error_status": 503}));
    assert_eq!(
        repository.send()["sent"][0]["status"],
        "retry_pending",
        "a 503 is retried too"
    );
    repository.jira_set(json!({"error_status": null}));
    assert_eq!(repository.send()["sent"][0]["status"], "completed");
    assert_eq!(repository.events()[0]["attempts"], 3);
    let jira = repository.jira();
    assert_eq!(jira["transitions"], json!(["31"]));
    assert_eq!(jira["comments"].as_array().unwrap().len(), 1);
    assert_eq!(
        repository.session_events(),
        journal,
        "the engineering session is never rerun"
    );
    assert_eq!(repository.task_state(), "COMPLETED");
}

/** Process restart: a dispatcher that died while sending (its event left "sending", its lock left
 * behind, the answer to its comment lost) is recovered by reconciliation without posting the comment
 * twice; a completion event lost before it was written is queued again with the same id */
#[test]
fn completion_recovers_after_a_restart() {
    let repository = Repository::new();
    let merged = repository.merge();
    let id = merged["task_completion"]["completion_event"]
        .as_str()
        .unwrap()
        .to_string();
    // The comment reaches Jira but its answer is lost, and the process dies mid-send
    repository.jira_set(json!({"lose_comment_answer": true}));
    repository.send();
    let path = repository
        .root
        .join(format!(".crane/runtime/completions/{id}.json"));
    let mut event: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    event["status"] = json!("sending");
    fs::write(&path, event.to_string()).unwrap();
    fs::write(
        repository
            .root
            .join(".crane/runtime/completions/dispatch.lock"),
        "4242 1",
    )
    .unwrap();
    assert_eq!(repository.jira()["comments"].as_array().unwrap().len(), 1);

    let reconciled = repository.json(&["task", "completions", "reconcile", "--send"]);
    assert_eq!(reconciled["recovered"], json!([id]));
    assert_eq!(reconciled["sent"][0]["status"], "completed");
    let jira = repository.jira();
    assert_eq!(
        jira["comments"].as_array().unwrap().len(),
        1,
        "the comment carrying the event id is never posted twice"
    );
    assert_eq!(jira["transitions"], json!(["31"]));
    assert_eq!(repository.task_state(), "COMPLETED");

    // The process stopped between the merge and queuing its completion
    let other = Repository::new();
    let merged = other.merge();
    let id = merged["task_completion"]["completion_event"]
        .as_str()
        .unwrap()
        .to_string();
    fs::remove_file(
        other
            .root
            .join(format!(".crane/runtime/completions/{id}.json")),
    )
    .unwrap();
    assert_eq!(other.task_state(), "MERGED");
    let reconciled = other.json(&["task", "completions", "reconcile", "--send"]);
    assert_eq!(
        reconciled["queued"],
        json!([id]),
        "the same event id is queued again"
    );
    assert_eq!(reconciled["sent"][0]["status"], "completed");
    assert_eq!(other.jira()["transitions"], json!(["31"]));
}

/** Merge succeeded but the completion callback failed: the delivery is COMPLETE while the task waits
 * in COMPLETION_RETRY_PENDING (the merge is never undone and the task is not failed); the retry
 * finishes the transition without a second comment */
#[test]
fn merge_succeeded_but_completion_failed() {
    let repository = Repository::new();
    repository.jira_set(json!({"fail_transitions": 1}));
    repository.merge();
    let failed = repository.send();
    assert_eq!(failed["sent"][0]["status"], "retry_pending");
    assert!(failed["sent"][0]["last_error"]
        .as_str()
        .unwrap()
        .contains("timeout"));
    assert_eq!(
        repository.json(&["deliver", "status", SESSION])["delivery_state"],
        "COMPLETE"
    );
    assert_eq!(repository.task_state(), "COMPLETION_RETRY_PENDING");
    let lifecycle = repository.json(&["session", "lifecycle", SESSION]);
    assert_eq!(lifecycle["record"]["phase"], "DELIVERY_READY");
    assert_eq!(repository.jira()["comments"].as_array().unwrap().len(), 1);
    assert_eq!(repository.send()["sent"][0]["status"], "completed");
    let jira = repository.jira();
    assert_eq!(jira["comments"].as_array().unwrap().len(), 1);
    assert_eq!(jira["transitions"], json!(["31"]));
    assert_eq!(repository.task_state(), "COMPLETED");
}

/** Task already completed externally: the dispatcher finds the issue done and neither comments nor
 * transitions; the task is COMPLETED */
#[test]
fn task_already_completed_externally() {
    let repository = Repository::new();
    repository.merge();
    repository.jira_set(json!({"status": "Done"}));
    let sent = repository.send();
    assert_eq!(sent["sent"][0]["status"], "completed_externally");
    let jira = repository.jira();
    assert_eq!(jira["transitions"], json!([]));
    assert_eq!(jira["comments"], json!([]));
    assert_eq!(repository.task_state(), "COMPLETED");
    assert!(repository.send()["sent"].as_array().unwrap().is_empty());
}

/** Agents never drive tracker completion */
#[test]
fn agents_cannot_send_completions() {
    let repository = Repository::new();
    repository.merge();
    for args in [
        vec!["task", "completions", "send", "--now"],
        vec!["task", "completions", "reconcile", "--send"],
    ] {
        let output = repository.run(&args, &[("CLAUDECODE", "1")]);
        assert!(!output.status.success());
        assert!(text(&output.stderr).contains("refuses to run in an agent environment"));
    }
    assert!(repository.jira()["calls"].as_array().unwrap().is_empty());
}
