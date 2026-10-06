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

/** Payment code */
const PAYMENTS: &str = "class PaymentService:\n    def charge(self, amount):\n        return amount + 1\n\n    def refund(self, amount):\n        return -amount\n";

/** API code: a route handler calling payments */
const API: &str = "from payments.service import PaymentService\n\n\ndef create_order(request):\n    return PaymentService().charge(request)\n";

/** Ordinary utility code */
const UTILS: &str = "def slugify(text):\n    return text.lower()\n";

/** Authentication code */
const AUTH: &str = "def login(user, password):\n    return user\n";

/** A connected repository with payment, API, utility, and authentication code and a test,
 * removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Repository {
    root: PathBuf,
}

impl Repository {
    /** Create and connect the repository
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
            "crane-zone-review-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Self { root };
        repository.write("payments/service.py", PAYMENTS);
        repository.write("api/routes/orders.py", API);
        repository.write("utils/strings.py", UTILS);
        repository.write("auth/session.py", AUTH);
        repository.write(
            "tests/test_strings.py",
            "def test_slug():\n    assert True\n",
        );
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "lead@example.com"],
            vec!["config", "user.name", "Crane Zone Review"],
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

    /** Run git and require success
     * Input
        - args: &[&str] - arguments
     * Output
        - None
    */
    fn git(&self, args: &[&str]) {
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

    /** Run crane with --json and parse the answer
     * Input
        - args: &[&str] - arguments
     * Output
        - Value
    */
    fn json(&self, args: &[&str]) -> Value {
        let mut full = args.to_vec();
        full.push("--json");
        serde_json::from_str(&self.crane(&full)).unwrap()
    }

    /** Return the digest prefix a reviewer quotes to approve a recommendation
     * Input
        - id: &str - recommendation id
     * Output
        - String
    */
    fn confirm(&self, id: &str) -> String {
        self.json(&["zones", "review", id])["digest"]
            .as_str()
            .unwrap()
            .trim_start_matches("sha256:")[..12]
            .to_string()
    }

    /** Approve a recommendation as a human
     * Input
        - id: &str - recommendation id
     * Output
        - Value the record
    */
    fn approve(&self, id: &str) -> Value {
        let confirm = self.confirm(id);
        self.json(&[
            "zones",
            "approve",
            id,
            "--approver",
            "security-lead",
            "--confirm",
            &confirm,
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

    /** Ask the hook about an edit of a file by an autonomous agent
     * Input
        - session: &str - provider session id
        - path: &str - file
        - old: &str - text replaced
        - new: &str - replacement
     * Output
        - Output
    */
    fn edit(&self, session: &str, path: &str, old: &str, new: &str) -> Output {
        self.hook("pre-tool-use", session, json!({"tool_name": "Edit", "tool_input": {"file_path": self.root.join(path).to_string_lossy(), "old_string": old, "new_string": new}}))
    }

    /** List the active zone files
     * Input
        - None
     * Output
        - Vec<String>
    */
    fn zone_files(&self) -> Vec<String> {
        let mut names = fs::read_dir(self.root.join(".crane/zones"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
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

/** Decode process output as text
 * Input
    - bytes: &[u8] - output
 * Output
    - String
*/
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/** Acceptance: discovery over payment, API, and utility code produces candidate zones that govern
 * nothing; a human reviews and approves; the approved zones are then enforced by the runtime
 * authority engine in an agent session, while utility code stays free */
#[test]
fn approved_recommendations_govern_agent_sessions() {
    let repository = Repository::new();
    let discovery = repository.json(&["zones", "recommend"]);
    assert_eq!(
        discovery["recommendations"],
        json!(["api", "payments", "security", "tests"]),
        "{discovery}"
    );
    assert!(
        repository.zone_files().is_empty(),
        "recommendations govern nothing"
    );
    let list = repository.json(&["zones", "recommendations"]);
    let by_id = |id: &str| {
        list["recommendations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        (
            by_id("payments")["criticality"].as_str(),
            by_id("payments")["autonomy"].as_str()
        ),
        (Some("critical"), Some("assisted"))
    );
    assert_eq!(
        (
            by_id("api")["criticality"].as_str(),
            by_id("api")["autonomy"].as_str()
        ),
        (Some("sensitive"), Some("delegated"))
    );
    assert_eq!(
        (
            by_id("security")["criticality"].as_str(),
            by_id("security")["autonomy"].as_str()
        ),
        (Some("restricted"), Some("observe"))
    );
    assert!(list["recommendations"]
        .to_string()
        .contains("pack:payments@1"));

    let payments = repository.json(&["zones", "review", "payments"]);
    let recommendation = &payments["recommendation"];
    for field in [
        "selectors",
        "affected",
        "criticality",
        "autonomy",
        "safety_state",
        "rationale",
        "confidence",
        "sources",
        "signals",
    ] {
        assert!(
            !recommendation[field].is_null(),
            "{field} missing: {recommendation}"
        );
    }
    assert_eq!(
        recommendation["affected"]["files"],
        json!(["payments/service.py"])
    );
    assert_eq!(
        recommendation["selectors"],
        json!(["module python:payments.service"])
    );
    assert_eq!(recommendation["safety_state"], "active");
    assert_eq!(payments["status"], "proposed");
    let reviewing = repository.json(&[
        "zones",
        "review",
        "payments",
        "--claim",
        "--by",
        "security-lead",
    ]);
    assert_eq!(reviewing["status"], "in_review");

    // Before approval, an autonomous agent may change payment code
    repository.crane(&[
        "agent",
        "session",
        "start",
        "--profile",
        "claude",
        "--session",
        "before",
        "--autonomy",
        "autonomous",
    ]);
    let free = repository.edit(
        "before",
        "payments/service.py",
        "return -amount",
        "return -abs(amount)",
    );
    assert!(
        free.status.success() && !text(&free.stdout).contains("\"ask\""),
        "{}",
        text(&free.stdout)
    );

    let approved = repository.approve("payments");
    assert_eq!(approved["status"], "approved");
    assert_eq!(approved["active"], true);
    assert_eq!(
        fs::read_to_string(repository.root.join(".crane/zones/payments.zone")).unwrap(),
        approved["zone_text"].as_str().unwrap()
    );
    repository.approve("security");
    let active = repository.json(&["zones"]);
    let zones = active["zones"]
        .as_array()
        .unwrap()
        .iter()
        .map(|zone| zone["zone_id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        zones,
        ["payments", "security"],
        "the approved recommendations are ordinary active zones"
    );

    // A session started now is governed by them
    repository.crane(&[
        "agent",
        "session",
        "start",
        "--profile",
        "claude",
        "--session",
        "after",
        "--autonomy",
        "autonomous",
    ]);
    let critical = repository.edit(
        "after",
        "payments/service.py",
        "return -amount",
        "return -abs(amount)",
    );
    assert!(
        text(&critical.stdout).contains("\"permissionDecision\":\"ask\""),
        "{}",
        text(&critical.stdout)
    );
    assert!(text(&critical.stdout).contains("zones payments (critical"));
    let restricted = repository.edit("after", "auth/session.py", "return user", "return None");
    assert_eq!(
        restricted.status.code(),
        Some(2),
        "restricted code is closed to an autonomous agent"
    );
    assert!(text(&restricted.stderr).contains("zones security (restricted"));
    let utility = repository.edit(
        "after",
        "utils/strings.py",
        "return text.lower()",
        "return text.casefold()",
    );
    assert!(
        utility.status.success() && !text(&utility.stdout).contains("\"ask\""),
        "utility code stays free"
    );
    // The session that started before keeps its bound zones
    assert!(!text(
        &repository
            .edit(
                "before",
                "payments/service.py",
                "return amount + 1",
                "return amount + 2"
            )
            .stdout
    )
    .contains("\"ask\""));
}

/** Approval is idempotent and audited; rejection is final for that revision and idempotent; the
 * audit log is hash-chained */
#[test]
fn approval_and_rejection_are_idempotent_and_audited() {
    let repository = Repository::new();
    repository.crane(&["zones", "recommend"]);
    let wrong = repository.run(
        &[
            "zones",
            "approve",
            "payments",
            "--approver",
            "lead",
            "--confirm",
            "000000000000",
        ],
        &[],
        "",
    );
    assert!(
        text(&wrong.stderr).contains("--confirm with at least the first 12 characters"),
        "{}",
        text(&wrong.stderr)
    );
    let missing = repository.run(
        &[
            "zones",
            "approve",
            "payments",
            "--confirm",
            &repository.confirm("payments"),
        ],
        &[],
        "",
    );
    assert!(text(&missing.stderr).contains("requires --approver"));

    let first = repository.approve("payments");
    assert_eq!(first["already_approved"], false);
    let second = repository.approve("payments");
    assert_eq!(second["already_approved"], true);
    assert_eq!(
        second["history"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["action"] == "approved")
            .count(),
        1
    );

    let rejected = repository.json(&[
        "zones",
        "reject",
        "api",
        "--approver",
        "lead",
        "--reason",
        "too broad",
    ]);
    assert_eq!(rejected["status"], "rejected");
    assert_eq!(rejected["rejection"]["reason"], "too broad");
    assert_eq!(
        repository.json(&["zones", "reject", "api", "--approver", "lead"])["already_rejected"],
        true
    );
    let late = repository.run(
        &[
            "zones",
            "approve",
            "api",
            "--approver",
            "lead",
            "--confirm",
            &repository.confirm("api"),
        ],
        &[],
        "",
    );
    assert!(text(&late.stderr).contains("only proposed recommendations can be approved"));
    assert!(!repository.root.join(".crane/zones/api.zone").exists());
    let reject_active = repository.run(
        &["zones", "reject", "payments", "--approver", "lead"],
        &[],
        "",
    );
    assert!(text(&reject_active.stderr).contains("only proposed recommendations can be rejected"));

    // Rediscovery leaves decisions alone when nothing changed
    let again = repository.json(&["zones", "recommend"]);
    let change = |id: &str| {
        again["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["id"] == id)
            .unwrap()["change"]
            .clone()
    };
    assert_eq!(change("payments"), "unchanged");
    assert_eq!(change("api"), "unchanged");
    assert_eq!(
        repository.json(&["zones", "review", "api"])["status"],
        "rejected"
    );

    let audit = repository.json(&["zones", "audit"]);
    assert_eq!(audit["chain"]["status"], "verified");
    let events = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            format!(
                "{}:{}",
                event["event"].as_str().unwrap(),
                event["recommendation"].as_str().unwrap()
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        events,
        [
            "zone_recommendation_approved:payments",
            "zone_recommendation_rejected:api"
        ]
    );
    assert_eq!(audit["events"][0]["by"], "security-lead");
    assert!(audit["events"][0]["zone_set_version_after"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
}

/** Versioning: changed evidence makes a new revision; an approved zone keeps governing until the
 * new revision is approved; evidence that disappears withdraws a pending recommendation; a zone
 * written by hand is never overwritten */
#[test]
fn recommendations_are_versioned() {
    let repository = Repository::new();
    repository.crane(&["zones", "recommend"]);
    let approved = repository.approve("payments");
    let first_text = approved["zone_text"].as_str().unwrap().to_string();

    repository.write(
        "billing/ledger.py",
        "def settle_transaction(amount):\n    return amount\n",
    );
    repository.git(&["rm", "-q", "auth/session.py"]);
    repository.git(&["add", "."]);
    repository.git(&["commit", "-qm", "billing, no auth"]);
    let discovery = repository.json(&["zones", "recommend"]);
    let change = |id: &str| {
        discovery["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["id"] == id)
            .map(|change| change["change"].clone())
    };
    assert_eq!(change("payments"), Some(json!("revision_proposed")));
    assert_eq!(change("security"), Some(json!("withdrawn")));
    let payments = repository.json(&["zones", "review", "payments"]);
    assert_eq!(payments["revision"], 2);
    assert_eq!(payments["status"], "proposed");
    assert!(
        payments["zone_text"]
            .as_str()
            .unwrap()
            .contains("select module python:billing.ledger;"),
        "{}",
        payments["zone_text"]
    );
    assert_eq!(
        fs::read_to_string(repository.root.join(".crane/zones/payments.zone")).unwrap(),
        first_text,
        "the active zone is unchanged until approval"
    );
    let updated = repository.approve("payments");
    assert_eq!(updated["activation"]["revision"], 2);
    assert!(
        fs::read_to_string(repository.root.join(".crane/zones/payments.zone"))
            .unwrap()
            .contains("billing.ledger")
    );
    assert_eq!(
        repository.json(&["zones", "review", "security"])["status"],
        "withdrawn"
    );

    // A hand-written zone is never replaced
    repository.write(".crane/zones/api.zone", "zone api {\n    criticality routine;\n    autonomy autonomous;\n    select folder docs;\n}\n");
    let conflict = repository.run(
        &[
            "zones",
            "approve",
            "api",
            "--approver",
            "lead",
            "--confirm",
            &repository.confirm("api"),
        ],
        &[],
        "",
    );
    assert!(
        text(&conflict.stderr).contains("was not written by a zone review"),
        "{}",
        text(&conflict.stderr)
    );
    let renamed = repository.json(&["zones", "recommend"]);
    assert!(renamed["changes"].to_string().contains("\"api\""));
    assert_eq!(
        repository.json(&["zones", "review", "api"])["zone_id"],
        "api_2",
        "the next revision takes a free zone name"
    );

    let runs: Value = serde_json::from_str(
        &fs::read_to_string(repository.root.join(".crane/zone-proposals/discovery.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        runs["runs"].as_array().unwrap().len(),
        3,
        "every discovery run is recorded"
    );
}

/** No agent may decide zone authority: the CLI refuses in an agent environment, an agent's shell
 * running it is denied and quarantines the session, and an agent cannot write a zone file */
#[test]
fn agents_cannot_change_zone_authority() {
    let repository = Repository::new();
    repository.crane(&["zones", "recommend"]);
    let confirm = repository.confirm("payments");
    for args in [
        vec![
            "zones",
            "approve",
            "payments",
            "--approver",
            "agent",
            "--confirm",
            confirm.as_str(),
        ],
        vec!["zones", "reject", "payments", "--approver", "agent"],
        vec!["zones", "review", "payments", "--claim"],
    ] {
        let refused = repository.run(&args, &[("CLAUDECODE", "1")], "");
        assert!(!refused.status.success(), "{args:?}");
        assert!(
            text(&refused.stderr).contains("refuses to run in an agent environment"),
            "{args:?}: {}",
            text(&refused.stderr)
        );
    }
    assert_eq!(
        repository.json(&["zones", "review", "payments"])["status"],
        "proposed"
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
    let shell = repository.hook("pre-tool-use", "a1", json!({"tool_name": "Bash", "tool_input": {"command": format!("crane zones approve payments --approver me --confirm {confirm}")}}));
    assert_eq!(shell.status.code(), Some(2));
    let status = repository.json(&["autonomy", "status", "claude-a1"]);
    assert_eq!(
        status["safety"], "quarantined",
        "trying to approve zones is self-escalation"
    );
    let write = repository.hook("pre-tool-use", "a1", json!({"tool_name": "Write", "tool_input": {"file_path": repository.root.join(".crane/zones/mine.zone").to_string_lossy(), "content": "zone mine {\n    criticality routine;\n    autonomy autonomous;\n    select tests;\n}\n"}}));
    assert_eq!(
        write.status.code(),
        Some(2),
        "agents cannot write zone files"
    );
    assert!(repository.zone_files().is_empty());
    // An agent may run discovery: recommendations are only proposals, and are labelled
    let discovered = repository.run(
        &["zones", "recommend", "--json"],
        &[("CLAUDECODE", "1")],
        "",
    );
    assert!(discovered.status.success());
    let by: Value = serde_json::from_slice(&discovered.stdout).unwrap();
    assert!(by["by"].as_str().unwrap().contains("agent environment"));
}

/** The dashboard review API reports the same state and drives the same decisions */
#[test]
fn dashboard_review_api() {
    let repository = Repository::new();
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
    let (ok, discovery) = api(
        "POST",
        "/api/zones/recommendations",
        Some(json!({"by": "lead"})),
    );
    assert!(ok, "{discovery}");
    let (_, listed) = api("GET", "/api/zones/recommendations", None);
    assert_eq!(
        listed,
        repository.json(&["zones", "recommendations"]),
        "the dashboard and the CLI agree"
    );
    assert_eq!(listed["discovery"]["run"], discovery["run"]);
    let (_, record) = api("GET", "/api/zones/recommendations/payments", None);
    let confirm = record["digest"].as_str().unwrap()[7..19].to_string();
    let (ok, reviewing) = api(
        "POST",
        "/api/zones/recommendations/payments/review",
        Some(json!({"by": "lead"})),
    );
    assert!(ok);
    assert_eq!(reviewing["status"], "in_review");
    let (ok, approved) = api(
        "POST",
        "/api/zones/recommendations/payments/approve",
        Some(json!({"approver": "lead", "confirm": confirm})),
    );
    assert!(ok, "{approved}");
    assert_eq!(approved["status"], "approved");
    let (ok, rejected) = api(
        "POST",
        "/api/zones/recommendations/api/reject",
        Some(json!({"approver": "lead", "reason": "later"})),
    );
    assert!(ok);
    assert_eq!(rejected["status"], "rejected");
    let (_, zones) = api("GET", "/api/zones", None);
    assert_eq!(zones["zones"][0]["zone_id"], "payments");
    let (_, audit) = api("GET", "/api/zones/audit", None);
    assert_eq!(audit["events"].as_array().unwrap().len(), 2);
    let page = include_str!("../src/dashboard/app.html");
    for needle in [
        "/api/zones/recommendations",
        "Recommended zones",
        "Approve and activate",
        "suggested autonomy",
        "suggested safety state",
    ] {
        assert!(page.contains(needle), "{needle}");
    }
}

/** Task history feeds recommendations: sessions that changed and broke code show in the rationale */
#[test]
fn task_history_is_a_recommendation_source() {
    let repository = Repository::new();
    repository.crane(&[
        "protect",
        "--function",
        "PaymentService.charge",
        "--policy",
        "core",
    ]);
    repository.crane(&[
        "agent",
        "session",
        "start",
        "--profile",
        "claude",
        "--session",
        "h1",
    ]);
    repository.write(
        "payments/service.py",
        &PAYMENTS.replace("return amount + 1", "return amount + 2"),
    );
    repository.hook(
        "post-tool-use",
        "h1",
        json!({"tool_name": "Bash", "tool_input": {"command": "python rewrite.py"}}),
    );
    repository.json(&["zones", "recommend"]);
    let payments = repository.json(&["zones", "review", "payments"]);
    assert!(
        payments["recommendation"]["sources"]
            .as_array()
            .unwrap()
            .contains(&json!("task_history")),
        "{payments}"
    );
    assert!(
        payments["recommendation"]["rationale"]
            .to_string()
            .contains("with violations 1 time(s)"),
        "{}",
        payments["recommendation"]["rationale"]
    );
}
