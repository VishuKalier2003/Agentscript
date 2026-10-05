use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
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

/** The dashboard token the tests use */
const TOKEN: &str = "dashboard-test-token-0123456789";

/** Path of the payment service, in a critical zone */
const SERVICE: &str = "services/payments/src/main/java/com/acme/payments/PaymentService.java";

/** Payment service source: processing, refunds, a fee, and a state transition */
const PAYMENT: &str = "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    public int refund(int amount) {\n        return -amount;\n    }\n\n    public String markSettled(String status) {\n        return \"SETTLED\";\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n";

/** Repository files: payments with a ledger and a JUnit test, a web page with a pytest test */
const FILES: &[(&str, &str)] = &[
    ("README.md", "# Shop\n"),
    ("services/payments/pom.xml", "<project/>\n"),
    (SERVICE, PAYMENT),
    ("services/payments/src/main/java/com/acme/payments/Ledger.java", "package com.acme.payments;\n\npublic class Ledger {\n    public void postTransaction(int amount) {\n        System.out.println(amount);\n    }\n}\n"),
    ("services/payments/src/test/java/com/acme/payments/PaymentServiceTest.java", "package com.acme.payments;\n\nimport org.junit.jupiter.api.Test;\n\nclass PaymentServiceTest {\n    @Test\n    void charges() {\n        new PaymentService().charge(10);\n    }\n}\n"),
    ("web/page.py", "def render(page):\n    return page\n"),
    ("tests/test_page.py", "import pytest\nfrom web.page import render\n\n\ndef test_render():\n    assert render(1) == 1\n"),
    (".github/CODEOWNERS", "/services/payments/ @acme/payments\n"),
];

/** A temporary repository with Crane initialized, a critical payments zone, and a policy
 * preserving PaymentService.fee, removed on drop
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
            "crane-dashboard-{suffix}-{}",
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
            vec!["config", "user.name", "Crane Dashboard Test"],
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
        repository.human(&["init"]);
        repository.human(&["checkpoint", "--name", "baseline"]);
        repository.write(".crane/zones/org.zone", "zone payments {\n    criticality critical;\n    autonomy assisted;\n    select subsystem payments;\n}\n");
        repository.write(".crane/policies/core.crane", "policy core {\n    checkpoint baseline;\n    preserve --function PaymentService.fee;\n}\n");
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

    /** Run crane with agent markers removed and extra variables
     * Input
        - args: &[&str] - crane arguments
        - environment: &[(&str, &str)] - variables
        - stdin: &str - standard input
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

    /** Call the dashboard API through the CLI
     * Input
        - method: &str - GET or POST
        - path: &str - API path
        - body: Option<Value> - JSON body
     * Output
        - (bool, Value) success and the answer
    */
    fn api(&self, method: &str, path: &str, body: Option<Value>) -> (bool, Value) {
        let body = body.map(|body| body.to_string());
        let mut args = vec!["dashboard", "api", method, path];
        if let Some(body) = &body {
            args.extend(["--body", body.as_str()]);
        }
        let output = self.crane(&args, &[], "");
        (
            output.status.success(),
            serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!("{error}: {}{}", text(&output.stdout), text(&output.stderr))
            }),
        )
    }

    /** Call the dashboard API through the CLI and require success
     * Input
        - method: &str - GET or POST
        - path: &str - API path
        - body: Option<Value> - JSON body
     * Output
        - Value
    */
    fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (ok, value) = self.api(method, path, body);
        assert!(ok, "{method} {path}: {value}");
        value
    }

    /** Make one HTTP request to the dashboard server (started for that one request)
     * Input
        - method: &str - GET or POST
        - path: &str - request path
        - token: Option<&str> - X-Crane-Token
     * Output
        - (u16, String) status and body
    */
    fn http(&self, method: &str, path: &str, token: Option<&str>) -> (u16, String) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
        command
            .args(["dashboard", "--addr", "127.0.0.1:0", "--once"])
            .current_dir(&self.root)
            .env("CRANE_DASHBOARD_TOKEN", TOKEN)
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
        assert!(line.contains(&format!("?token={TOKEN}")), "{line}");
        let address = line
            .split("http://")
            .nth(1)
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .to_string();
        let mut stream = TcpStream::connect(&address).unwrap();
        let header = token.map_or(String::new(), |token| format!("X-Crane-Token: {token}\r\n"));
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{header}Content-Length: 0\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        server.wait().unwrap();
        let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        (status, body)
    }

    /** List the active policy files and their contents
     * Input
        - None
     * Output
        - Vec<(String, String)>
    */
    fn policies(&self) -> Vec<(String, String)> {
        let mut out = fs::read_dir(self.root.join(".crane/policies"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                (
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    fs::read_to_string(&path).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        out.sort();
        out
    }

    /** Run a Claude session: start it, write files through the hooks (authorized, written,
     * reported), and finalize it
     * Input
        - id: &str - provider session id
        - writes: &[(&str, &str)] - files and content
     * Output
        - None
    */
    fn session(&self, id: &str, writes: &[(&str, &str)]) {
        self.human(&[
            "agent",
            "session",
            "start",
            "--profile",
            "claude",
            "--session",
            id,
        ]);
        for (path, content) in writes {
            let payload = json!({"session_id": id, "tool_name": "Write", "tool_input": {"file_path": self.root.join(path).to_string_lossy(), "content": content}});
            self.crane(
                &[
                    "agent",
                    "hook",
                    "--event",
                    "pre-tool-use",
                    "--profile",
                    "claude",
                ],
                &[],
                &payload.to_string(),
            );
            self.write(path, content);
            self.crane(
                &[
                    "agent",
                    "hook",
                    "--event",
                    "post-tool-use",
                    "--profile",
                    "claude",
                ],
                &[],
                &payload.to_string(),
            );
        }
        self.crane(
            &["agent", "session", "finalize", &format!("claude-{id}")],
            &[],
            "",
        );
        self.write(SERVICE, PAYMENT);
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

/** The draft the policy tests create
 * Input
    - None
 * Output
    - Value
*/
fn draft() -> Value {
    json!({"name": "payments_core", "checkpoint": "baseline", "rules": [
        {"rule": "preserve", "kind": "function", "target": "PaymentService.charge", "scope": "block"},
        {"rule": "preserve", "kind": "class", "target": "Ledger", "scope": "file"},
    ]})
}

/** Repository connection happens once: screens wait for it, agents cannot make it, connecting
 * again returns the same connection, and a refresh keeps who connected and when */
#[test]
fn repository_connection_happens_once() {
    let repository = Repository::new();
    let (ok, refused) = repository.api("GET", "/api/repository", None);
    assert!(!ok);
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("connect the repository first"));
    let agent = repository.crane(&["connect"], &[("CLAUDECODE", "1")], "");
    assert!(!agent.status.success());
    assert!(text(&agent.stderr).contains("agent environment"));

    let first: Value = serde_json::from_str(&repository.human(&["connect", "--json"])).unwrap();
    assert_eq!(first["already_connected"], false);
    assert!(first["repository_id"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(first["remote"], "https://github.com/acme/shop.git");
    assert_eq!(first["default_branch"], "main");
    assert!(first["languages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|language| language["language"] == "java"));
    let again: Value = serde_json::from_str(&repository.human(&["connect", "--json"])).unwrap();
    assert_eq!(again["already_connected"], true);
    assert_eq!(again["connected_at"], first["connected_at"]);
    let refreshed = repository.ok("POST", "/api/connection", Some(json!({"refresh": true})));
    assert_eq!(refreshed["connected_at"], first["connected_at"]);
    assert!(refreshed["refreshed_at"].as_u64().is_some());
    assert_eq!(
        repository.ok("GET", "/api/connection", None)["repository_id"],
        first["repository_id"]
    );

    // Every screen now works from the connection without more configuration
    let screen = repository.ok("GET", "/api/repository", None);
    assert_eq!(
        screen["connection"]["repository_id"],
        first["repository_id"]
    );
    assert!(screen["inventory"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|target| target["qualified"] == "PaymentService.charge"));
    assert!(screen["critical_regions"]["zones"][0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|file| file == SERVICE));
}

/** Policy creation, versioning, and generated AgentScript: the visual editor's draft becomes
 * canonical AgentScript, then a pending proposal (nothing enforced); the Advanced editor records a
 * new revision; approval activates exactly the reviewed text and is recorded in the history */
#[test]
fn policy_creation_and_versioning() {
    let repository = Repository::new();
    repository.human(&["connect"]);
    let before = repository.policies();
    let preview = repository.ok("POST", "/api/contracts/preview", Some(draft()));
    assert_eq!(preview["valid"], true);
    let generated = "policy payments_core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n    preserve --class Ledger scope file;\n}\n";
    assert_eq!(preview["agentscript"], generated);
    let invalid = repository.ok(
        "POST",
        "/api/contracts/preview",
        Some(json!({"name": "x y", "rules": [{"rule": "guard", "target": "A"}]})),
    );
    assert_eq!(invalid["valid"], false);
    assert_eq!(invalid["problems"].as_array().unwrap().len(), 2);
    let parsed = repository.ok(
        "POST",
        "/api/contracts/parse",
        Some(json!({"agentscript": generated})),
    );
    assert_eq!(parsed["draft"]["rules"][1]["target"], "Ledger");

    let created = repository.ok(
        "POST",
        "/api/contracts",
        Some(json!({"draft": draft(), "by": "lead"})),
    );
    assert_eq!(created["status"], "pending");
    assert_eq!(created["activated"], false);
    assert_eq!(
        repository.policies(),
        before,
        "a new contract is only a proposal"
    );
    let (ok, duplicate) = repository.api("POST", "/api/contracts", Some(json!({"draft": draft()})));
    assert!(!ok);
    assert!(duplicate["error"]
        .as_str()
        .unwrap()
        .contains("already exists"));
    let contract = repository.ok("GET", "/api/contracts/payments_core", None);
    assert_eq!(contract["status"], "pending");
    assert_eq!(contract["agentscript"], generated);
    assert_eq!(contract["proposal"]["origin"]["editor"], "visual");

    let advanced = generated.replace("}\n", "    preserve --function PaymentService.refund;\n}\n");
    let edited = repository.ok(
        "POST",
        "/api/contracts/payments_core/edit",
        Some(json!({"agentscript": advanced, "by": "lead"})),
    );
    let versions = edited["versions"].as_array().unwrap();
    assert_eq!(
        versions
            .iter()
            .map(|version| version["action"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["generated", "edited"]
    );
    assert_ne!(versions[0]["digest"], versions[1]["digest"]);
    assert_eq!(edited["draft"]["rules"].as_array().unwrap().len(), 3);

    let digest = edited["proposal"]["digest"]
        .as_str()
        .unwrap()
        .trim_start_matches("sha256:")[..12]
        .to_string();
    let (ok, unconfirmed) = repository.api(
        "POST",
        "/api/contracts/payments_core/approve",
        Some(json!({"approver": "lead"})),
    );
    assert!(!ok, "{unconfirmed}");
    let agent = repository.crane(
        &[
            "dashboard",
            "api",
            "POST",
            "/api/contracts/payments_core/approve",
            "--body",
            &json!({"approver": "agent", "confirm": digest}).to_string(),
        ],
        &[("CLAUDECODE", "1")],
        "",
    );
    assert!(
        !agent.status.success(),
        "agents cannot approve through the dashboard API"
    );
    let approved = repository.ok(
        "POST",
        "/api/contracts/payments_core/approve",
        Some(json!({"approver": "security-lead", "confirm": digest})),
    );
    assert_eq!(approved["status"], "active");
    assert_eq!(approved["approvals"][0]["action"], "approved");
    assert_eq!(approved["approvals"][0]["by"], "security-lead");
    assert_eq!(
        fs::read_to_string(repository.root.join(".crane/policies/payments_core.crane")).unwrap(),
        advanced,
        "exactly the reviewed text is activated"
    );
    let contracts = repository.ok("GET", "/api/contracts", None);
    assert!(contracts["active"]
        .as_array()
        .unwrap()
        .iter()
        .any(|policy| policy["name"] == "payments_core"
            && policy["draft"]["rules"].as_array().unwrap().len() == 3));
}

/** The dashboard and its API are consistent: the web server, the CLI, and Crane's own commands
 * report the same data; the page names the six screens and their endpoints; the API needs the
 * token, the page does not */
#[test]
fn dashboard_and_api_are_consistent() {
    let repository = Repository::new();
    repository.human(&["connect"]);
    let (status, page) = repository.http("GET", "/", None);
    assert_eq!(status, 200);
    for (screen, endpoint) in [
        ("Repository", "/api/repository"),
        ("Zones", "/api/zones"),
        ("Contracts", "/api/contracts"),
        ("Agent Sessions", "/api/sessions"),
        ("Policy Simulator", "/api/simulate"),
        ("Attestations", "/api/attestations"),
    ] {
        assert!(page.contains(screen) && page.contains(endpoint), "{screen}");
    }
    assert_eq!(repository.http("GET", "/api/zones", None).0, 401);
    assert_eq!(
        repository
            .http("GET", "/api/zones", Some("wrong-token-0000000000000"))
            .0,
        401
    );

    let (status, body) = repository.http("GET", "/api/zones", Some(TOKEN));
    assert_eq!(status, 200);
    let served: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        served,
        repository.ok("GET", "/api/zones", None),
        "web and CLI answer alike"
    );
    let zones: Value = serde_json::from_str(&repository.human(&["zones", "--json"])).unwrap();
    assert_eq!(
        served, zones,
        "the Zones screen is exactly crane zones --json"
    );

    let discovered: Value =
        serde_json::from_str(&repository.human(&["discover", "--json"])).unwrap();
    let screen = repository.ok("GET", "/api/repository", None);
    assert_eq!(
        screen["inventory"]["repository"]["files"],
        discovered["repository"]["files"]
    );
    assert_eq!(
        screen["inventory"]["repository"]["symbols"],
        discovered["repository"]["symbols"]
    );
    assert_eq!(screen["policy_coverage"], discovered["coverage"]);

    repository.session("c1", &[("docs/notes.md", "notes\n")]);
    let sessions = repository.ok("GET", "/api/sessions", None);
    let exported: Value =
        serde_json::from_str(&repository.human(&["session", "export", "claude-c1", "--json"]))
            .unwrap();
    assert_eq!(
        sessions["sessions"][0]["final_outcome"],
        exported["attestation"]["final_decision"]
    );
    let detail = repository.ok("GET", "/api/attestations/claude-c1", None);
    assert_eq!(detail["evidence"], exported["evidence"]);
    assert_eq!(
        detail["attestation"]["attestation_digest"],
        exported["attestation"]["attestation_digest"]
    );
    assert_eq!(detail["matches_evidence"], true);
    let listed = repository.ok("GET", "/api/attestations", None);
    assert_eq!(
        listed["attestations"][0]["digest"],
        exported["attestation"]["attestation_digest"]
    );
}

/** The Payments pack recommends policies for payment processing, refunds, transaction logic, and
 * state transitions; the Testing pack finds test directories, frameworks, ownership, and the
 * contract-test configuration; neither activates anything, and only these two packs exist */
#[test]
fn policy_pack_recommendations() {
    let repository = Repository::new();
    repository.human(&["connect"]);
    let before = repository.policies();
    let packs = repository.ok("GET", "/api/packs", None);
    assert_eq!(
        packs["packs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pack| pack["pack"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["payments", "testing"]
    );

    let payments: Value =
        serde_json::from_str(&repository.human(&["packs", "show", "payments", "--json"])).unwrap();
    assert_eq!(payments, repository.ok("GET", "/api/packs/payments", None));
    let by = |category: &str| {
        payments["recommendations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["category"] == category)
            .map(|item| {
                item["entity"]
                    .as_str()
                    .unwrap()
                    .rsplit('.')
                    .take(2)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(".")
            })
            .collect::<Vec<_>>()
    };
    assert!(
        by("payment_processing").contains(&"PaymentService.charge".to_string()),
        "{payments}"
    );
    assert!(by("refunds").contains(&"PaymentService.refund".to_string()));
    assert!(by("transaction_logic").contains(&"Ledger.postTransaction".to_string()));
    assert!(by("payment_state_transitions").contains(&"PaymentService.markSettled".to_string()));
    let fee = payments["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["entity"].as_str().unwrap().ends_with("fee"))
        .unwrap();
    assert_eq!(
        fee["status"], "covered",
        "fee is already preserved by the core policy"
    );
    assert!(
        !payments.to_string().contains("PaymentServiceTest"),
        "tests are not payment code"
    );
    assert!(!payments.to_string().contains("render"));
    let policy = payments["proposed_policy"].as_str().unwrap();
    assert!(
        policy.contains("preserve --function PaymentService.charge;")
            && !policy.contains("PaymentService.fee")
    );
    assert_eq!(payments["activates_nothing"], true);

    let proposed: Value =
        serde_json::from_str(&repository.human(&["packs", "propose", "payments"])).unwrap();
    assert_eq!(proposed["status"], "pending");
    assert_eq!(
        repository.policies(),
        before,
        "a pack never activates a policy"
    );
    assert_eq!(
        repository.ok("GET", "/api/contracts/payments_pack", None)["proposal"]["origin"]["pack"],
        "payments"
    );

    let testing = repository.ok("GET", "/api/packs/testing", None);
    assert_eq!(
        testing["summary"]["frameworks"],
        json!(["junit5", "pytest"])
    );
    assert_eq!(testing["summary"]["contract_test_configuration"], "missing");
    let directories = testing["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["category"] == "test_directory")
        .map(|item| {
            (
                item["directory"].as_str().unwrap().to_string(),
                item["owners"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        directories,
        [
            (
                "services/payments/src/test".to_string(),
                json!(["@acme/payments"])
            ),
            ("tests".to_string(), json!([]))
        ]
    );
    let gaps = testing["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["category"] == "test_ownership")
        .map(|item| item["directory"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(gaps, ["tests"]);
    assert_eq!(
        testing["suggested_testing_json"]["commands"]["python"][2],
        "pytest"
    );
    assert!(
        !repository.root.join(".crane/testing.json").exists(),
        "the testing pack writes no configuration"
    );
    repository.write(
        ".crane/testing.json",
        "{\"commands\": {\"python\": [\"python\", \"-m\", \"pytest\", \"{files}\"]}}",
    );
    let configured = repository.ok("GET", "/api/packs/testing", None);
    let pytest = configured["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["framework"] == "pytest")
        .unwrap();
    assert_eq!(pytest["status"], "configured");
    let refused = repository.crane(&["packs", "propose", "testing"], &[], "");
    assert!(text(&refused.stderr).contains("only the payments pack proposes a policy"));
}

/** The simulator evaluates a draft in shadow mode against real session history: the refund rule
 * would have blocked a session that passed (a likely false positive), the charge rule one that
 * failed (a likely true positive); nothing is enforced */
#[test]
fn simulator_results() {
    let repository = Repository::new();
    repository.human(&["connect"]);
    repository.session(
        "s1",
        &[(
            SERVICE,
            &PAYMENT.replace("return -amount;", "return -Math.abs(amount);"),
        )],
    );
    // s2 changes charge through an approved write, then breaks the preserved fee with a shell command
    repository.human(&[
        "agent",
        "session",
        "start",
        "--profile",
        "claude",
        "--session",
        "s2",
    ]);
    let charged = PAYMENT.replace("fee(amount) + amount", "amount * 2");
    let payload = json!({"session_id": "s2", "tool_name": "Write", "tool_input": {"file_path": repository.root.join(SERVICE).to_string_lossy(), "content": charged}});
    repository.crane(
        &[
            "agent",
            "hook",
            "--event",
            "pre-tool-use",
            "--profile",
            "claude",
        ],
        &[],
        &payload.to_string(),
    );
    repository.write(SERVICE, &charged);
    repository.crane(
        &[
            "agent",
            "hook",
            "--event",
            "post-tool-use",
            "--profile",
            "claude",
        ],
        &[],
        &payload.to_string(),
    );
    repository.write(SERVICE, &charged.replace("amount / 10", "amount / 5"));
    let shell = json!({"session_id": "s2", "tool_name": "Bash", "tool_input": {"command": "python tweak_fee.py"}});
    repository.crane(
        &[
            "agent",
            "hook",
            "--event",
            "post-tool-use",
            "--profile",
            "claude",
        ],
        &[],
        &shell.to_string(),
    );
    repository.crane(&["agent", "session", "finalize", "claude-s2"], &[], "");
    repository.write(SERVICE, PAYMENT);
    let sessions = repository.ok("GET", "/api/sessions", None);
    let outcomes = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| {
            (
                session["session"].as_str().unwrap().to_string(),
                session["final_outcome"]["decision"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        [
            ("claude-s1".to_string(), "PASS".to_string()),
            ("claude-s2".to_string(), "FAIL".to_string())
        ]
    );
    let before = repository.policies();

    let draft = json!({"name": "shadow", "checkpoint": "baseline", "rules": [
        {"rule": "preserve", "kind": "function", "target": "PaymentService.refund"},
        {"rule": "preserve", "kind": "function", "target": "PaymentService.charge"},
    ]});
    let result = repository.ok("POST", "/api/simulate", Some(json!({"draft": draft})));
    assert_eq!(result["mode"], "shadow");
    assert_eq!(result["enforced"], false);
    assert_eq!(result["sessions_evaluated"], 2);
    assert_eq!(result["rules"][0]["resolution"], "resolved");
    assert_eq!(
        result["rules"][0]["sessions_affected"],
        json!(["claude-s1"])
    );
    assert_eq!(
        result["rules"][1]["sessions_affected"],
        json!(["claude-s2"])
    );
    let classes = result["impact"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["session"].as_str().unwrap().to_string(),
                item["classification"].as_str().unwrap().to_string(),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        classes.contains(&("claude-s1".into(), "likely_false_positive".into())),
        "{result}"
    );
    assert!(
        classes.contains(&("claude-s2".into(), "likely_true_positive".into())),
        "{result}"
    );
    assert_eq!(
        result["false_positive_analysis"]["false_positive_rate"],
        0.5
    );
    assert_eq!(
        repository.policies(),
        before,
        "a simulation enforces nothing"
    );

    let agentscript = repository.ok("POST", "/api/simulate", Some(json!({"agentscript": "policy only_refund {\n    checkpoint baseline;\n    preserve --function PaymentService.refund;\n}\n"})));
    assert_eq!(
        agentscript["false_positive_analysis"]["likely_false_positives"],
        1
    );
    assert_eq!(
        agentscript["false_positive_analysis"]["likely_true_positives"],
        0
    );
    let missing = repository.ok("POST", "/api/simulate", Some(json!({"agentscript": "policy gone {\n    checkpoint baseline;\n    preserve --function PaymentService.capture;\n}\n"})));
    assert_eq!(missing["rules"][0]["resolution"], "missing");
    assert_eq!(missing["impact"], json!([]));
}
