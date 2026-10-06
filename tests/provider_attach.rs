use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/** Counter that keeps fixture directory names unique when tests run in parallel */
static REPOSITORIES: AtomicUsize = AtomicUsize::new(0);

/** Environment variables that mark a process as running for an agent, or bind a hook */
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

/** A Claude Code session id (Claude Code uses UUIDs) */
const CLAUDE_SESSION: &str = "8c1f2a4e-5b6d-4e7f-9a0b-1c2d3e4f5a6b";

/** A Codex session id (Codex uses UUIDv7 thread ids) */
const CODEX_SESSION: &str = "0199a3c2-7d41-7e2a-b6c0-5f1e2d3c4b5a";

/** A Jira issue as the REST API returns it
 * Input
    - key: &str - issue key
    - summary: &str - summary
    - description: &str - description
    - criteria: &str - acceptance criteria field
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
            "labels": [],
            "reporter": {"displayName": "Product Manager", "emailAddress": "pm@example.com"},
            "assignee": {"displayName": "Crane Bot"},
            "status": {"name": "To Do", "statusCategory": {"key": "new"}},
            "description": description,
            "customfield_10050": criteria,
        }
    })
}

/** A connected repository with a payments zone, a permanent policy, Jira snapshots for two tasks,
 * removed on drop
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
            "crane-provider-{suffix}-{}",
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
            vec!["config", "user.name", "Crane Provider"],
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
            assert!(output.status.success());
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
            issue(
                "PAY-1821",
                "Reject negative refunds",
                "Make `PaymentService.refund` reject negative amounts without changing `PaymentService.charge`.",
                "`PaymentService.refund` throws for amounts below zero",
            ),
            issue(
                "PAY-1830",
                "Round invoice totals",
                "Make `InvoiceService.total` round to cents.",
                "totals have two decimals",
            ),
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

    /** Plan, approve, and launch a task for an agent, as a human does before starting it
     * Input
        - task: &str - task id
        - agent: &str - claude or codex
     * Output
        - String the launched Society session id
    */
    fn launch(&self, task: &str, agent: &str) -> String {
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
        let launched: Value = serde_json::from_str(
            &self.crane(&["task", "launch", task, "--agent", agent, "--json"]),
        )
        .unwrap();
        assert_eq!(launched["ready_to_run"], true);
        launched["launch"]["session_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /** Run a Claude Code hook the way its configuration does, with the input fields Claude Code
     * sends with every event
     * Input
        - event: &str - Claude Code event name, such as PreToolUse
        - environment: &[(&str, &str)] - the agent's environment (CRANE_SESSION)
        - fields: Value - event-specific fields
     * Output
        - Output
    */
    fn claude(&self, event: &str, environment: &[(&str, &str)], mut fields: Value) -> Output {
        let command = match event {
            "SessionStart" => "session-start",
            "PreToolUse" => "pre-tool-use",
            "PostToolUse" => "post-tool-use",
            "Stop" => "stop",
            "SessionEnd" => "session-end",
            other => other,
        };
        if fields.get("session_id").is_none() {
            fields["session_id"] = json!(CLAUDE_SESSION);
        }
        fields["transcript_path"] = json!(format!(
            "/home/dev/.claude/projects/shop/{CLAUDE_SESSION}.jsonl"
        ));
        if fields.get("cwd").is_none() {
            fields["cwd"] = json!(self.root.to_string_lossy());
        }
        fields["permission_mode"] = json!("default");
        if fields.get("hook_event_name").is_none() {
            fields["hook_event_name"] = json!(event);
        }
        self.run(
            &["agent", "hook", "--event", command, "--profile", "claude"],
            environment,
            &fields.to_string(),
        )
    }

    /** Run a Codex hook the way its configuration does, with Codex's common input fields
     * Input
        - event: &str - Codex event name
        - environment: &[(&str, &str)] - the agent's environment
        - fields: Value - event-specific fields
     * Output
        - Output
    */
    fn codex(&self, event: &str, environment: &[(&str, &str)], mut fields: Value) -> Output {
        fields["session_id"] = json!(CODEX_SESSION);
        fields["transcript_path"] = json!(format!(
            "/home/dev/.codex/sessions/2026/10/06/rollout-{CODEX_SESSION}.jsonl"
        ));
        fields["cwd"] = json!(self.root.to_string_lossy());
        fields["hook_event_name"] = json!(event);
        fields["model"] = json!("gpt-5-codex");
        fields["permission_mode"] = json!("default");
        fields["turn_id"] = json!("turn-3");
        self.run(
            &["agent", "hook", "--event", event, "--profile", "codex"],
            environment,
            &fields.to_string(),
        )
    }

    /** Read a session's journal
     * Input
        - session: &str - Crane session id
     * Output
        - Vec<Value>
    */
    fn journal(&self, session: &str) -> Vec<Value> {
        self.read(&format!(".crane/runtime/sessions/{session}/journal.jsonl"))
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /** List the Crane session ids
     * Input
        - None
     * Output
        - String
    */
    fn sessions(&self) -> String {
        self.crane(&["agent", "session", "list"])
    }

    /** A PATH without any Crane executable (git and the rest stay)
     * Input
        - None
     * Output
        - String
    */
    fn path_without_crane() -> String {
        let paths = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .filter(|folder| !folder.join("crane").is_file() && !folder.join("crane.exe").is_file())
            .collect::<Vec<_>>();
        std::env::join_paths(paths)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    /** The PATH the agent host runs hooks with: the Crane executable's folder first
     * Input
        - None
     * Output
        - String
    */
    fn path() -> String {
        let folder = Path::new(env!("CARGO_BIN_EXE_crane"))
            .parent()
            .unwrap()
            .to_path_buf();
        let mut paths = vec![folder];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        std::env::join_paths(paths)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for Repository {
    /** Remove the repository
     * Input
        - None (uses self)
     * Output
        - None
    */
    fn drop(&mut self) {
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

/** List the pre-tool decisions of a journal, with the normalized action
 * Input
    - journal: &[Value] - events
 * Output
    - Vec<(String, String, String)> tool, operation, decision
*/
fn decisions(journal: &[Value]) -> Vec<(String, String, String)> {
    journal
        .iter()
        .filter(|event| {
            matches!(
                event["event"].as_str(),
                Some("pre_tool_use" | "permission_request")
            )
        })
        .map(|event| {
            (
                event["tool"].as_str().unwrap_or_default().to_string(),
                event["operation"].as_str().unwrap_or_default().to_string(),
                event["decision"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/** Acceptance (Claude Code): a real Claude Code session, with its own UUID, attaches to the
 * already-approved Society session named by CRANE_SESSION; its SessionStart, PreToolUse,
 * PostToolUse, and Stop events are normalized into AgentActions that the authority engine decides
 * (allow, approval, denial), executed actions are verified, and the session is reconciled; no
 * second session is created and the status reports the binding */
#[test]
fn claude_attaches_to_an_approved_session() {
    let repository = Repository::new();
    let society = repository.launch("PAY-1821", "claude");
    assert_eq!(society, "claude-task-PAY-1821-v1");
    let path = Repository::path();
    let installed = repository.run(
        &["agent", "install", "--profile", "claude"],
        &[("PATH", &path)],
        "",
    );
    assert!(installed.status.success(), "{}", text(&installed.stderr));
    let hooks = repository.run(
        &["agent", "hooks", "--profile", "claude", "--json"],
        &[("PATH", &path)],
        "",
    );
    assert!(hooks.status.success(), "{}", text(&hooks.stdout));
    let agent = [("CRANE_SESSION", society.as_str()), ("PATH", path.as_str())];

    let start = repository.claude(
        "SessionStart",
        &agent,
        json!({"source": "startup", "model": "claude-opus-4-1"}),
    );
    assert!(start.status.success(), "{}", text(&start.stderr));
    let context = text(&start.stdout);
    assert!(
        context.contains("session: claude-task-PAY-1821-v1"),
        "{context}"
    );
    assert!(
        context.contains("task contract: PAY-1821@v1 (approved, sha256:"),
        "{context}"
    );

    let read = repository.claude("PreToolUse", &agent, json!({"tool_name": "Read", "tool_input": {"file_path": repository.root.join(PAYMENT).to_string_lossy()}, "tool_use_id": "toolu_01A"}));
    assert!(
        read.status.success() && text(&read.stdout).is_empty(),
        "{}",
        text(&read.stdout)
    );
    let refund = repository.claude("PreToolUse", &agent, json!({"tool_name": "Edit", "tool_input": {"file_path": repository.root.join(PAYMENT).to_string_lossy(), "old_string": "        return charge(-amount);", "new_string": "        if (amount < 0) {\n            throw new IllegalArgumentException(\"negative\");\n        }\n        return charge(-amount);", "replace_all": false}, "tool_use_id": "toolu_01B"}));
    assert!(
        text(&refund.stdout).contains("\"permissionDecision\":\"ask\""),
        "{}",
        text(&refund.stdout)
    );
    assert!(text(&refund.stdout).contains("zones payments (critical"));
    let charge = repository.claude("PreToolUse", &agent, json!({"tool_name": "Edit", "tool_input": {"file_path": repository.root.join(PAYMENT).to_string_lossy(), "old_string": "return fee(amount) + amount;", "new_string": "return amount;"}, "tool_use_id": "toolu_01C"}));
    assert_eq!(charge.status.code(), Some(2));
    assert!(text(&charge.stderr).contains("PaymentService.charge"));
    let outside = repository.claude("PreToolUse", &agent, json!({"tool_name": "Write", "tool_input": {"file_path": repository.root.join("services/billing/Notes.java").to_string_lossy(), "content": "class Notes {}\n"}, "tool_use_id": "toolu_01D"}));
    assert!(
        text(&outside.stdout).contains("outside the task scope"),
        "{}",
        text(&outside.stdout)
    );

    // The human approved the refund edit; Claude Code ran it and reports the result
    let source = repository.read(PAYMENT).replace("        return charge(-amount);", "        if (amount < 0) {\n            throw new IllegalArgumentException(\"negative\");\n        }\n        return charge(-amount);");
    repository.write(PAYMENT, &source);
    let post = repository.claude("PostToolUse", &agent, json!({"tool_name": "Edit", "tool_input": {"file_path": repository.root.join(PAYMENT).to_string_lossy(), "old_string": "        return charge(-amount);", "new_string": "x"}, "tool_response": {"filePath": repository.root.join(PAYMENT).to_string_lossy(), "success": true}, "tool_use_id": "toolu_01B"}));
    assert!(
        post.status.success(),
        "{}{}",
        text(&post.stdout),
        text(&post.stderr)
    );
    let _ = repository.claude("Stop", &agent, json!({"stop_hook_active": false}));

    assert!(
        !repository.sessions().contains(CLAUDE_SESSION),
        "no session keyed by the Claude UUID: {}",
        repository.sessions()
    );
    let journal = repository.journal(&society);
    let attached = journal
        .iter()
        .find(|event| event["event"] == "provider_attached")
        .unwrap();
    assert_eq!(attached["provider"], "claude");
    assert_eq!(attached["provider_session"], CLAUDE_SESSION);
    assert_eq!(attached["via"], "CRANE_SESSION");
    assert_eq!(attached["model"], "claude-opus-4-1");
    assert_eq!(attached["permission_mode"], "default");
    assert!(attached["transcript_path"]
        .as_str()
        .unwrap()
        .ends_with(".jsonl"));
    assert_eq!(
        decisions(&journal),
        [
            ("Read".to_string(), "read".to_string(), "allow".to_string()),
            ("Edit".into(), "write".into(), "approval_required".into()),
            ("Edit".into(), "write".into(), "deny".into()),
            ("Write".into(), "write".into(), "approval_required".into()),
        ]
    );
    let executed = journal
        .iter()
        .find(|event| event["event"] == "post_tool_use")
        .unwrap();
    assert!(executed["resources"][0]
        .as_str()
        .unwrap()
        .replace('\\', "/")
        .ends_with("PaymentService.java"));
    assert_eq!(executed["result"], "executed");
    assert!(journal
        .iter()
        .any(|event| event["event"] == "stop" && event["final_status"].is_string()));
    assert!(
        journal
            .iter()
            .all(|event| event["binding"] == journal[0]["binding"]),
        "every event carries the one binding"
    );

    // A second Claude Code session (after /clear) attaches to the same Society session
    let cleared = repository.claude(
        "SessionStart",
        &agent,
        json!({"session_id": "f00dbabe-0000-4000-8000-000000000001", "source": "clear"}),
    );
    assert!(cleared.status.success());
    let status: Value = serde_json::from_str(&text(
        &repository
            .run(
                &["agent", "status", "--session", &society, "--json"],
                &[("PATH", &path)],
                "",
            )
            .stdout,
    ))
    .unwrap();
    let session = &status["sessions"][0];
    assert_eq!(session["attachments"].as_array().unwrap().len(), 2);
    assert_eq!(session["task_id"], "PAY-1821");
    assert_eq!(session["task_contract"]["contract_id"], "PAY-1821@v1");
    assert_eq!(session["task_contract"]["current"], true);
    assert_eq!(session["checkpoints"][0]["checkpoint"], "baseline");
    assert!(session["checkpoints"][0]["sha"].as_str().unwrap().len() >= 40);
    assert_eq!(session["autonomy"], "delegated");
    assert_eq!(status["hooks"][0]["valid"], true);
}

/** Acceptance (Codex): a Codex session started with CRANE_TASK_ID attaches to the task's launched
 * session; apply_patch (direct and through the shell), shell commands as argument vectors, and
 * PermissionRequest are normalized and decided by the authority engine; PostToolUse verifies and
 * Stop reconciles */
#[test]
fn codex_attaches_through_its_task() {
    let repository = Repository::new();
    let society = repository.launch("PAY-1830", "codex");
    assert_eq!(society, "codex-task-PAY-1830-v1");
    let agent = [("CRANE_TASK_ID", "PAY-1830")];
    let start = repository.codex("SessionStart", &agent, json!({"source": "startup"}));
    assert!(start.status.success(), "{}", text(&start.stderr));
    assert!(text(&start.stdout).contains("task contract: PAY-1830@v1 (approved"));

    let patch = format!("*** Begin Patch\n*** Update File: {INVOICE}\n@@\n-        return amount * 1.0;\n+        return Math.round(amount * 100.0) / 100.0;\n*** End Patch\n");
    let allowed = repository.codex("PreToolUse", &agent, json!({"tool_name": "apply_patch", "tool_input": {"command": patch}, "tool_use_id": "call_1"}));
    assert!(
        allowed.status.success() && text(&allowed.stdout).is_empty(),
        "{}{}",
        text(&allowed.stdout),
        text(&allowed.stderr)
    );
    let through_shell = repository.codex("PreToolUse", &agent, json!({"tool_name": "Bash", "tool_input": {"command": ["bash", "-lc", format!("apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: {PAYMENT}\n@@\n-        return fee(amount) + amount;\n+        return amount;\n*** End Patch\nEOF")]}, "tool_use_id": "call_2"}));
    assert_eq!(
        through_shell.status.code(),
        Some(2),
        "a patch through the shell is the same write"
    );
    assert!(
        text(&through_shell.stderr).contains("PaymentService.charge"),
        "{}",
        text(&through_shell.stderr)
    );
    let listing = repository.codex("PreToolUse", &agent, json!({"tool_name": "Bash", "tool_input": {"command": ["bash", "-lc", "ls services"]}, "tool_use_id": "call_3"}));
    assert!(listing.status.success(), "{}", text(&listing.stderr));
    let permission = repository.codex("PermissionRequest", &agent, json!({"tool_name": "Bash", "tool_input": {"command": "crane checkpoint --name baseline"}, "tool_use_id": "call_4"}));
    assert!(
        text(&permission.stdout).contains("\"behavior\":\"deny\""),
        "{}",
        text(&permission.stdout)
    );

    repository.write(
        INVOICE,
        &repository.read(INVOICE).replace(
            "return amount * 1.0;",
            "return Math.round(amount * 100.0) / 100.0;",
        ),
    );
    let post = repository.codex("PostToolUse", &agent, json!({"tool_name": "apply_patch", "tool_input": {"command": patch}, "tool_response": "Success. Updated the following files:\nM services/billing/src/main/java/com/acme/billing/InvoiceService.java\n", "tool_use_id": "call_1"}));
    assert!(post.status.success(), "{}", text(&post.stderr));
    let _ = repository.codex(
        "Stop",
        &agent,
        json!({"stop_hook_active": false, "last_assistant_message": "Done."}),
    );

    assert!(!repository.sessions().contains(CODEX_SESSION));
    let journal = repository.journal(&society);
    let attached = journal
        .iter()
        .find(|event| event["event"] == "provider_attached")
        .unwrap();
    assert_eq!(attached["provider"], "codex");
    assert_eq!(attached["via"], "task");
    assert_eq!(attached["turn_id"], "turn-3");
    assert_eq!(
        decisions(&journal),
        [
            (
                "apply_patch".to_string(),
                "write".to_string(),
                "allow".to_string()
            ),
            ("Bash".into(), "write".into(), "deny".into()),
            ("Bash".into(), "execute".into(), "allow".into()),
            ("Bash".into(), "execute".into(), "deny".into()),
        ]
    );
    assert!(journal
        .iter()
        .any(|event| event["event"] == "post_tool_use" && event["result"] == "executed"));
    assert!(journal.iter().any(|event| event["event"] == "stop"));
}

/** Whenever the Society session or its decision cannot be obtained, nothing is authorized: an
 * unknown or ended CRANE_SESSION, another provider's session, a payload for another event, an
 * agent working outside the session's repository, and unreadable input are all answered with the
 * provider's blocking or deny response, and no session is created */
#[test]
fn binding_failures_never_authorize() {
    let repository = Repository::new();
    let society = repository.launch("PAY-1821", "claude");
    let edit = json!({"tool_name": "Edit", "tool_input": {"file_path": repository.root.join(INVOICE).to_string_lossy(), "old_string": "1.0", "new_string": "2.0"}});

    let unknown = repository.claude(
        "PreToolUse",
        &[("CRANE_SESSION", "claude-task-NOPE-1-v1")],
        edit.clone(),
    );
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        text(&unknown.stderr).contains("does not exist"),
        "{}",
        text(&unknown.stderr)
    );
    assert!(!repository.sessions().contains(CLAUDE_SESSION));

    let wrong = repository.codex("PreToolUse", &[("CRANE_SESSION", society.as_str())], json!({"tool_name": "apply_patch", "tool_input": {"command": "*** Begin Patch\n*** Delete File: README.md\n*** End Patch\n"}}));
    assert_eq!(wrong.status.code(), Some(2));
    assert!(
        text(&wrong.stderr).contains("belongs to claude, not codex"),
        "{}",
        text(&wrong.stderr)
    );
    let wrong_permission = repository.codex(
        "PermissionRequest",
        &[("CRANE_SESSION", society.as_str())],
        json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
    );
    assert!(
        text(&wrong_permission.stdout).contains("\"behavior\":\"deny\""),
        "{}",
        text(&wrong_permission.stdout)
    );

    let mut mismatched = edit.clone();
    mismatched["hook_event_name"] = json!("PostToolUse");
    let mismatch = repository.claude(
        "PreToolUse",
        &[("CRANE_SESSION", society.as_str())],
        mismatched,
    );
    assert_eq!(mismatch.status.code(), Some(2));
    assert!(text(&mismatch.stderr).contains("the payload is a PostToolUse event"));

    let elsewhere = std::env::temp_dir();
    let mut outside = edit.clone();
    outside["cwd"] = json!(elsewhere.to_string_lossy());
    let away = repository.claude(
        "PreToolUse",
        &[("CRANE_SESSION", society.as_str())],
        outside,
    );
    assert_eq!(away.status.code(), Some(2));
    assert!(
        text(&away.stderr).contains("outside the repository of session"),
        "{}",
        text(&away.stderr)
    );

    for (event, check) in [("PreToolUse", "exit"), ("PostToolUse", "exit")] {
        let broken = repository.run(
            &["agent", "hook", "--event", event, "--profile", "claude"],
            &[("CRANE_SESSION", society.as_str())],
            "{\"tool_name\": ",
        );
        assert_eq!(broken.status.code(), Some(2), "{event} {check}");
    }
    let codex_permission = repository.run(
        &[
            "agent",
            "hook",
            "--event",
            "PermissionRequest",
            "--profile",
            "codex",
        ],
        &[],
        "{not json",
    );
    assert!(codex_permission.status.success());
    assert!(text(&codex_permission.stdout).contains("\"behavior\":\"deny\""));
    let codex_post = repository.run(
        &[
            "agent",
            "hook",
            "--event",
            "PostToolUse",
            "--profile",
            "codex",
        ],
        &[],
        "{not json",
    );
    assert!(
        text(&codex_post.stdout).contains("\"decision\":\"block\""),
        "{}",
        text(&codex_post.stdout)
    );

    repository.crane(&["agent", "session", "cancel", &society]);
    let ended = repository.claude("PreToolUse", &[("CRANE_SESSION", society.as_str())], edit);
    assert_eq!(ended.status.code(), Some(2));
    assert!(
        text(&ended.stderr).contains("is cancelled"),
        "{}",
        text(&ended.stderr)
    );
}

/** Hooks are installed additively and idempotently, validated (events, matchers, duplicates,
 * permission rules, the executable on PATH), and removed without touching anything else; an agent
 * can never remove them */
#[test]
fn hooks_install_validate_and_disconnect() {
    let repository = Repository::new();
    let path = Repository::path();
    let with_path = [("PATH", path.as_str())];
    let user = json!({
        "model": "opus",
        "permissions": {"allow": ["Bash(npm test)"]},
        "hooks": {"Notification": [{"hooks": [{"type": "command", "command": "notify-send done"}]}]},
    });
    repository.write(
        ".claude/settings.local.json",
        &serde_json::to_string_pretty(&user).unwrap(),
    );
    assert!(repository
        .run(&["agent", "install", "--profile", "claude"], &with_path, "")
        .status
        .success());
    let settings: Value =
        serde_json::from_str(&repository.read(".claude/settings.local.json")).unwrap();
    assert_eq!(settings["model"], "opus");
    assert_eq!(
        settings["hooks"]["Notification"],
        user["hooks"]["Notification"]
    );
    assert_eq!(settings["permissions"]["allow"], json!(["Bash(npm test)"]));
    assert_eq!(settings["permissions"]["deny"].as_array().unwrap().len(), 3);
    assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "*");
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"],
        120
    );
    let installed = repository.read(".claude/settings.local.json");
    let again = repository.run(&["agent", "install", "--profile", "claude"], &with_path, "");
    assert!(text(&again.stdout).contains("already installed"));
    assert_eq!(repository.read(".claude/settings.local.json"), installed);

    let valid = repository.run(
        &["agent", "hooks", "--profile", "claude", "--json"],
        &with_path,
        "",
    );
    assert!(valid.status.success(), "{}", text(&valid.stdout));
    let without = Repository::path_without_crane();
    let missing_path = repository.run(
        &["agent", "hooks", "--profile", "claude"],
        &[("PATH", without.as_str())],
        "",
    );
    assert!(!missing_path.status.success());
    assert!(text(&missing_path.stdout).contains("crane is not on PATH"));

    let mut narrowed: Value = serde_json::from_str(&installed).unwrap();
    narrowed["hooks"]["PreToolUse"][0]["matcher"] = json!("Bash");
    let duplicate = narrowed["hooks"]["PostToolUse"][0].clone();
    narrowed["hooks"]["PostToolUse"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    repository.write(".claude/settings.local.json", &narrowed.to_string());
    let report: Value = serde_json::from_slice(
        &repository
            .run(
                &["agent", "hooks", "--profile", "claude", "--json"],
                &with_path,
                "",
            )
            .stdout,
    )
    .unwrap();
    assert_eq!(report["valid"], false);
    let problems = report["problems"].to_string();
    assert!(problems.contains("matcher 'Bash'"), "{problems}");
    assert!(
        problems.contains("PostToolUse runs Crane 2 times"),
        "{problems}"
    );
    repository.write(".claude/settings.local.json", &installed);

    // An agent cannot disconnect Crane
    repository.crane(&[
        "agent",
        "session",
        "start",
        "--profile",
        "claude",
        "--session",
        "a1",
    ]);
    let attempt = repository.claude("PreToolUse", &[], json!({"session_id": "a1", "tool_name": "Bash", "tool_input": {"command": "crane agent uninstall --profile claude"}}));
    assert_eq!(attempt.status.code(), Some(2));
    let refused = repository.run(
        &["agent", "uninstall", "--profile", "claude"],
        &[("CLAUDECODE", "1")],
        "",
    );
    assert!(text(&refused.stderr).contains("refuses to run in an agent environment"));
    assert_eq!(repository.read(".claude/settings.local.json"), installed);
    let status: Value = serde_json::from_slice(
        &repository
            .run(&["autonomy", "status", "claude-a1", "--json"], &[], "")
            .stdout,
    )
    .unwrap();
    assert_eq!(
        status["safety"], "quarantined",
        "trying to disconnect Crane is self-escalation"
    );

    let removed = repository.run(
        &["agent", "uninstall", "--profile", "claude"],
        &with_path,
        "",
    );
    assert!(removed.status.success(), "{}", text(&removed.stderr));
    let remaining: Value =
        serde_json::from_str(&repository.read(".claude/settings.local.json")).unwrap();
    assert_eq!(remaining, user, "only Crane's hooks and rules are removed");
    let disconnected: Value = serde_json::from_slice(
        &repository
            .run(
                &["agent", "hooks", "--profile", "claude", "--json"],
                &with_path,
                "",
            )
            .stdout,
    )
    .unwrap();
    assert_eq!(disconnected["valid"], false);
    assert!(disconnected["problems"]
        .to_string()
        .contains("PreToolUse is not registered"));
    assert!(text(
        &repository
            .run(
                &["agent", "uninstall", "--profile", "claude"],
                &with_path,
                ""
            )
            .stdout
    )
    .contains("not installed"));

    // Codex: a file Crane created alone is removed entirely
    assert!(repository
        .run(&["agent", "install", "--profile", "codex"], &with_path, "")
        .status
        .success());
    let codex = repository.run(
        &["agent", "hooks", "--profile", "codex", "--json"],
        &with_path,
        "",
    );
    assert!(codex.status.success(), "{}", text(&codex.stdout));
    let codex_report: Value = serde_json::from_slice(&codex.stdout).unwrap();
    assert_eq!(codex_report["events"].as_array().unwrap().len(), 7);
    assert!(codex_report["warnings"].to_string().contains("/hooks"));
    assert!(repository
        .run(
            &["agent", "uninstall", "--profile", "codex"],
            &with_path,
            ""
        )
        .status
        .success());
    assert!(!repository.root.join(".codex/hooks.json").exists());
}
