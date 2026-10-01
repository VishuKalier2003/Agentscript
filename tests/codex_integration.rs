use std::fs;
use std::io::Write;
use std::path::PathBuf;
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

/** A temporary Git repository, removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /** Create a repository with payment.py committed, but no Crane metadata yet
     * Input
        - None
     * Output
        - Fixture
    */
    fn bare() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-codex-{suffix}-{}",
            FIXTURES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let fixture = Self { root };
        fixture.write("payment.py", PAYMENT);
        fixture.git(&["init", "-q"]);
        fixture.git(&["config", "user.email", "crane@example.com"]);
        fixture.git(&["config", "user.name", "Crane Codex Test"]);
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-qm", "baseline"]);
        fixture
    }

    /** Create a repository with Crane initialized, a baseline checkpoint, and the policy
     * Input
        - None
     * Output
        - Fixture
    */
    fn new() -> Self {
        let fixture = Self::bare();
        assert!(fixture.crane(&["init"], "").status.success());
        fixture.protect();
        fixture
    }

    /** Create the baseline checkpoint and write the policy
     * Input
        - None (uses self)
     * Output
        - None
    */
    fn protect(&self) {
        assert!(self
            .crane(&["checkpoint", "--name", "baseline"], "")
            .status
            .success());
        self.write(".crane/policies/payment.crane", POLICY);
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
        assert!(output.status.success());
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

    /** Run a Codex hook the way the installed configuration does, with the common Codex input
     * fields (session_id, cwd, hook_event_name, model, permission_mode, turn_id) added
     * Input
        - event: &str - Codex event name such as "PreToolUse"
        - session: &str - Codex session id
        - payload: Value - event-specific fields
     * Output
        - Output of the hook process
    */
    fn codex(&self, event: &str, session: &str, mut payload: Value) -> Output {
        payload["session_id"] = json!(session);
        payload["transcript_path"] = Value::Null;
        payload["cwd"] = json!(self.root.to_string_lossy());
        payload["hook_event_name"] = json!(event);
        payload["model"] = json!("gpt-5-codex");
        payload["permission_mode"] = json!("default");
        payload["turn_id"] = json!("turn-1");
        self.crane(
            &["agent", "hook", "--event", event, "--profile", "codex"],
            &payload.to_string(),
        )
    }

    /** Send an apply_patch tool call to a Codex tool event
     * Input
        - event: &str - PreToolUse, PostToolUse, or PermissionRequest
        - session: &str - Codex session id
        - patch: &str - apply_patch text
     * Output
        - Output of the hook process
    */
    fn patch(&self, event: &str, session: &str, patch: &str) -> Output {
        self.codex(
            event,
            session,
            json!({
                "tool_name": "apply_patch",
                "tool_use_id": "call-1",
                "tool_input": { "command": patch },
            }),
        )
    }

    /** Send a Bash tool call to a Codex tool event
     * Input
        - event: &str - PreToolUse, PostToolUse, or PermissionRequest
        - session: &str - Codex session id
        - command: &str - shell command
     * Output
        - Output of the hook process
    */
    fn bash(&self, event: &str, session: &str, command: &str) -> Output {
        self.codex(
            event,
            session,
            json!({
                "tool_name": "Bash",
                "tool_use_id": "call-2",
                "tool_input": { "command": command },
            }),
        )
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

/** Build an apply_patch patch that replaces one line of payment.py
 * Input
    - old: &str - line to remove (without indentation)
    - new: &str - line to add (without indentation)
 * Output
    - String patch
*/
fn update(old: &str, new: &str) -> String {
    format!(
        "*** Begin Patch\n*** Update File: payment.py\n@@ class PaymentService:\n-        {old}\n+        {new}\n*** End Patch\n"
    )
}

/** Parse stdout as one JSON value
 * Input
    - output: &Output - hook output
 * Output
    - Value
*/
fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("stdout is not JSON: {}", text(&output.stdout)))
}

/** 1, 3, 4, 13: a valid apply_patch PreToolUse event is parsed; a patch changing preserved code
 * is denied with exit 2 and a clean reason on stderr, a patch changing the target is allowed
 * silently, and both decisions are journaled for the Codex session
 */
#[test]
fn pre_tool_use_authorizes_apply_patch() {
    let fixture = Fixture::new();
    let denied = fixture.patch(
        "PreToolUse",
        "c1",
        &update("return amount * 3", "return amount * 4"),
    );
    assert_eq!(denied.status.code(), Some(2));
    assert!(denied.stdout.is_empty());
    let reason = text(&denied.stderr);
    assert!(
        reason.starts_with("crane: Crane denied this tool call: "),
        "{reason}"
    );
    assert!(!reason.contains("HOOK_BLOCK"));
    assert!(
        reason.contains("would modify code protected by preserve function PaymentService.charge")
    );

    let allowed = fixture.patch(
        "PreToolUse",
        "c1",
        &update("return amount + 1", "return amount + 2"),
    );
    assert!(allowed.status.success(), "{}", text(&allowed.stderr));
    assert!(allowed.stdout.is_empty());

    let journal = fixture.journal("codex-c1");
    assert_eq!(journal[0]["agent"], "codex");
    assert_eq!(journal[0]["tool"], "apply_patch");
    assert_eq!(journal[0]["operation"], "write");
    assert_eq!(journal[0]["decision"], "deny");
    assert!(journal[0]["resources"]
        .as_array()
        .unwrap()
        .contains(&json!("symbol:function:PaymentService.charge")));
    assert_eq!(journal[1]["decision"], "allow");
    assert!(!fixture
        .read(".crane/runtime/sessions/codex-c1/journal.jsonl")
        .contains("amount * 4"));
}

/** 2: invalid hook JSON, an unparsable patch, and a patch without its end marker all deny */
#[test]
fn malformed_events_fail_closed() {
    let fixture = Fixture::new();
    let invalid = fixture.crane(
        &[
            "agent",
            "hook",
            "--event",
            "PreToolUse",
            "--profile",
            "codex",
        ],
        "{not json",
    );
    assert_eq!(invalid.status.code(), Some(2));
    assert!(text(&invalid.stderr).contains("invalid hook input"));
    for patch in [
        "rewrite payment.py please",
        "*** Begin Patch\n*** Update File: payment.py\n-        return 1\n+        return 2\n",
        "*** Begin Patch\n*** Frobnicate File: payment.py\n*** End Patch\n",
    ] {
        let output = fixture.patch("PreToolUse", "c2", patch);
        assert_eq!(output.status.code(), Some(2), "{patch}");
        assert!(
            text(&output.stderr).contains("cannot be determined"),
            "{patch}"
        );
    }
}

/** 5 and 13: PostToolUse journals the executed call and, when it broke preserved code, returns
 * Codex's decision "block" with the report as additionalContext
 */
#[test]
fn post_tool_use_records_evidence_and_blocks_on_violation() {
    let fixture = Fixture::new();
    // Verification follows actual effects: a command that changed nothing has nothing to verify
    let idle = fixture.bash("PostToolUse", "c5", "ls");
    assert!(idle.status.success());
    assert!(String::from_utf8_lossy(&idle.stdout).trim().is_empty());
    // The unmet target is work still to do: reported as context, never as a block
    fixture.write(
        "payment.py",
        &format!(
            "{PAYMENT}
# reviewed
"
        ),
    );
    let pending = fixture.bash("PostToolUse", "c5", "python annotate.py");
    assert!(pending.status.success());
    let advisory = stdout_json(&pending);
    assert!(advisory.get("decision").is_none());
    assert!(advisory["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .contains("PaymentService.calculate was not changed"));

    fixture.write("payment.py", &PAYMENT.replace("amount * 3", "amount * 4"));
    let blocked = fixture.bash("PostToolUse", "c5", "python rewrite.py");
    assert!(
        blocked.status.success(),
        "Codex reads the block from stdout"
    );
    let output = stdout_json(&blocked);
    assert_eq!(output["decision"], "block");
    assert!(output["reason"]
        .as_str()
        .unwrap()
        .contains("Protected function was modified."));
    assert_eq!(output["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .contains("\"violation_type\":\"source_changed\""));

    let journal = fixture.journal("codex-c5");
    assert_eq!(journal.len(), 3);
    assert_eq!(journal[0]["effect"]["clauses_checked"], 0);
    assert_eq!(journal[2]["event"], "post_tool_use");
    assert_eq!(journal[2]["tool"], "Bash");
    assert_eq!(journal[2]["operation"], "execute");
    assert_eq!(journal[2]["verification"], "fail");
    assert_eq!(
        journal[2]["effect"]["files"]["modified"],
        json!(["payment.py"])
    );
    assert!(journal[2]["arguments_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
}

/** 6: SessionStart binds a Codex contract session, prints plain-text developer context, and a
 * later SessionStart (resume) loads the same binding instead of refreshing it
 */
#[test]
fn session_start_creates_and_loads_the_contract_session() {
    let fixture = Fixture::new();
    let start = fixture.codex("SessionStart", "c6", json!({"source": "startup"}));
    assert!(start.status.success());
    let context = text(&start.stdout);
    assert!(context.starts_with("CRANE_CONTEXT_V1"));
    assert!(context.contains("Do not modify:\n- function PaymentService.charge"));
    let session_file = ".crane/runtime/sessions/codex-c6/session.json";
    let bound = fixture.read(session_file);
    let session: Value = serde_json::from_str(&bound).unwrap();
    assert_eq!(session["agent"], "codex");
    assert_eq!(session["provider_session"], "c6");

    fixture.write(".crane/policies/payment.crane", &format!("{POLICY}\n"));
    assert!(fixture
        .codex("SessionStart", "c6", json!({"source": "resume"}))
        .status
        .success());
    assert_eq!(fixture.read(session_file), bound);
}

/** 7, 8, 9, 10: installation adds Crane's hooks to .codex/hooks.json, keeps every existing key
 * and hook, never duplicates on reinstall, leaves config.toml untouched, and skips events already
 * registered inline in config.toml
 */
#[test]
fn install_is_additive_and_idempotent() {
    let fixture = Fixture::new();
    let existing = json!({
        "note": "user setting",
        "hooks": {
            "PreToolUse": [{"matcher": "^Bash$", "hooks": [{"type": "command", "command": "python3 check_bash.py"}]}],
            "Notification": [{"hooks": [{"type": "command", "command": "notify-send done"}]}],
        },
    });
    fixture.write(".codex/hooks.json", &existing.to_string());
    let toml = "model = \"gpt-5-codex\"\n\n[[hooks.SessionEnd]]\n\n[[hooks.SessionEnd.hooks]]\ntype = \"command\"\ncommand = \"crane agent hook --event SessionEnd --profile codex\"\n";
    fixture.write(".codex/config.toml", toml);

    let first = fixture.crane(&["agent", "install", "--profile", "codex"], "");
    assert!(first.status.success(), "{}", text(&first.stderr));
    assert!(text(&first.stdout).contains("/hooks"));
    let installed: Value = serde_json::from_str(&fixture.read(".codex/hooks.json")).unwrap();
    assert_eq!(installed["note"], "user setting");
    assert_eq!(
        installed["hooks"]["Notification"],
        existing["hooks"]["Notification"]
    );
    let pre = installed["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre[0], existing["hooks"]["PreToolUse"][0]);
    assert_eq!(pre[1]["matcher"], "*");
    assert_eq!(
        pre[1]["hooks"][0]["command"],
        "crane agent hook --event PreToolUse --profile codex"
    );
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PermissionRequest",
        "PostToolUse",
        "Stop",
    ] {
        assert_eq!(
            installed["hooks"][event].as_array().unwrap().len(),
            1,
            "{event}"
        );
    }
    assert!(
        installed["hooks"].get("SessionEnd").is_none(),
        "registered inline in config.toml"
    );
    assert_eq!(fixture.read(".codex/config.toml"), toml);

    let before = fixture.read(".codex/hooks.json");
    let second = fixture.crane(&["agent", "install", "--profile", "codex"], "");
    assert!(second.status.success());
    assert!(text(&second.stdout).contains("already installed"));
    assert_eq!(fixture.read(".codex/hooks.json"), before);

    fixture.write(".codex/hooks.json", "[1, 2]");
    let refused = fixture.crane(&["agent", "install", "--profile", "codex"], "");
    assert!(!refused.status.success());
    assert_eq!(fixture.read(".codex/hooks.json"), "[1, 2]");
}

/** 11 and 12: Crane metadata, the Codex hook configuration, and mutating crane commands stay out
 * of the agent's reach through apply_patch, Bash, and other tools
 */
#[test]
fn protected_paths_and_commands_are_denied() {
    let fixture = Fixture::new();
    for path in [
        ".crane/policies/payment.crane",
        ".crane/checkpoints/baseline.json",
        ".codex/hooks.json",
        ".codex/config.toml",
    ] {
        let patch = format!("*** Begin Patch\n*** Add File: {path}\n+{{}}\n*** End Patch\n");
        let output = fixture.patch("PreToolUse", "c11", &patch);
        assert_eq!(output.status.code(), Some(2), "{path}");
        assert!(
            text(&output.stderr).contains("may not modify .crane"),
            "{path}"
        );
    }
    for command in [
        "crane checkpoint --name baseline",
        "crane protect --function A.b",
        "crane target --function A.b",
        "crane agent install --profile codex",
        "echo {} | crane agent hook --event SessionEnd --profile codex",
        "sed -i s/preserve/target/ .crane/policies/payment.crane",
        "cat > .codex/hooks.json",
    ] {
        assert_eq!(
            fixture.bash("PreToolUse", "c11", command).status.code(),
            Some(2),
            "{command}"
        );
    }
    let mcp = fixture.codex(
        "PreToolUse",
        "c11",
        json!({"tool_name": "mcp__fs__write_file", "tool_input": {"path": ".crane/policies/x.crane"}}),
    );
    assert_eq!(mcp.status.code(), Some(2));
    assert!(fixture
        .bash("PreToolUse", "c11", "crane check --agent")
        .status
        .success());
    assert_eq!(fixture.read(".crane/policies/payment.crane"), POLICY);
}

/** 13: PermissionRequest denies with Codex's decision object when the contract forbids the call,
 * and prints nothing otherwise so the user's own approval prompt decides
 */
#[test]
fn permission_request_uses_codex_decision_objects() {
    let fixture = Fixture::new();
    let denied = fixture.patch(
        "PermissionRequest",
        "c13",
        &update("return amount * 3", "return amount * 4"),
    );
    assert!(denied.status.success());
    let output = stdout_json(&denied);
    assert_eq!(
        output["hookSpecificOutput"]["hookEventName"],
        "PermissionRequest"
    );
    assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
    assert!(output["hookSpecificOutput"]["decision"]["message"]
        .as_str()
        .unwrap()
        .contains("PaymentService.charge"));

    let deferred = fixture.bash("PermissionRequest", "c13", "rm -rf build");
    assert!(deferred.status.success());
    assert!(
        deferred.stdout.is_empty(),
        "Crane never approves on the user's behalf"
    );
    let guarded = fixture.bash("PermissionRequest", "c13", "rm -rf .crane");
    assert_eq!(
        stdout_json(&guarded)["hookSpecificOutput"]["decision"]["behavior"],
        "deny"
    );
    let events = fixture.journal("codex-c13");
    assert!(events
        .iter()
        .all(|event| event["event"] == "permission_request"));
    assert_eq!(events[1]["decision"], "allow");
}

/** 13 and loop prevention: Stop continues the turn with decision "block" while a target is unmet,
 * lets a forced retry finish with a systemMessage, and ends silently once the contract holds
 */
#[test]
fn stop_uses_codex_continuation_without_looping() {
    let fixture = Fixture::new();
    let first = fixture.codex(
        "Stop",
        "c14",
        json!({"stop_hook_active": false, "last_assistant_message": "done"}),
    );
    assert!(first.status.success());
    let output = stdout_json(&first);
    assert_eq!(output["decision"], "block");
    assert!(output["reason"]
        .as_str()
        .unwrap()
        .contains("target_unchanged"));

    let retry = fixture.codex("Stop", "c14", json!({"stop_hook_active": true}));
    assert!(retry.status.success());
    let output = stdout_json(&retry);
    assert!(output.get("decision").is_none());
    assert!(output["systemMessage"]
        .as_str()
        .unwrap()
        .contains("was not changed"));

    fixture.write("payment.py", &PAYMENT.replace("amount + 1", "amount + 2"));
    let done = fixture.codex("Stop", "c14", json!({"stop_hook_active": false}));
    assert!(done.status.success());
    assert!(done.stdout.is_empty());
    let end = fixture.codex("SessionEnd", "c14", json!({"reason": "other"}));
    assert!(end.status.success());
    assert!(end.stdout.is_empty());
    let show = fixture.crane(&["agent", "session", "show", "codex-c14"], "");
    let shown: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(shown["lifecycle"], "closed");
    assert_eq!(shown["attestation"]["final_status"], "PASS");
    assert_eq!(shown["attestation"]["agent_id"], "codex");
}

/** 14: the CLI help documents the Codex profile */
#[test]
fn help_documents_the_codex_profile() {
    let fixture = Fixture::bare();
    let help = text(&fixture.crane(&["help"], "").stdout);
    assert!(help.contains("agent install --profile claude|codex"));
    assert!(help.contains("Codex hook JSON for --profile codex"));
    assert!(help.contains("permission-request"));
    assert!(help.contains("agent init --profile codex also initializes .crane"));
}

/** 15: end to end from a bare repository: agent init --profile codex initializes Crane, installs
 * the hooks, and verifies; then a Codex session is bound, a forbidden patch is denied, an allowed
 * one runs, post-tool-use verifies, and stop reconciles to PASS
 */
#[test]
fn codex_end_to_end_from_agent_init() {
    let fixture = Fixture::bare();
    let init = fixture.crane(&["agent", "init", "--profile", "codex"], "");
    assert!(init.status.success(), "{}", text(&init.stderr));
    let init_output = text(&init.stdout);
    assert!(init_output.contains("Installed Crane Codex hooks"));
    assert!(init_output.contains("Activated Crane agent adapter profile 'codex'."));
    let hooks: Value = serde_json::from_str(&fixture.read(".codex/hooks.json")).unwrap();
    assert_eq!(hooks["hooks"].as_object().unwrap().len(), 7);
    let again = fixture.crane(&["agent", "init", "--profile", "codex"], "");
    assert!(again.status.success());
    assert!(text(&again.stdout).contains("already installed"));

    fixture.protect();
    let hook = |event: &str| -> String {
        hooks["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        hook("PreToolUse"),
        "crane agent hook --event PreToolUse --profile codex"
    );
    assert!(fixture
        .codex("SessionStart", "c15", json!({"source": "startup"}))
        .status
        .success());
    assert_eq!(
        fixture
            .patch(
                "PreToolUse",
                "c15",
                &update("return amount * 3", "return amount * 9")
            )
            .status
            .code(),
        Some(2)
    );
    let patch = update("return amount + 1", "return amount + 2");
    assert!(fixture.patch("PreToolUse", "c15", &patch).status.success());
    fixture.write("payment.py", &PAYMENT.replace("amount + 1", "amount + 2"));
    let post = fixture.patch("PostToolUse", "c15", &patch);
    assert!(post.status.success());
    assert!(post.stdout.is_empty(), "{}", text(&post.stdout));
    let stop = fixture.codex("Stop", "c15", json!({"stop_hook_active": false}));
    assert!(stop.status.success());
    assert!(stop.stdout.is_empty(), "{}", text(&stop.stdout));
    let verify = fixture.crane(&["agent", "verify", "--profile", "codex"], "");
    assert!(verify.status.success());
    let events = fixture
        .journal("codex-c15")
        .iter()
        .map(|event| {
            format!(
                "{}:{}",
                event["event"].as_str().unwrap(),
                event["decision"].as_str().unwrap_or("-")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        events,
        [
            "session_start:-",
            "pre_tool_use:deny",
            "pre_tool_use:allow",
            "post_tool_use:-",
            "stop:-"
        ]
    );
}
