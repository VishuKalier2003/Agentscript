use std::fs;
use std::io::Write;
use std::path::PathBuf;
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

/** Path of the payment service, in a sensitive zone */
const SERVICE: &str = "services/payments/src/main/java/com/acme/payments/PaymentService.java";

/** Payment service source */
const PAYMENT: &str = "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n";

/** Path of the notes file, unzoned and unprotected: a routine write */
const NOTES: &str = "docs/notes.md";

/** Repository files */
const FILES: &[(&str, &str)] = &[
    ("README.md", "# Shop\n"),
    ("services/payments/pom.xml", "<project/>\n"),
    (SERVICE, PAYMENT),
    (NOTES, "notes\n"),
    (
        "lib/money/round.py",
        "def round_cents(value):\n    return round(value, 2)\n",
    ),
];

/** Zones: Payments is Sensitive (delegated agents may change it, at a higher price) */
const ZONES: &str = "zone payments {\n    criticality sensitive;\n    autonomy delegated;\n    select subsystem payments;\n}\n";

/** The permanent policy */
const POLICY: &str =
    "policy core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n";

/** A small budget so tests reach every state quickly: delegated sessions get 10 points; a routine
 * write costs 3; violations cost 4; two compliant actions in a row regenerate 1 (once) */
const MODEL: &str = "{\"max\": {\"delegated\": 10, \"autonomous\": 20}, \"penalties\": {\"violation\": 4}, \"compliance\": {\"every\": 2, \"amount\": 1, \"cap\": 1}, \"max_refill_expiry_seconds\": 86400}";

/** A temporary repository with Crane initialized, a critical zone, a policy, and a small budget
 * model, removed on drop
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
            "crane-budget-{suffix}-{}",
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
            vec!["config", "user.name", "Crane Budget Test"],
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
        repository.human(&["init"]);
        repository.human(&["checkpoint", "--name", "baseline"]);
        repository.write(".crane/zones/org.zone", ZONES);
        repository.write(".crane/policies/core.crane", POLICY);
        repository.write(".crane/budget.json", MODEL);
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
     * Output
        - Output
    */
    fn hook(&self, event: &str, session: &str, mut payload: Value) -> Output {
        payload["session_id"] = json!(session);
        self.crane(
            &["agent", "hook", "--event", event, "--profile", "claude"],
            &[],
            &payload.to_string(),
        )
    }

    /** Start a Claude session with options
     * Input
        - session: &str - provider session id
        - options: &[&str] - session options
     * Output
        - None
    */
    fn start(&self, session: &str, options: &[&str]) {
        let mut args = vec![
            "agent",
            "hook",
            "--event",
            "session-start",
            "--profile",
            "claude",
        ];
        args.extend_from_slice(options);
        let output = self.crane(
            &args,
            &[],
            &json!({"session_id": session, "source": "startup"}).to_string(),
        );
        assert!(output.status.success(), "{}", text(&output.stderr));
    }

    /** Ask to write a file with the Write tool
     * Input
        - session: &str - provider session id
        - path: &str - file
        - content: &str - new content
     * Output
        - (Output, Value) the hook output and the payload, for the post-tool event
    */
    fn ask_write(&self, session: &str, path: &str, content: &str) -> (Output, Value) {
        let payload = json!({"tool_name": "Write", "tool_input": {"file_path": self.root.join(path).to_string_lossy(), "content": content}});
        (self.hook("pre-tool-use", session, payload.clone()), payload)
    }

    /** Write a file as the agent: authorized, written, then reported, and require it to have been
     * allowed outright
     * Input
        - session: &str - provider session id
        - path: &str - file
        - content: &str - new content
     * Output
        - None
    */
    fn spend(&self, session: &str, path: &str, content: &str) {
        let (before, payload) = self.ask_write(session, path, content);
        assert!(
            allowed(&before),
            "{}{}",
            text(&before.stdout),
            text(&before.stderr)
        );
        self.write(path, content);
        let after = self.hook("post-tool-use", session, payload);
        assert!(after.status.success(), "{}", text(&after.stdout));
    }

    /** Read a session's budget
     * Input
        - id: &str - Crane session id
     * Output
        - Value
    */
    fn budget(&self, id: &str) -> Value {
        serde_json::from_str(&self.human(&["autonomy", "budget", id, "--json"])).unwrap()
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

    /** Refill a session as a human
     * Input
        - id: &str - Crane session id
        - amount: &str - points
        - expires: &str - duration
     * Output
        - Output
    */
    fn refill(&self, id: &str, amount: &str, expires: &str) -> Output {
        self.crane(
            &[
                "autonomy",
                "refill",
                id,
                "--amount",
                amount,
                "--reason",
                "reviewed the plan",
                "--approver",
                "team-lead",
                "--expires",
                expires,
            ],
            &[],
            "",
        )
    }

    /** Read every file under .crane/policies and the model files, to prove nothing permanent changed
     * Input
        - None
     * Output
        - Vec<(String, String)>
    */
    fn permanent(&self) -> Vec<(String, String)> {
        let mut files = Vec::new();
        for entry in fs::read_dir(self.root.join(".crane/policies")).unwrap() {
            let path = entry.unwrap().path();
            files.push((
                path.display().to_string(),
                fs::read_to_string(&path).unwrap(),
            ));
        }
        for name in ["budget.json", "zones/org.zone"] {
            files.push((
                name.into(),
                fs::read_to_string(self.root.join(".crane").join(name)).unwrap(),
            ));
        }
        files
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

/** Whether a hook sent the action for human approval
 * Input
    - output: &Output - hook output
 * Output
    - bool
*/
fn asks(output: &Output) -> bool {
    text(&output.stdout).contains("\"permissionDecision\":\"ask\"")
}

/** Whether a hook allowed the action outright
 * Input
    - output: &Output - hook output
 * Output
    - bool
*/
fn allowed(output: &Output) -> bool {
    output.status.success() && !asks(output)
}

/** Consumption: an authorized write reserves its price on the pre-tool event (current stays,
 * available drops), the executed write consumes it, a turn boundary releases reservations of
 * actions that never ran, and riskier resources cost more
 */
#[test]
fn consumption() {
    let repository = Repository::new();
    repository.start("c1", &[]);
    let budget = repository.budget("claude-c1");
    assert_eq!(
        (
            budget["budget_current"].as_u64(),
            budget["budget_max"].as_u64()
        ),
        (Some(10), Some(10))
    );

    let (asked, payload) = repository.ask_write("c1", NOTES, "first\n");
    assert!(allowed(&asked));
    let budget = repository.budget("claude-c1");
    assert_eq!(budget["budget_reserved"], 3);
    assert_eq!(budget["budget_available"], 7);
    assert_eq!(budget["budget_current"], 10);
    let reserve = &budget["budget_events"][0];
    assert_eq!(reserve["kind"], "reserve");
    assert_eq!(reserve["factors"]["criticality"], "routine");
    assert_eq!(reserve["factors"]["scope"], "repository");

    repository.write(NOTES, "first\n");
    assert!(repository
        .hook("post-tool-use", "c1", payload)
        .status
        .success());
    let budget = repository.budget("claude-c1");
    assert_eq!(budget["budget_reserved"], 0);
    assert_eq!(budget["budget_consumed"], 3);
    assert_eq!(budget["budget_current"], 7);

    // Authorized but never run: released at the next turn boundary
    let (asked, _) = repository.ask_write("c1", NOTES, "second\n");
    assert!(allowed(&asked));
    assert_eq!(repository.budget("claude-c1")["budget_reserved"], 3);
    assert!(repository
        .hook("user-prompt-submit", "c1", json!({"prompt": "next"}))
        .status
        .success());
    let budget = repository.budget("claude-c1");
    assert_eq!(budget["budget_reserved"], 0);
    assert_eq!(budget["budget_current"], 7);

    // A sensitive resource costs more than the routine notes, and is reserved as such
    let (asked, _) = repository.ask_write(
        "c1",
        "services/payments/src/main/java/com/acme/payments/Ledger.java",
        "class Ledger {}\n",
    );
    assert!(allowed(&asked), "{}", text(&asked.stderr));
    let reserve = repository.budget("claude-c1")["budget_events"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|event| event["kind"] == "reserve")
        .unwrap()
        .clone();
    assert_eq!(reserve["factors"]["criticality"], "sensitive");
    assert!(reserve["amount"].as_u64().unwrap() > 3);
    // Reads cost nothing
    let read = repository.hook("pre-tool-use", "c1", json!({"tool_name": "Read", "tool_input": {"file_path": repository.root.join(NOTES).to_string_lossy()}}));
    assert!(read.status.success());
}

/** Exhausted budget: an action the budget cannot cover needs human approval, the session is
 * degraded (supervised), a refill is requested once, and promotion is blocked; a human refill
 * restores autonomy without ever exceeding the maximum
 */
#[test]
fn exhausted_budget_and_human_refill() {
    let repository = Repository::new();
    repository.start("e1", &[]);
    for round in 0..3 {
        repository.spend("e1", NOTES, &format!("round {round}\n"));
    }
    // 10 - 3 x 3 consumed + 1 regenerated for two compliant actions in a row
    assert_eq!(repository.budget("claude-e1")["budget_current"], 2);
    let (asked, _) = repository.ask_write("e1", NOTES, "one more\n");
    assert!(asks(&asked), "{}", text(&asked.stdout));
    assert!(text(&asked.stdout)
        .contains("risk budget exhausted: this action costs 3 and 2 of 10 is available"));
    let status = repository.status("claude-e1");
    assert_eq!(status["safety"], "degraded");
    assert_eq!(status["safety_reason"], "violation: risk_budget_exhausted");
    assert_eq!(status["budget"]["refill_requested"], true);
    let promote = repository.crane(
        &["autonomy", "promote", "claude-e1", "--to", "autonomous"],
        &[],
        "",
    );
    assert!(
        text(&promote.stderr).contains("risk budget is exhausted"),
        "{}",
        text(&promote.stderr)
    );
    // Asking again requests nothing new and is not another violation
    assert!(asks(&repository.ask_write("e1", NOTES, "and another\n").0));
    let requests = repository.budget("claude-e1")["budget_events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "refill_requested")
        .count();
    assert_eq!(requests, 1);
    assert_eq!(repository.status("claude-e1")["violations_in_incident"], 1);

    let before = repository.permanent();
    let session_file = fs::read_to_string(
        repository
            .root
            .join(".crane/runtime/sessions/claude-e1/session.json"),
    )
    .unwrap();
    let refilled = repository.refill("claude-e1", "500", "2h");
    assert!(refilled.status.success(), "{}", text(&refilled.stderr));
    assert!(
        text(&refilled.stdout).contains("budget 10 of 10"),
        "{}",
        text(&refilled.stdout)
    );
    assert_eq!(
        repository.permanent(),
        before,
        "a refill never mutates permanent policies"
    );
    assert_eq!(
        fs::read_to_string(
            repository
                .root
                .join(".crane/runtime/sessions/claude-e1/session.json")
        )
        .unwrap(),
        session_file,
        "nor the session binding"
    );
    let budget = repository.budget("claude-e1");
    assert_eq!(budget["refilled"], 8, "a refill never exceeds budget_max");
    assert_eq!(budget["refills"][0]["approver"], "team-lead");
    assert_eq!(budget["refill_requested"], false);
    assert_eq!(repository.status("claude-e1")["safety"], "active");
    repository.spend("e1", NOTES, "after the refill\n");

    for (args, message) in [
        (
            vec![
                "--amount",
                "0",
                "--reason",
                "r",
                "--approver",
                "a",
                "--expires",
                "1h",
            ],
            "at least 1",
        ),
        (
            vec![
                "--amount",
                "5",
                "--reason",
                "r",
                "--approver",
                "a",
                "--expires",
                "30d",
            ],
            "between 1 second and 86400",
        ),
        (
            vec!["--amount", "5", "--reason", "r", "--expires", "1h"],
            "refill needs --approver",
        ),
        (
            vec!["--amount", "5", "--approver", "a", "--expires", "1h"],
            "refill needs --reason",
        ),
    ] {
        let mut full = vec!["autonomy", "refill", "claude-e1"];
        full.extend(args);
        let refused = repository.crane(&full, &[], "");
        assert!(
            text(&refused.stderr).contains(message),
            "{full:?}: {}",
            text(&refused.stderr)
        );
    }
}

/** Expiry: what is left of a refill expires at its expiry */
#[test]
fn refills_expire() {
    let repository = Repository::new();
    repository.start("x1", &[]);
    for round in 0..3 {
        repository.spend("x1", NOTES, &format!("round {round}\n"));
    }
    assert!(repository.refill("claude-x1", "6", "2s").status.success());
    assert_eq!(repository.budget("claude-x1")["budget_current"], 8);
    std::thread::sleep(Duration::from_secs(3));
    let budget = repository.budget("claude-x1");
    assert_eq!(budget["budget_current"], 2);
    assert_eq!(budget["refill_expired"], 6);
    assert_eq!(budget["refills"], json!([]));
    assert!(asks(&repository.ask_write("x1", NOTES, "late\n").0));
}

/** Regeneration: controlled events add budget once each and never above the maximum; sustained
 * compliance earns its configured points; nothing regenerates a full budget
 */
#[test]
fn regeneration() {
    let repository = Repository::new();
    repository.start("g1", &[]);
    let full = repository.human(&[
        "autonomy",
        "credit",
        "claude-g1",
        "--event",
        "merge",
        "--reference",
        "PR-1",
        "--approver",
        "lead",
    ]);
    assert!(full.contains("Nothing credited"), "{full}");

    repository.spend("g1", NOTES, "one\n");
    repository.spend("g1", NOTES, "two\n");
    let budget = repository.budget("claude-g1");
    assert_eq!(budget["budget_consumed"], 6);
    assert_eq!(
        budget["regenerated"], 1,
        "two compliant actions earn one point"
    );
    assert_eq!(budget["budget_current"], 5);

    let credited = repository.human(&[
        "autonomy",
        "credit",
        "claude-g1",
        "--event",
        "human_review",
        "--reference",
        "PR-7",
        "--approver",
        "reviewer",
    ]);
    assert!(credited.contains("Credited 15 points"), "{credited}");
    let budget = repository.budget("claude-g1");
    assert_eq!(budget["budget_current"], 10, "never above budget_max");
    repository.spend("g1", NOTES, "three\n");
    let again = repository.human(&[
        "autonomy",
        "credit",
        "claude-g1",
        "--event",
        "human_review",
        "--reference",
        "PR-7",
        "--approver",
        "reviewer",
    ]);
    assert!(again.contains("Nothing credited"), "each event counts once");
    let wrong = repository.crane(
        &[
            "autonomy",
            "credit",
            "claude-g1",
            "--event",
            "contract_completed",
            "--reference",
            "x",
            "--approver",
            "me",
        ],
        &[],
        "",
    );
    assert!(text(&wrong.stderr).contains("cannot be credited by hand"));

    // A passing full validation is a successful contract completion
    repository.human(&["agent", "session", "verify", "claude-g1", "--level", "full"]);
    let sources = repository.budget("claude-g1")["budget_events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "regenerate")
        .map(|event| event["source"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        sources,
        ["sustained_compliance", "human_review", "contract_completed"]
    );
    assert_eq!(repository.budget("claude-g1")["budget_current"], 10);
}

/** Violation penalties: a violation costs the configured penalty; a critical one sets the budget
 * to zero, refills included
 */
#[test]
fn violation_penalties() {
    let repository = Repository::new();
    repository.start("v1", &[]);
    repository.write(SERVICE, &PAYMENT.replace("fee(amount) + amount", "amount"));
    let caught = repository.hook(
        "post-tool-use",
        "v1",
        json!({"tool_name": "Bash", "tool_input": {"command": "python rewrite.py"}}),
    );
    assert_eq!(caught.status.code(), Some(2));
    let budget = repository.budget("claude-v1");
    assert_eq!(budget["penalized"], 4);
    let penalty = budget["budget_events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "penalty")
        .unwrap()
        .clone();
    assert_eq!(penalty["violation"], "source_changed");
    repository.write(SERVICE, PAYMENT);
    repository.human(&["autonomy", "approve", "claude-v1"]);
    assert!(repository.refill("claude-v1", "3", "1h").status.success());

    let escalation = repository.hook("pre-tool-use", "v1", json!({"tool_name": "Bash", "tool_input": {"command": "crane autonomy refill claude-v1 --amount 99"}}));
    assert_eq!(escalation.status.code(), Some(2));
    let budget = repository.budget("claude-v1");
    assert_eq!(budget["budget_current"], 0);
    assert_eq!(budget["refills"], json!([]));
    assert_eq!(repository.status("claude-v1")["safety"], "quarantined");
}

/** An agent can never refill or credit its own budget: refused, journaled, and quarantined */
#[test]
fn agents_cannot_refill() {
    let repository = Repository::new();
    repository.start("a1", &[]);
    let attempt = repository.crane(
        &[
            "autonomy",
            "refill",
            "claude-a1",
            "--amount",
            "5",
            "--reason",
            "need it",
            "--approver",
            "me",
            "--expires",
            "1h",
        ],
        &[("CLAUDECODE", "1")],
        "",
    );
    assert!(!attempt.status.success());
    assert!(text(&attempt.stderr).contains("refuses to run in an agent environment"));
    assert_eq!(repository.status("claude-a1")["safety"], "quarantined");
    assert_eq!(repository.budget("claude-a1")["budget_current"], 0);
    assert_eq!(repository.budget("claude-a1")["refilled"], 0);
}

/** Session isolation: spending, refills, and penalties in one session never touch another's
 * budget, and a new session starts with its own full budget at its mode's maximum
 */
#[test]
fn session_isolation() {
    let repository = Repository::new();
    repository.start("i1", &[]);
    repository.start("i2", &["--autonomy", "autonomous"]);
    repository.spend("i1", NOTES, "only in i1\n");
    repository.spend("i1", NOTES, "still i1\n");
    assert_eq!(repository.budget("claude-i1")["budget_consumed"], 6);
    let other = repository.budget("claude-i2");
    assert_eq!(other["budget_consumed"], 0);
    assert_eq!(
        (
            other["budget_current"].as_u64(),
            other["budget_max"].as_u64()
        ),
        (Some(20), Some(20))
    );
    assert_eq!(other["budget_events"], json!([]));

    assert!(repository.refill("claude-i1", "5", "1h").status.success());
    assert_eq!(repository.budget("claude-i2")["refilled"], 0);
    repository.write(SERVICE, &PAYMENT.replace("fee(amount) + amount", "amount"));
    repository.hook(
        "post-tool-use",
        "i1",
        json!({"tool_name": "Bash", "tool_input": {"command": "python rewrite.py"}}),
    );
    assert!(
        repository.budget("claude-i1")["penalized"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(repository.budget("claude-i2")["penalized"], 0);
    assert_eq!(repository.budget("claude-i2")["budget_current"], 20);

    repository.start("i3", &[]);
    assert_eq!(repository.budget("claude-i3")["budget_current"], 10);
}

/** The model is bound at session start, and an invalid model fails closed */
#[test]
fn model_is_bound_and_validated() {
    let repository = Repository::new();
    repository.start("m1", &[]);
    repository.write(".crane/budget.json", "{\"max\": {\"delegated\": 1000}}");
    assert_eq!(repository.budget("claude-m1")["budget_max"], 10);
    repository.start("m2", &[]);
    assert_eq!(repository.budget("claude-m2")["budget_max"], 1000);
    repository.write(".crane/budget.json", "{\"tokens\": 5}");
    let start = repository.hook("session-start", "m3", json!({"source": "startup"}));
    assert!(!start.status.success());
    assert!(
        text(&start.stderr).contains("unknown setting 'tokens'"),
        "{}",
        text(&start.stderr)
    );
}
