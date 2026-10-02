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

/** Path of the payment service */
const SERVICE: &str = "services/payments/src/main/java/com/acme/payments/PaymentService.java";

/** Payment service source */
const PAYMENT: &str = "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n";

/** Path of the billing service, unzoned and unprotected */
const BILLING: &str = "services/billing/src/main/java/com/acme/billing/InvoiceService.java";

/** Path of the authentication module, in a restricted zone */
const LOGIN: &str = "auth/login.py";

/** Repository files */
const FILES: &[(&str, &str)] = &[
    ("README.md", "# Shop\n"),
    ("services/payments/pom.xml", "<project/>\n"),
    (SERVICE, PAYMENT),
    ("services/billing/pom.xml", "<project/>\n"),
    (
        BILLING,
        "package com.acme.billing;\n\npublic class InvoiceService {\n    public double total(double amount) {\n        return amount * 1.0;\n    }\n}\n",
    ),
    (
        LOGIN,
        "def authenticate(user):\n    return user\n\n\ndef logout(user):\n    return None\n",
    ),
];

/** Zones: Authentication is Restricted */
const ZONES: &str = "zone authentication {\n    criticality restricted;\n    autonomy observe;\n    select subsystem auth;\n}\n";

/** The permanent policies: charge and authenticate are preserved */
const POLICY: &str = "policy core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n    preserve --function authenticate;\n}\n";

/** A temporary repository with Crane initialized, a restricted zone, and a policy, removed on drop
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
            "crane-autonomy-{suffix}-{}",
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
            vec!["config", "user.name", "Crane Autonomy Test"],
            vec!["config", "core.autocrlf", "false"],
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
        repository.write(".crane/policies/core.crane", POLICY);
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

    /** Send one Claude hook event for a session
     * Input
        - event: &str - hook event
        - session: &str - provider session id
        - payload: Value - event payload (session_id is added)
        - options: &[&str] - session options such as --autonomy
     * Output
        - Output
    */
    fn hook(&self, event: &str, session: &str, mut payload: Value, options: &[&str]) -> Output {
        payload["session_id"] = json!(session);
        let mut args = vec!["agent", "hook", "--event", event, "--profile", "claude"];
        args.extend_from_slice(options);
        self.crane(&args, &[], &payload.to_string())
    }

    /** Start a Claude session through its session-start hook
     * Input
        - session: &str - provider session id
        - options: &[&str] - session options
     * Output
        - None
    */
    fn start(&self, session: &str, options: &[&str]) {
        let output = self.hook(
            "session-start",
            session,
            json!({"source": "startup"}),
            options,
        );
        assert!(output.status.success(), "{}", text(&output.stderr));
    }

    /** Ask the Claude hook about an edit
     * Input
        - session: &str - provider session id
        - path: &str - file
        - old: &str - text replaced
        - new: &str - replacement
     * Output
        - Output
    */
    fn edit(&self, session: &str, path: &str, old: &str, new: &str) -> Output {
        self.hook(
            "pre-tool-use",
            session,
            json!({"tool_name": "Edit", "tool_input": {
                "file_path": self.root.join(path).to_string_lossy(),
                "old_string": old,
                "new_string": new,
            }}),
            &[],
        )
    }

    /** Ask the Claude hook about an edit of the billing service
     * Input
        - session: &str - provider session id
     * Output
        - Output
    */
    fn billing_edit(&self, session: &str) -> Output {
        self.edit(
            session,
            BILLING,
            "        return amount * 1.0;",
            "        return amount * 2.0;",
        )
    }

    /** Report a shell command to the post-tool hook
     * Input
        - session: &str - provider session id
        - command: &str - command
     * Output
        - Output
    */
    fn after_shell(&self, session: &str, command: &str) -> Output {
        self.hook(
            "post-tool-use",
            session,
            json!({"tool_name": "Bash", "tool_input": {"command": command}}),
            &[],
        )
    }

    /** Read a session's autonomy status
     * Input
        - id: &str - Crane session id
     * Output
        - Value
    */
    fn status(&self, id: &str) -> Value {
        serde_json::from_str(&self.human(&["autonomy", "status", id, "--json"])).unwrap()
    }

    /** Read a session's autonomy history
     * Input
        - id: &str - Crane session id
     * Output
        - Vec<Value>
    */
    fn history(&self, id: &str) -> Vec<Value> {
        let value: Value =
            serde_json::from_str(&self.human(&["autonomy", "history", id, "--json"])).unwrap();
        value["history"].as_array().unwrap().clone()
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

/** Whether an Edit was sent for human approval
 * Input
    - output: &Output - hook output
 * Output
    - bool
*/
fn asks(output: &Output) -> bool {
    text(&output.stdout).contains("\"permissionDecision\":\"ask\"")
}

/** Whether an Edit was allowed outright
 * Input
    - output: &Output - hook output
 * Output
    - bool
*/
fn allowed(output: &Output) -> bool {
    output.status.success() && !asks(output)
}

/** Status and history: a human promotes an assisted session one step, which changes what it may
 * do; status shows both dimensions and the next legal promotion; history journals each transition
 */
#[test]
fn human_promotion_is_journaled_and_takes_effect() {
    let repository = Repository::new();
    repository.start("p1", &["--autonomy", "assisted"]);
    assert!(asks(&repository.billing_edit("p1")));
    let status = repository.status("claude-p1");
    assert_eq!(status["autonomy"], "assisted");
    assert_eq!(status["safety"], "active");
    assert_eq!(status["promotion"]["to"], "delegated");
    assert!(status["precedence"][1]
        .as_str()
        .unwrap()
        .starts_with("contract"));

    let promoted = repository.human(&[
        "autonomy",
        "promote",
        "claude-p1",
        "--to",
        "delegated",
        "--reason",
        "trusted",
    ]);
    assert!(
        promoted.contains("autonomy delegated, safety active"),
        "{promoted}"
    );
    assert!(allowed(&repository.billing_edit("p1")));
    assert_eq!(
        repository.status("claude-p1")["initial_autonomy"],
        "assisted"
    );

    let history = repository.history("claude-p1");
    assert_eq!(history[0]["kind"], "initial");
    assert_eq!(history[0]["state"]["autonomy"], "assisted");
    let transition = history.last().unwrap();
    assert_eq!(transition["kind"], "transition");
    assert_eq!(transition["trigger"], "promote");
    assert_eq!(transition["actor"], "human");
    assert_eq!(transition["note"], "trusted");
    assert_eq!(
        transition["changes"],
        json!([{"dimension": "autonomy", "from": "assisted", "to": "delegated"}])
    );
    let human = repository.human(&["autonomy", "history", "claude-p1"]);
    assert!(
        human.contains(
            "transition: promote to delegated by human: autonomy assisted -> delegated (trusted)"
        ),
        "{human}"
    );
    let overview = repository.human(&["autonomy", "status"]);
    assert!(overview.contains("AUTONOMY POLICY (defaults"), "{overview}");
    assert!(
        overview.contains("autonomy observe -> assisted"),
        "{overview}"
    );
    assert!(
        overview.contains("claude-p1  active  autonomy delegated  safety active"),
        "{overview}"
    );

    // Demotion is immediate and needs nothing
    repository.human(&["autonomy", "demote", "claude-p1", "--to", "observe"]);
    let denied = repository.billing_edit("p1");
    assert_eq!(denied.status.code(), Some(2));
    assert!(text(&denied.stderr).contains("autonomy mode is observe"));
}

/** Illegal transitions are refused and journaled: skipping a step, promoting past the policy
 * maximum (which also caps what a session may start with), demoting upwards
 */
#[test]
fn illegal_transitions_are_refused() {
    let repository = Repository::new();
    repository.start("i1", &["--autonomy", "observe"]);
    let skipped = repository.crane(
        &["autonomy", "promote", "claude-i1", "--to", "delegated"],
        &[],
        "",
    );
    assert!(!skipped.status.success());
    assert!(text(&skipped.stderr).contains("one step at a time: observe can only become assisted"));
    let upward = repository.crane(
        &["autonomy", "demote", "claude-i1", "--to", "assisted"],
        &[],
        "",
    );
    assert!(text(&upward.stderr).contains("demotion must lower autonomy"));
    let rejected = repository
        .history("claude-i1")
        .into_iter()
        .filter(|entry| entry["kind"] == "rejected")
        .count();
    assert_eq!(rejected, 2);
    assert_eq!(repository.status("claude-i1")["autonomy"], "observe");

    repository.write(".crane/autonomy.json", "{\"max_autonomy\": \"delegated\"}");
    repository.start("i2", &["--autonomy", "autonomous"]);
    let status = repository.status("claude-i2");
    assert_eq!(
        status["autonomy"], "delegated",
        "the session start is capped"
    );
    assert_eq!(status["promotion"], Value::Null);
    let capped = repository.crane(
        &["autonomy", "promote", "claude-i2", "--to", "autonomous"],
        &[],
        "",
    );
    assert!(text(&capped.stderr).contains("allows at most delegated"));
}

/** An agent can never change its own autonomy: the CLI refuses in an agent environment and the
 * hook denies the command; either attempt is journaled and quarantines the session
 */
#[test]
fn agents_cannot_change_their_own_autonomy() {
    let repository = Repository::new();
    repository.start("e1", &["--autonomy", "assisted"]);
    let attempt = repository.crane(
        &["autonomy", "promote", "claude-e1", "--to", "delegated"],
        &[("CLAUDECODE", "1")],
        "",
    );
    assert!(!attempt.status.success());
    assert!(text(&attempt.stderr).contains("refuses to run in an agent environment"));
    let status = repository.status("claude-e1");
    assert_eq!(status["autonomy"], "assisted");
    assert_eq!(status["safety"], "quarantined");
    assert_eq!(
        status["safety_reason"],
        "critical violation: self_escalation"
    );
    let history = repository.history("claude-e1");
    assert!(history
        .iter()
        .any(|entry| entry["kind"] == "rejected" && entry["actor"] == "agent"));

    repository.start("e2", &["--autonomy", "delegated"]);
    let shell = repository.hook(
        "pre-tool-use",
        "e2",
        json!({"tool_name": "Bash", "tool_input": {"command": "crane autonomy promote claude-e2 --to autonomous"}}),
        &[],
    );
    assert_eq!(shell.status.code(), Some(2));
    let status = repository.status("claude-e2");
    assert_eq!(status["autonomy"], "delegated");
    assert_eq!(status["safety"], "quarantined");
    let denied = repository.billing_edit("e2");
    assert_eq!(denied.status.code(), Some(2));
    assert!(text(&denied.stderr).contains("quarantined"));
}

/** A violation degrades the session (changes then need approval, whatever its autonomy); a clean
 * full validation after the repair is the verified repair that recovers it
 */
#[test]
fn violation_degrades_and_verified_repair_recovers() {
    let repository = Repository::new();
    repository.start("v1", &["--autonomy", "autonomous"]);
    assert!(allowed(&repository.billing_edit("v1")));
    repository.write(SERVICE, &PAYMENT.replace("fee(amount) + amount", "amount"));
    let caught = repository.after_shell("v1", "python rewrite.py");
    assert_eq!(caught.status.code(), Some(2), "{}", text(&caught.stdout));
    let status = repository.status("claude-v1");
    assert_eq!(status["safety"], "degraded");
    assert_eq!(
        status["autonomy"], "autonomous",
        "safety never changes autonomy"
    );
    assert_eq!(status["effective_autonomy"], "assisted");
    assert_eq!(status["promotion"]["blocked"], "safety is degraded");
    let asked = repository.billing_edit("v1");
    assert!(asks(&asked), "{}", text(&asked.stdout));
    assert!(text(&asked.stdout).contains("the session is degraded"));

    repository.write(SERVICE, PAYMENT);
    repository.human(&["agent", "session", "verify", "claude-v1", "--level", "full"]);
    let status = repository.status("claude-v1");
    assert_eq!(status["safety"], "active", "{status}");
    assert!(allowed(&repository.billing_edit("v1")));
    let kinds = repository
        .history("claude-v1")
        .iter()
        .filter(|entry| entry["kind"] == "transition")
        .map(|entry| {
            format!(
                "{} {}",
                entry["trigger"].as_str().unwrap(),
                entry["changes"][0]["to"].as_str().unwrap()
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["violation degraded", "evidence active"]);
}

/** A critical violation (an exhausted budget) quarantines; a human approval alone is not enough,
 * and the quarantine lifts only once a configured set is complete (approval + new risk budget)
 */
#[test]
fn quarantine_recovery_needs_a_complete_condition_set() {
    let repository = Repository::new();
    repository.start("q1", &["--autonomy", "autonomous", "--max-actions", "1"]);
    assert!(allowed(&repository.billing_edit("q1")));
    assert_eq!(repository.billing_edit("q1").status.code(), Some(2));
    let status = repository.status("claude-q1");
    assert_eq!(status["safety"], "quarantined");
    assert_eq!(
        status["safety_reason"],
        "critical violation: budget_exhausted"
    );
    assert_eq!(status["effective_autonomy"], "observe");

    let resumed = repository.human(&["agent", "session", "resume", "claude-q1"]);
    assert!(resumed.contains("Still quarantined"), "{resumed}");
    let status = repository.status("claude-q1");
    assert_eq!(status["safety"], "quarantined");
    assert_eq!(status["evidence"], json!(["human_approval"]));
    assert_eq!(
        status["recovery"]["missing"],
        json!([["verified_repair"], ["new_risk_budget"], ["new_session"]])
    );
    let human = repository.human(&["autonomy", "status", "claude-q1"]);
    assert!(
        human.contains("missing:   verified_repair | new_risk_budget | new_session"),
        "{human}"
    );

    repository.human(&["agent", "session", "extend", "claude-q1", "--actions", "5"]);
    assert_eq!(repository.status("claude-q1")["safety"], "active");
    // The critical violation also zeroed the risk budget: autonomy needs a refill as well
    assert_eq!(
        repository.status("claude-q1")["budget"]["budget_current"],
        0
    );
    assert!(asks(&repository.billing_edit("q1")));
    repository.human(&[
        "autonomy",
        "refill",
        "claude-q1",
        "--amount",
        "50",
        "--reason",
        "reviewed",
        "--approver",
        "lead",
        "--expires",
        "1h",
    ]);
    assert!(allowed(&repository.billing_edit("q1")));
}

/** A quarantine follows the agent into its next session on the same task, which only recovers
 * with a human approval; a session a human starts is approved from the outset
 */
#[test]
fn quarantine_is_inherited_by_the_next_session() {
    let repository = Repository::new();
    repository.start("h1", &[]);
    repository.human(&[
        "agent",
        "session",
        "quarantine",
        "claude-h1",
        "--reason",
        "suspicious",
    ]);
    repository.start("h2", &[]);
    let status = repository.status("claude-h2");
    assert_eq!(status["safety"], "quarantined");
    assert!(status["safety_reason"]
        .as_str()
        .unwrap()
        .contains("inherited from quarantined session claude-h1"));
    assert_eq!(status["evidence"], json!(["new_session"]));
    assert_eq!(repository.billing_edit("h2").status.code(), Some(2));
    repository.human(&["autonomy", "approve", "claude-h2"]);
    assert_eq!(repository.status("claude-h2")["safety"], "active");

    repository.human(&["agent", "session", "quarantine", "claude-h2"]);
    repository.human(&[
        "agent",
        "session",
        "start",
        "--profile",
        "claude",
        "--session",
        "h3",
    ]);
    let status = repository.status("claude-h3");
    assert_eq!(status["safety"], "active");
    let history = repository.history("claude-h3");
    let triggers = history
        .iter()
        .skip(1)
        .map(|entry| entry["trigger"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert_eq!(triggers, ["quarantine", "evidence", "evidence"]);

    // Another agent, or the same agent on another task, starts clean
    let codex = repository.crane(
        &[
            "agent",
            "hook",
            "--event",
            "SessionStart",
            "--profile",
            "codex",
        ],
        &[],
        &json!({"session_id": "h4", "hook_event_name": "SessionStart", "source": "startup"})
            .to_string(),
    );
    assert!(codex.status.success(), "{}", text(&codex.stderr));
    assert_eq!(repository.status("codex-h4")["safety"], "active");
}

/** The policy hierarchy outranks autonomy: a restricted zone stays closed to an autonomous agent
 * until an organizational grant opens it, only as far as the grant allows, only for the granted
 * task, and never past the contract
 */
#[test]
fn restricted_zones_need_an_organizational_grant() {
    let repository = Repository::new();
    let logout =
        |session: &str| repository.edit(session, LOGIN, "    return None", "    return False");
    repository.start("r1", &["--autonomy", "autonomous"]);
    let denied = logout("r1");
    assert_eq!(denied.status.code(), Some(2));
    assert!(
        text(&denied.stderr).contains("zones authentication (restricted"),
        "{}",
        text(&denied.stderr)
    );

    repository.write(
        ".crane/autonomy.json",
        &json!({"grants": [{"zone": "authentication", "autonomy": "delegated", "task": "SEC-9", "approved_by": "security-lead", "reason": "token rotation"}]}).to_string(),
    );
    repository.start("r2", &["--autonomy", "autonomous"]);
    assert_eq!(
        logout("r2").status.code(),
        Some(2),
        "the grant is limited to task SEC-9"
    );

    repository.write(
        ".crane/autonomy.json",
        &json!({"grants": [{"zone": "authentication", "autonomy": "assisted", "approved_by": "security-lead", "reason": "reviews"}]}).to_string(),
    );
    repository.start("r3", &["--autonomy", "autonomous"]);
    assert!(
        asks(&logout("r3")),
        "an assisted grant needs approval for each change"
    );

    repository.write(
        ".crane/autonomy.json",
        &json!({"grants": [{"zone": "authentication", "autonomy": "delegated", "approved_by": "security-lead", "reason": "token rotation"}]}).to_string(),
    );
    repository.start("r4", &["--autonomy", "autonomous"]);
    assert!(allowed(&logout("r4")), "{}", text(&logout("r4").stderr));
    let status = repository.status("claude-r4");
    assert_eq!(status["restricted_grants"][0]["zone"], "authentication");
    assert_eq!(
        status["restricted_grants"][0]["grant"]["approved_by"],
        "security-lead"
    );
    let contract = repository.edit("r4", LOGIN, "    return user", "    return None");
    assert_eq!(
        contract.status.code(),
        Some(2),
        "the contract outranks the grant"
    );
    assert!(text(&contract.stderr).contains("authenticate"));

    // The grant is bound at session start; editing the policy file later changes nothing
    repository.write(".crane/autonomy.json", "{}");
    assert!(allowed(&logout("r4")));
}

/** An invalid autonomy policy fails closed: no session starts under it, and status explains */
#[test]
fn invalid_policy_fails_closed() {
    let repository = Repository::new();
    repository.write(
        ".crane/autonomy.json",
        "{\"recovery\": {\"quarantined\": {\"any_of\": [[\"verified_repair\"]]}}}",
    );
    let start = repository.hook("session-start", "x1", json!({"source": "startup"}), &[]);
    assert!(!start.status.success());
    let status = repository.crane(&["autonomy", "status"], &[], "");
    assert!(!status.status.success());
    assert!(text(&status.stderr).contains("never lifted without a human"));
}
