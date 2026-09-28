use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const PAYMENT: &str = "class PaymentService:
    def charge(self, amount):
        return amount * 3

    def calculate(self, amount):
        return amount + 1


def unrelated():
    return 1
";

const POLICY: &str = "policy payment {
    checkpoint baseline;
    preserve --function PaymentService.charge;
    target --function PaymentService.calculate;
}
";

/** Counter that keeps fixture directory names unique when tests run in parallel */
static FIXTURES: AtomicUsize = AtomicUsize::new(0);

/** A temporary repository with a committed baseline, a checkpoint, and one policy, removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /** Create the standard repository holding payment.py
     * Input
        - policy: &str - policy file content
     * Output
        - Fixture
    */
    fn new(policy: &str) -> Self {
        Self::with_files(&[("payment.py", PAYMENT)], policy)
    }

    /** Create the repository from the given files: commit them, run crane init, create the
     * baseline checkpoint, and write the policy
     * Input
        - files: &[(&str, &str)] - relative paths and contents
        - policy: &str - policy file content
     * Output
        - Fixture
    */
    fn with_files(files: &[(&str, &str)], policy: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-runtime-{suffix}-{}",
            FIXTURES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let fixture = Self { root };
        for (path, content) in files {
            fixture.write(path, content);
        }
        fixture.git(&["init", "-q"]);
        fixture.git(&["config", "user.email", "crane@example.com"]);
        fixture.git(&["config", "user.name", "Crane Runtime Test"]);
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-qm", "baseline"]);
        assert!(fixture.crane(&["init"], "").status.success());
        assert!(fixture
            .crane(&["checkpoint", "--name", "baseline"], "")
            .status
            .success());
        fixture.write(".crane/policies/payment.crane", policy);
        fixture
    }

    /** Write a file relative to the repository root, creating parent folders
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

    /** Read a file relative to the repository root
     * Input
        - path: &str - relative path
     * Output
        - String content
    */
    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }

    /** Run git in the repository and require success
     * Input
        - args: &[&str] - git arguments
     * Output
        - None (panics if git fails)
    */
    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("git should execute");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /** Run crane in the repository with the given stdin
     * Input
        - args: &[&str] - crane arguments
        - stdin: &str - text written to stdin
     * Output
        - Output of the finished process
    */
    fn crane(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_crane"))
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("crane should execute");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /** Run a Claude Code hook with a payload bound to a session id
     * Input
        - event: &str - hook event name
        - session: &str - Claude session id
        - payload: Value - extra payload fields (tool_name, tool_input, stop_hook_active)
     * Output
        - Output of the hook process
    */
    fn claude(&self, event: &str, session: &str, mut payload: Value) -> Output {
        payload["session_id"] = json!(session);
        payload["hook_event_name"] = json!(event);
        self.crane(
            &["agent", "hook", "--event", event, "--profile", "claude"],
            &payload.to_string(),
        )
    }

    /** Ask the Claude PreToolUse hook about an Edit of payment.py
     * Input
        - session: &str - Claude session id
        - old: &str - text to replace
        - new: &str - replacement
     * Output
        - Output of the hook process
    */
    fn edit(&self, session: &str, old: &str, new: &str) -> Output {
        self.claude(
            "pre-tool-use",
            session,
            json!({
                "tool_name": "Edit",
                "tool_input": {
                    "file_path": self.root.join("payment.py").to_string_lossy(),
                    "old_string": old,
                    "new_string": new,
                },
            }),
        )
    }

    /** Read a session's binding and attestation through crane agent session show
     * Input
        - session: &str - Crane session id
     * Output
        - Value JSON printed by the command
    */
    fn show(&self, session: &str) -> Value {
        let output = self.crane(&["agent", "session", "show", session], "");
        assert!(output.status.success(), "{}", text(&output.stderr));
        serde_json::from_slice(&output.stdout).expect("session show prints JSON")
    }

    /** Read a session's journal events
     * Input
        - session: &str - Crane session id
     * Output
        - Vec<Value> of journal events
    */
    fn journal(&self, session: &str) -> Vec<Value> {
        self.read(&format!(".crane/runtime/sessions/{session}/journal.jsonl"))
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Fixture {
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
    - bytes: &[u8] - stdout or stderr
 * Output
    - String
*/
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/** Change the target method so the target rule is satisfied
 * Input
    - fixture: &Fixture - repository
 * Output
    - None
*/
fn satisfy_target(fixture: &Fixture) {
    fixture.write(
        "payment.py",
        &PAYMENT.replace("return amount + 1", "return amount + 2"),
    );
}

/** 1: preserve denies a direct mutation before it runs, for Edit and Write, while comment-only
 * edits and restoring the checkpoint version stay allowed; the file is never touched by the hook
 */
#[test]
fn preserve_blocks_direct_mutation_before_execution() {
    let fixture = Fixture::new(POLICY);
    let denied = fixture.edit("s1", "return amount * 3", "return amount * 4");
    assert_eq!(denied.status.code(), Some(2), "{}", text(&denied.stderr));
    let reason = text(&denied.stderr);
    assert!(reason.contains("Crane denied this tool call"), "{reason}");
    assert!(
        reason.contains("would modify code protected by preserve function PaymentService.charge"),
        "{reason}"
    );
    assert_eq!(fixture.read("payment.py"), PAYMENT);

    let write = fixture.claude(
        "pre-tool-use",
        "s1",
        json!({"tool_name": "Write", "tool_input": {
            "file_path": "payment.py",
            "content": PAYMENT.replace("amount * 3", "amount * 5"),
        }}),
    );
    assert_eq!(write.status.code(), Some(2));

    let comment = fixture.edit(
        "s1",
        "        return amount * 3",
        "        # fee rate\n        return amount * 3",
    );
    assert!(comment.status.success(), "{}", text(&comment.stderr));

    // A change made outside Crane's view may be repaired, but not replaced by another change
    fixture.write("payment.py", &PAYMENT.replace("amount * 3", "amount * 9"));
    assert!(fixture
        .edit("s1", "return amount * 9", "return amount * 3")
        .status
        .success());
    assert_eq!(
        fixture
            .edit("s1", "return amount * 9", "return amount * 7")
            .status
            .code(),
        Some(2)
    );

    let decisions = fixture
        .journal("claude-s1")
        .iter()
        .filter(|event| event["event"] == "pre_tool_use")
        .map(|event| event["decision"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(decisions, ["deny", "deny", "allow", "allow", "deny"]);
    let denied_event = &fixture.journal("claude-s1")[0];
    assert!(denied_event["resources"]
        .as_array()
        .unwrap()
        .contains(&json!("symbol:function:PaymentService.charge")));
    assert!(denied_event["arguments_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(!denied_event.to_string().contains("amount * 4"));
}

/** 2: a shell command is allowed before it runs, but its effect on protected code is caught by
 * post-tool-use verification and by the final reconciliation
 */
#[test]
fn preserve_still_fails_after_indirect_shell_mutation() {
    let fixture = Fixture::new(POLICY);
    let bash = json!({"tool_name": "Bash", "tool_input": {"command": "python scripts/rewrite.py"}});
    assert!(fixture
        .claude("pre-tool-use", "s2", bash.clone())
        .status
        .success());
    fixture.write("payment.py", &PAYMENT.replace("amount * 3", "amount * 4"));
    let post = fixture.claude("post-tool-use", "s2", bash);
    assert_eq!(post.status.code(), Some(2));
    assert!(text(&post.stdout).contains("\"violation_type\":\"source_changed\""));

    let stop = fixture.claude("stop", "s2", json!({"stop_hook_active": false}));
    assert_eq!(stop.status.code(), Some(2));
    let attestation = &fixture.show("claude-s2")["attestation"];
    assert_eq!(attestation["final_status"], "FAIL");
    assert_eq!(attestation["preserve_results"][0]["status"], "fail");
    assert_eq!(
        attestation["preserve_results"][0]["violation_type"],
        "source_changed"
    );
    assert_eq!(
        attestation["reconciliation"]["preserve_invariants_satisfied"],
        false
    );
    assert_eq!(attestation["authorized_actions"][0]["tool"], "Bash");
}

/** 3: target authorizes mutating its item, and the full lifecycle passes once the target changed */
#[test]
fn target_permits_mutation_and_passes_when_changed() {
    let fixture = Fixture::new(POLICY);
    let allowed = fixture.edit("s3", "return amount + 1", "return amount + 2");
    assert!(allowed.status.success(), "{}", text(&allowed.stderr));
    let event = &fixture.journal("claude-s3")[0];
    assert_eq!(event["decision"], "allow");
    assert!(event["reasons"][0]
        .as_str()
        .unwrap()
        .contains("authorized by target function PaymentService.calculate"));

    satisfy_target(&fixture);
    let edit = json!({"tool_name": "Edit", "tool_input": {"file_path": "payment.py"}});
    assert!(fixture.claude("post-tool-use", "s3", edit).status.success());
    let stop = fixture.claude("stop", "s3", json!({"stop_hook_active": false}));
    assert!(stop.status.success(), "{}", text(&stop.stdout));
    let attestation = &fixture.show("claude-s3")["attestation"];
    assert_eq!(attestation["final_status"], "PASS");
    assert_eq!(attestation["target_results"][0]["status"], "pass");
    assert_eq!(
        attestation["reconciliation"]["required_targets_changed"],
        true
    );
    assert_eq!(
        attestation["reconciliation"]["forbidden_mutations_attempted"],
        false
    );
}

/** 4: target fails at stop when the final state is unchanged */
#[test]
fn target_fails_when_final_state_is_unchanged() {
    let fixture = Fixture::new(POLICY);
    let stop = fixture.claude("stop", "s4", json!({"stop_hook_active": false}));
    assert_eq!(stop.status.code(), Some(2));
    assert!(text(&stop.stdout).contains("\"violation_type\":\"target_unchanged\""));
    let attestation = &fixture.show("claude-s4")["attestation"];
    assert_eq!(attestation["final_status"], "FAIL");
    assert_eq!(
        attestation["reconciliation"]["required_targets_changed"],
        false
    );
}

/** 5: an authorized target change that is later reverted does not count; only the final state does */
#[test]
fn target_fails_when_intermediate_change_is_reverted() {
    let fixture = Fixture::new(POLICY);
    assert!(fixture
        .edit("s5", "return amount + 1", "return amount + 2")
        .status
        .success());
    satisfy_target(&fixture);
    let edit = json!({"tool_name": "Edit", "tool_input": {"file_path": "payment.py"}});
    assert!(fixture
        .claude("post-tool-use", "s5", edit.clone())
        .status
        .success());
    fixture.write("payment.py", PAYMENT);
    // A pending target does not block while the agent is still working
    assert!(fixture.claude("post-tool-use", "s5", edit).status.success());

    let stop = fixture.claude("stop", "s5", json!({"stop_hook_active": false}));
    assert_eq!(stop.status.code(), Some(2));
    let attestation = &fixture.show("claude-s5")["attestation"];
    assert_eq!(attestation["final_status"], "FAIL");
    assert_eq!(
        attestation["authorized_actions"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        attestation["target_results"][0]["violation_type"],
        "target_unchanged"
    );
}

/** 6: changes no contract covers stay allowed; target does not create an implicit allowlist */
#[test]
fn unrelated_mutation_remains_allowed() {
    let fixture = Fixture::new(POLICY);
    let edit = fixture.edit("s6", "    return 1", "    return 2");
    assert!(edit.status.success(), "{}", text(&edit.stderr));
    let create = fixture.claude(
        "pre-tool-use",
        "s6",
        json!({"tool_name": "Write", "tool_input": {"file_path": "notes/todo.md", "content": "hi"}}),
    );
    assert!(create.status.success());
    let journal = fixture.journal("claude-s6");
    assert_eq!(
        journal[0]["reasons"][0],
        "no contract clause covers this change"
    );
    assert!(journal[0]["resources"]
        .as_array()
        .unwrap()
        .contains(&json!("symbol:function:unrelated")));
    assert_eq!(journal[1]["resources"][0], "file:notes/todo.md");
}

/** 7 and 8: the session binds the checkpoint commit and contract version; a human re-baselining or
 * editing policies mid-session changes neither runtime authority nor verification, and the drift
 * is reported instead of silently adopted
 */
#[test]
fn runtime_and_verifier_share_checkpoint_and_policy_version() {
    let fixture = Fixture::new(POLICY);
    let start = fixture.claude("session-start", "s7", json!({"source": "startup"}));
    assert!(start.status.success());
    let bound = fixture.show("claude-s7");
    let version = bound["contracts"]["version"].as_str().unwrap().to_string();
    let sha = bound["contracts"]["contracts"][0]["checkpoint_sha"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text(&start.stdout).contains(&format!("contract_version: {version}")));

    // A human changes the protected code, commits it, re-baselines, and edits the policy
    fixture.write("payment.py", &PAYMENT.replace("amount * 3", "amount * 4"));
    fixture.git(&["commit", "-qam", "new rate"]);
    assert!(fixture
        .crane(&["checkpoint", "--name", "baseline"], "")
        .status
        .success());
    fixture.write(".crane/policies/payment.crane", &format!("{POLICY}\n"));
    fixture.write(
        "payment.py",
        &PAYMENT
            .replace("amount * 3", "amount * 4")
            .replace("amount + 1", "amount + 2"),
    );
    let disk = fixture.crane(&["check", "--json"], "");
    assert!(disk.status.success(), "{}", text(&disk.stdout));

    // The session keeps judging against the checkpoint and version it was bound to
    let denied = fixture.edit("s7", "return amount * 4", "return amount * 5");
    assert_eq!(denied.status.code(), Some(2));
    let stop = fixture.claude("stop", "s7", json!({"stop_hook_active": false}));
    assert_eq!(stop.status.code(), Some(2));
    let stop_output = text(&stop.stdout);
    assert!(stop_output.contains("\"violation_type\":\"source_changed\""));
    assert!(stop_output.contains("\"violation_type\":\"contract_drift\""));
    let attestation = &fixture.show("claude-s7")["attestation"];
    assert_eq!(attestation["contract_version"], version.as_str());
    assert_eq!(
        attestation["verification_contract_version"],
        version.as_str()
    );
    assert_ne!(attestation["repository_contract_version"], version.as_str());
    assert_eq!(attestation["contracts"][0]["checkpoint_sha"], sha.as_str());
    assert_eq!(
        attestation["preserve_results"][0]["checkpoint_sha"],
        sha.as_str()
    );
    assert_eq!(
        attestation["reconciliation"]["same_contract_version"],
        false
    );
    assert_eq!(attestation["final_status"], "FAIL");
    for event in fixture.journal("claude-s7") {
        assert_eq!(event["contract_version"], version.as_str());
        assert_eq!(event["checkpoints"][0], format!("baseline@{sha}"));
    }
}

/** 9: a target that cannot be located, or is ambiguous, denies every mutating tool (fail closed)
 * while reads stay allowed, and the final outcome fails
 */
#[test]
fn missing_and_ambiguous_targets_fail_closed() {
    for (name, policy) in [
        (
            "missing",
            "policy ghost {\n checkpoint baseline;\n preserve --function Ghost.method;\n}\n",
        ),
        (
            "ambiguous",
            "policy twin {\n checkpoint baseline;\n preserve --function helper;\n}\n",
        ),
    ] {
        let fixture = Fixture::new(policy);
        fixture.write("a.py", "def helper():\n    return 1\n");
        fixture.write("b.py", "def helper():\n    return 2\n");
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-qm", "helpers"]);
        assert!(fixture
            .crane(&["checkpoint", "--name", "baseline"], "")
            .status
            .success());
        let session = format!("s9{name}");
        let denied = fixture.edit(&session, "    return 1", "    return 2");
        assert_eq!(denied.status.code(), Some(2), "{name}");
        let reason = text(&denied.stderr);
        assert!(
            reason.contains("cannot establish runtime authority"),
            "{reason}"
        );
        assert!(
            reason.contains(if name == "missing" {
                "missing from checkpoint"
            } else {
                "ambiguous"
            }),
            "{reason}"
        );
        let bash = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
        assert_eq!(
            fixture.claude("pre-tool-use", &session, bash).status.code(),
            Some(2)
        );
        let read = json!({"tool_name": "Read", "tool_input": {"file_path": "payment.py"}});
        assert!(fixture
            .claude("pre-tool-use", &session, read)
            .status
            .success());

        // Only a human can repair the policy, so stop reports without trapping the agent
        let stop = fixture.claude("stop", &session, json!({"stop_hook_active": false}));
        assert!(stop.status.success(), "{name}");
        let attestation = &fixture.show(&format!("claude-{session}"))["attestation"];
        assert_eq!(attestation["final_status"], "FAIL", "{name}");
        assert_eq!(attestation["reconciliation"]["fully_reconciled"], false);
    }
}

/** 10: a malformed policy denies every mutating tool and fails the outcome, without looping */
#[test]
fn malformed_policy_fails_closed() {
    let fixture = Fixture::new(POLICY);
    fixture.write(
        ".crane/policies/broken.crane",
        "policy broken {\n preserve --function\n",
    );
    let denied = fixture.edit("s10", "    return 1", "    return 2");
    assert_eq!(denied.status.code(), Some(2));
    assert!(text(&denied.stderr).contains("policy broken is malformed"));
    let read = json!({"tool_name": "Grep", "tool_input": {"pattern": "charge"}});
    assert!(fixture.claude("pre-tool-use", "s10", read).status.success());
    let start = fixture.claude("session-start", "s10", json!({}));
    assert!(text(&start.stdout).contains("Unavailable policies"));

    satisfy_target(&fixture);
    for active in [false, true] {
        let stop = fixture.claude("stop", "s10", json!({"stop_hook_active": active}));
        assert!(stop.status.success());
        let payload: Value = serde_json::from_slice(&stop.stdout).unwrap();
        assert!(payload["systemMessage"]
            .as_str()
            .unwrap()
            .contains("human repairs .crane"));
    }
    let attestation = &fixture.show("claude-s10")["attestation"];
    assert_eq!(attestation["final_status"], "FAIL");
    assert_eq!(
        attestation["findings"][0]["violation_type"],
        "malformed_policy"
    );
}

/** 11 and 13: the same action gets the same decision through the Claude payload and through the
 * neutral format used by generic agents, and Codex verification still works
 */
#[test]
fn providers_share_one_decision_engine() {
    let fixture = Fixture::new(POLICY);
    let claude = fixture.edit("s11", "return amount * 3", "return amount * 4");
    let neutral = json!({
        "session_id": "s11",
        "tool": "apply_patch",
        "operation": "write",
        "path": "payment.py",
        "edits": [{"old": "return amount * 3", "new": "return amount * 4"}],
    });
    let generic = fixture.crane(
        &[
            "agent",
            "hook",
            "--event",
            "pre-tool-use",
            "--profile",
            "generic",
        ],
        &neutral.to_string(),
    );
    assert_eq!(claude.status.code(), Some(2));
    assert_eq!(generic.status.code(), Some(2));
    let generic_verdict: Value = serde_json::from_slice(&generic.stdout).unwrap();
    assert_eq!(generic_verdict["decision"], "deny");
    let reason = generic_verdict["reasons"][0].as_str().unwrap();
    assert!(text(&claude.stderr).contains(reason), "{reason}");
    assert_eq!(fixture.journal("generic-s11")[0]["agent"], "generic");

    let mut allowed = neutral.clone();
    allowed["edits"] = json!([{"old": "return amount + 1", "new": "return amount + 2"}]);
    let generic_allowed = fixture.crane(
        &[
            "agent",
            "hook",
            "--event",
            "pre-tool-use",
            "--profile",
            "generic",
        ],
        &allowed.to_string(),
    );
    assert!(generic_allowed.status.success());

    satisfy_target(&fixture);
    assert!(fixture
        .crane(&["agent", "verify", "--profile", "codex"], "")
        .status
        .success());
    let generic_stop = fixture.crane(
        &["agent", "hook", "--event", "stop", "--profile", "generic"],
        &json!({"session_id": "s11"}).to_string(),
    );
    assert!(
        generic_stop.status.success(),
        "{}",
        text(&generic_stop.stdout)
    );
}

/** 12: the Claude lifecycle still works end to end, and session-start gives the model only
 * readable context, never the runtime authority material
 */
#[test]
fn claude_session_lifecycle_and_context() {
    let fixture = Fixture::new(POLICY);
    let start = fixture.claude("session-start", "s12", json!({"source": "startup"}));
    assert!(start.status.success());
    let context = text(&start.stdout);
    assert!(context.starts_with("CRANE_CONTEXT_V1"));
    assert!(context.contains("Do not modify:\n- function PaymentService.charge"));
    assert!(context.contains("Must modify:\n- function PaymentService.calculate"));
    assert!(!context.contains("return amount * 3"), "{context}");
    assert!(!context.contains("grants"));

    let prompt = fixture.claude("user-prompt-submit", "s12", json!({"prompt": "go"}));
    assert!(prompt.status.success());
    let advisory: Value = serde_json::from_slice(&prompt.stdout).unwrap();
    assert!(advisory["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .contains("target rules require these changes"));

    satisfy_target(&fixture);
    assert!(fixture
        .claude("stop", "s12", json!({"stop_hook_active": false}))
        .status
        .success());
    assert!(fixture
        .claude("session-end", "s12", json!({}))
        .status
        .success());
    assert_eq!(fixture.show("claude-s12")["lifecycle"], "closed");
    let closed = fixture.edit("s12", "    return 1", "    return 2");
    assert_eq!(closed.status.code(), Some(2));
    assert!(text(&closed.stderr).contains("is closed"));
    assert!(fixture
        .claude("session-start", "s12", json!({"source": "resume"}))
        .status
        .success());
    assert_eq!(fixture.show("claude-s12")["lifecycle"], "active");
    assert!(fixture
        .edit("s12", "    return 1", "    return 2")
        .status
        .success());

    let events = fixture
        .journal("claude-s12")
        .iter()
        .map(|event| event["event"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        events,
        [
            "session_start",
            "user_prompt_submit",
            "stop",
            "session_end",
            "pre_tool_use",
            "session_start",
            "pre_tool_use"
        ]
    );
    let listing = text(&fixture.crane(&["agent", "session", "list"], "").stdout);
    assert!(listing.contains("claude-s12 active PASS"), "{listing}");
}

/** 14: an agent cannot create, replace, or refresh the contract or checkpoint that authorizes it */
#[test]
fn agent_cannot_create_or_refresh_its_authority() {
    let fixture = Fixture::new(POLICY);
    assert!(fixture
        .claude("session-start", "s14", json!({}))
        .status
        .success());
    let session_file = ".crane/runtime/sessions/claude-s14/session.json";
    let bound = fixture.read(session_file);
    for command in [
        "crane checkpoint --name baseline",
        "crane agent hook --event session-start --profile claude",
        "echo {} | crane.exe agent hook --event session-end",
        "git stash && crane init",
    ] {
        let payload = json!({"tool_name": "Bash", "tool_input": {"command": command}});
        assert_eq!(
            fixture.claude("pre-tool-use", "s14", payload).status.code(),
            Some(2),
            "{command}"
        );
    }
    for path in [
        session_file,
        ".crane/checkpoints/baseline.json",
        ".crane/runtime/sessions/claude-x/session.json",
    ] {
        let payload =
            json!({"tool_name": "Write", "tool_input": {"file_path": path, "content": "{}"}});
        assert_eq!(
            fixture.claude("pre-tool-use", "s14", payload).status.code(),
            Some(2),
            "{path}"
        );
    }
    // Reading the evidence is allowed
    let payload =
        json!({"tool_name": "Bash", "tool_input": {"command": "crane agent session list"}});
    assert!(fixture
        .claude("pre-tool-use", "s14", payload)
        .status
        .success());

    // A new session-start for the same session never refreshes its binding
    fixture.write(".crane/policies/payment.crane", &format!("{POLICY}\n"));
    assert!(fixture
        .claude("session-start", "s14", json!({"source": "resume"}))
        .status
        .success());
    assert_eq!(fixture.read(session_file), bound);
    assert_eq!(fixture.read(".crane/runtime/.gitignore"), "*\n");
}

/** 15: every route to the .crane policy files is denied before policy rules are even consulted */
#[test]
fn agent_cannot_modify_policy_files() {
    let fixture = Fixture::new(POLICY);
    let absolute = fixture
        .root
        .join(".crane")
        .join("policies")
        .join("payment.crane");
    for payload in [
        json!({"tool_name": "Edit", "tool_input": {"file_path": absolute.to_string_lossy(), "old_string": "preserve", "new_string": "target"}}),
        json!({"tool_name": "Write", "tool_input": {"file_path": ".crane/policies/new.crane", "content": "x"}}),
        json!({"tool_name": "PowerShell", "tool_input": {"command": "Set-Content .crane\\policies\\payment.crane ''"}}),
        json!({"tool_name": "mcp__fs__write_file", "tool_input": {"path": "./.crane/policies/payment.crane"}}),
    ] {
        let output = fixture.claude("pre-tool-use", "s15", payload.clone());
        assert_eq!(output.status.code(), Some(2), "{payload}");
        assert!(text(&output.stderr).contains("may not modify .crane"));
    }
    assert_eq!(fixture.read(".crane/policies/payment.crane"), POLICY);
}

/** 16: a forced retry of stop lets a pending target through, so an unreachable target cannot loop */
#[test]
fn stop_loop_prevention_still_works() {
    let fixture = Fixture::new(POLICY);
    let first = fixture.claude("stop", "s16", json!({"stop_hook_active": false}));
    assert_eq!(first.status.code(), Some(2));
    let retry = fixture.claude("stop", "s16", json!({"stop_hook_active": true}));
    assert!(retry.status.success());
    let payload: Value = serde_json::from_slice(&retry.stdout).unwrap();
    assert!(payload["systemMessage"]
        .as_str()
        .unwrap()
        .contains("targets not yet satisfied"));
    let stops = fixture
        .journal("claude-s16")
        .into_iter()
        .filter(|event| event["event"] == "stop")
        .map(|event| event["stop_hook_active"].clone())
        .collect::<Vec<_>>();
    assert_eq!(stops, [json!(false), json!(true)]);
}

/** Sessions can expire, and a tampered journal fails the outcome instead of being ignored */
#[test]
fn expired_sessions_and_tampered_journals_fail_closed() {
    let fixture = Fixture::new(POLICY);
    let start = fixture.crane(
        &[
            "agent",
            "hook",
            "--event",
            "session-start",
            "--profile",
            "claude",
            "--ttl",
            "0",
        ],
        &json!({"session_id": "s17"}).to_string(),
    );
    assert!(start.status.success());
    let expired = fixture.edit("s17", "    return 1", "    return 2");
    assert_eq!(expired.status.code(), Some(2));
    assert!(text(&expired.stderr).contains("expired"));

    satisfy_target(&fixture);
    let journal = root_path(&fixture, ".crane/runtime/sessions/claude-s17/journal.jsonl");
    let mut content = fs::read_to_string(&journal).unwrap();
    content.push_str("{\"event\":\"forged\"}\n");
    fs::write(&journal, content).unwrap();
    let stop = fixture.claude("stop", "s17", json!({"stop_hook_active": false}));
    assert!(stop.status.success(), "journal problems are human-owned");
    let attestation = &fixture.show("claude-s17")["attestation"];
    assert_eq!(attestation["final_status"], "FAIL");
    assert_eq!(
        attestation["findings"][0]["violation_type"],
        "journal_error"
    );
}

/** Hooks without a session id keep the pre-session behavior: nothing is journaled, but the same
 * decisions apply
 */
#[test]
fn hooks_without_session_id_use_a_transient_session() {
    let fixture = Fixture::new(POLICY);
    let payload = json!({"tool_name": "Edit", "tool_input": {
        "file_path": "payment.py", "old_string": "return amount * 3", "new_string": "return amount * 4"}});
    let output = fixture.crane(
        &[
            "agent",
            "hook",
            "--event",
            "pre-tool-use",
            "--profile",
            "claude",
        ],
        &payload.to_string(),
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(!fixture.root.join(".crane/runtime/sessions").exists());
}

/** Join a relative path onto the fixture root
 * Input
    - fixture: &Fixture - repository
    - path: &str - relative path
 * Output
    - PathBuf
*/
fn root_path(fixture: &Fixture, path: &str) -> PathBuf {
    Path::new(&fixture.root).join(path)
}

/** Every preserve scope is enforced before execution: each row is an edit and the expected
 * decision under a file, folder, flow, and all scope anchored on pay/service.py's charge
 */
#[test]
fn preserve_scopes_are_enforced_before_execution() {
    let service = "def charge(amount):\n    return fee(amount) + amount\n\n\ndef fee(amount):\n    return 2\n\n\ndef other():\n    return 3\n";
    let files = [
        ("pay/service.py", service),
        ("pay/extra.py", "def extra():\n    return 4\n"),
        ("api/handler.py", "def handle():\n    return 5\n"),
    ];
    // (file, old, new, [file, folder, flow, all] allowed?)
    let rows = [
        (
            "pay/service.py",
            "    return 3",
            "    return 30",
            [false, false, true, false],
        ),
        (
            "pay/service.py",
            "    return 2",
            "    return 20",
            [false, false, false, false],
        ),
        (
            "pay/extra.py",
            "    return 4",
            "    return 40",
            [true, false, true, false],
        ),
        (
            "api/handler.py",
            "    return 5",
            "    return 50",
            [true, true, true, false],
        ),
        (
            "api/handler.py",
            "    return 5",
            "    return charge(5)",
            [true, true, false, false],
        ),
        (
            "api/handler.py",
            "    return 5",
            "    # five\n    return 5",
            [true, true, true, true],
        ),
    ];
    for (column, scope) in ["file", "folder", "flow", "all"].into_iter().enumerate() {
        let policy = format!(
            "policy scoped {{\n checkpoint baseline;\n preserve --function charge scope {scope};\n}}\n"
        );
        let fixture = Fixture::with_files(&files, &policy);
        for (path, old, new, expected) in rows {
            let output = fixture.claude(
                "pre-tool-use",
                scope,
                json!({"tool_name": "Edit", "tool_input": {
                    "file_path": fixture.root.join(path).to_string_lossy(),
                    "old_string": old,
                    "new_string": new,
                }}),
            );
            assert_eq!(
                output.status.success(),
                expected[column],
                "scope {scope}: {path} {old:?} -> {new:?}: {}",
                text(&output.stderr)
            );
        }
    }
}
