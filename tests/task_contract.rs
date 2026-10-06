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

/** Repository files: a payments service, a billing service, authentication, and analytics */
const FILES: &[(&str, &str)] = &[
    ("README.md", "# Shop\n"),
    ("services/payments/pom.xml", "<project/>\n"),
    (
        "services/payments/src/main/java/com/acme/payments/PaymentService.java",
        "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    public int refund(int amount) {\n        return charge(-amount);\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n",
    ),
    (
        "services/payments/src/test/java/com/acme/payments/PaymentServiceTest.java",
        "package com.acme.payments;\n\nclass PaymentServiceTest {\n    @Test\n    void charges() {\n        new PaymentService().charge(5);\n    }\n}\n",
    ),
    ("services/billing/pom.xml", "<project/>\n"),
    (
        "services/billing/src/main/java/com/acme/billing/InvoiceService.java",
        "package com.acme.billing;\n\npublic class InvoiceService {\n    public double total(double amount) {\n        return amount * 1.0;\n    }\n}\n",
    ),
    ("auth/login.py", "def authenticate(user):\n    return user\n"),
    ("analytics/report.py", "def rows(data):\n    return len(data)\n"),
];

/** Organization zones: Payments is Critical */
const ZONES: &str = "zone payments {\n    criticality critical;\n    autonomy assisted;\n    select subsystem payments;\n}\n";

/** A permanent organization policy: charge never changes */
const POLICY: &str = "policy payments_core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n";

/** The Jira task of the acceptance criterion */
const DUPLICATE: &str = "Fix duplicate payment processing in PaymentService";

/** A temporary repository named shop with Crane initialized, a baseline checkpoint, a payments
 * zone, an organization policy, and an organization file, removed on drop
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
            "crane-task-contract-{suffix}-{}",
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
            vec!["config", "user.name", "Crane Task Contract"],
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
        repository.crane(&["init"]);
        repository.crane(&["checkpoint", "--name", "baseline"]);
        repository.write(".crane/zones/org.zone", ZONES);
        repository.write(".crane/policies/payments_core.crane", POLICY);
        repository.write(
            ".crane/organization.json",
            "{\"organization\": \"acme\", \"team\": \"payments\", \"policies\": [\"payments_core\"]}\n",
        );
        repository
    }

    /** Write a file relative to the root, creating parent folders
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
        - String stdout
    */
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
        text(&output.stdout)
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

    /** Run crane with --json as a human and parse the answer, whatever the exit status
     * Input
        - args: &[&str] - arguments
     * Output
        - (bool, Value) success and answer
    */
    fn json(&self, args: &[&str]) -> (bool, Value) {
        let mut full = args.to_vec();
        full.push("--json");
        let output = self.run(&full, &[], "");
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "crane {args:?} JSON expected: {}{}",
                text(&output.stdout),
                text(&output.stderr)
            )
        });
        (output.status.success(), value)
    }

    /** Store a task file in .crane/tasks
     * Input
        - id: &str - task id
        - title: &str - title, also used as description when none is given
        - description: Option<&str> - description
     * Output
        - None
    */
    fn task(&self, id: &str, title: &str, description: Option<&str>) {
        self.write(
            &format!(".crane/tasks/{id}.json"),
            &json!({
                "task_id": id,
                "title": title,
                "description": description.unwrap_or(title),
                "repositories": ["acme/shop"],
                "requester": "pm@example.com",
                "team": "payments",
            })
            .to_string(),
        );
    }

    /** Compile a task's contract as a human
     * Input
        - id: &str - task id
     * Output
        - (bool, Value) success and contract
    */
    fn compile(&self, id: &str) -> (bool, Value) {
        self.json(&["task", "contract", "compile", id])
    }

    /** Show a task's current contract
     * Input
        - id: &str - task id
     * Output
        - Value
    */
    fn show(&self, id: &str) -> Value {
        self.json(&["task", "contract", "show", id]).1
    }

    /** Approve a task's current contract as a human, quoting its digest
     * Input
        - id: &str - task id
     * Output
        - (bool, Value) success and answer
    */
    fn approve(&self, id: &str) -> Output {
        let confirm = confirm(&self.show(id));
        self.run(
            &[
                "task",
                "contract",
                "approve",
                id,
                "--approver",
                "payments-lead",
                "--confirm",
                &confirm,
            ],
            &[],
            "",
        )
    }

    /** Send a Claude hook event
     * Input
        - session: &str - provider session id
        - payload: Value - fields
     * Output
        - Output
    */
    fn hook(&self, session: &str, mut payload: Value) -> Output {
        payload["session_id"] = json!(session);
        self.run(
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
        )
    }

    /** Ask the hook about an edit by an agent
     * Input
        - session: &str - provider session id
        - path: &str - file
        - old: &str - text replaced
        - new: &str - replacement
     * Output
        - Output
    */
    fn edit(&self, session: &str, path: &str, old: &str, new: &str) -> Output {
        self.hook(session, json!({"tool_name": "Edit", "tool_input": {"file_path": self.root.join(path).to_string_lossy(), "old_string": old, "new_string": new}}))
    }

    /** Start an autonomous agent session bound to a task
     * Input
        - session: &str - provider session id
        - task: &str - task id
     * Output
        - None
    */
    fn session(&self, session: &str, task: &str) {
        self.crane(&[
            "agent",
            "session",
            "start",
            "--profile",
            "claude",
            "--session",
            session,
            "--autonomy",
            "autonomous",
            "--task",
            task,
        ]);
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

/** Return the digest prefix an approver quotes
 * Input
    - contract: &Value - contract
 * Output
    - String
*/
fn confirm(contract: &Value) -> String {
    contract["digest"].as_str().unwrap()[7..19].to_string()
}

/** List the ids of one contract section
 * Input
    - contract: &Value - contract
    - section: &str - MUST_CHANGE or MUST_NOT_CHANGE
 * Output
    - Vec<String>
*/
fn ids(contract: &Value, section: &str) -> Vec<String> {
    contract["contract"][section]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_string())
        .collect()
}

/** Billing code outside the payments scope */
const BILLING: &str = "services/billing/src/main/java/com/acme/billing/InvoiceService.java";

/** The payment service */
const PAYMENT: &str = "services/payments/src/main/java/com/acme/payments/PaymentService.java";

/** Acceptance: "Fix duplicate payment processing in PaymentService" compiles to a bounded contract
 * over the payment resources, bound to the repository, task, checkpoint, policies, zones, autonomy,
 * budget, and organization, instead of repository-wide write authority; it grants nothing until a
 * human approves it, and then only inside its scope */
#[test]
fn payment_task_compiles_to_a_bounded_contract() {
    let repository = Repository::new();
    repository.task("PAY-1900", DUPLICATE, None);
    let (success, contract) = repository.compile("PAY-1900");
    assert!(success, "{contract}");
    assert_eq!(contract["status"], "proposed");
    assert_eq!(contract["contract_id"], "PAY-1900@v1");
    assert_eq!(contract["authority"]["granted"], false);
    for section in [
        "MUST_CHANGE",
        "MUST_NOT_CHANGE",
        "MAY_CHANGE",
        "REQUIRES_APPROVAL",
        "TASK_SCOPE",
        "EXPECTED_TESTS",
    ] {
        assert!(
            !contract["contract"][section].is_null(),
            "{section} missing"
        );
    }
    assert_eq!(
        ids(&contract, "MUST_CHANGE"),
        ["symbol:java:com.acme.payments.PaymentService"]
    );
    assert_eq!(
        ids(&contract, "MUST_NOT_CHANGE"),
        ["symbol:java:com.acme.payments.PaymentService.charge"],
        "the permanent policy still holds inside the task"
    );
    let scope = &contract["contract"]["TASK_SCOPE"];
    assert_eq!(scope["modules"], json!(["module:java:com.acme.payments"]));
    assert_eq!(scope["services"], json!(["service:services/payments"]));
    assert_eq!(scope["files"], json!([PAYMENT]));
    for item in contract["contract"]["MAY_CHANGE"].as_array().unwrap() {
        assert!(
            item.as_str()
                .unwrap()
                .starts_with("symbol:java:com.acme.payments."),
            "{item}"
        );
    }
    let sections = contract["contract"].to_string();
    for outside in ["billing", "auth", "analytics"] {
        assert!(
            !sections.contains(outside),
            "{outside} is outside the task: {sections}"
        );
    }
    assert!(contract["contract"]["REQUIRES_APPROVAL"]
        .to_string()
        .contains("zones payments (critical) require human approval"));
    let tests = contract["contract"]["EXPECTED_TESTS"].to_string();
    assert!(tests.contains("PaymentServiceTest.java"), "{tests}");
    assert!(tests.contains("add_regression_test"), "{tests}");
    assert_eq!(
        contract["clarifications"][0]["blocking"], false,
        "missing acceptance criteria are advisory"
    );
    assert_eq!(
        contract["agentscript"],
        "policy task_pay_1900_v1 {\n    checkpoint baseline;\n    target --class PaymentService;\n}\n"
    );

    // Bindings
    let bindings = &contract["bindings"];
    assert_eq!(bindings["repository"]["name"], "shop");
    assert!(bindings["repository"]["identity"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(bindings["task"]["task_id"], "PAY-1900");
    assert!(bindings["task"]["task_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(bindings["checkpoint"]["name"], "baseline");
    assert_eq!(
        bindings["checkpoint"]["sha"],
        repository.git(&["rev-parse", "HEAD"]).trim()
    );
    let (_, activation) = repository.json(&["policy", "status"]);
    assert_eq!(bindings["policy_version"], activation["policy_version"]);
    assert_eq!(
        bindings["zone_set_version"],
        repository.json(&["zones"]).1["zone_set_version"]
    );
    assert_eq!(bindings["autonomy"]["max_autonomy"], "autonomous");
    assert!(bindings["budget"]["model_version"].is_string());
    assert!(bindings["budget"]["max"].is_u64());
    assert_eq!(bindings["organization"]["organization"], "acme");
    assert_eq!(
        bindings["organization"]["policies"],
        json!(["payments_core"])
    );
    assert!(contract["digest"].as_str().unwrap().starts_with("sha256:"));

    // Compiling again changes nothing
    let (_, again) = repository.compile("PAY-1900");
    assert_eq!(again["unchanged"], true);
    assert_eq!(again["digest"], contract["digest"]);
    assert_eq!(again["version"], 1);

    // Unapproved, the contract grants no autonomous authority
    repository.session("before", "PAY-1900");
    let held = repository.edit("before", BILLING, "amount * 1.0", "amount * 2.0");
    assert!(
        text(&held.stdout).contains("autonomy mode is assisted"),
        "{}",
        text(&held.stdout)
    );

    let approved = repository.approve("PAY-1900");
    assert!(approved.status.success(), "{}", text(&approved.stderr));
    assert!(repository
        .root
        .join(".crane/policies/task_pay_1900_v1.crane")
        .is_file());
    repository.session("after", "PAY-1900");
    let outside = repository.edit("after", BILLING, "amount * 1.0", "amount * 2.0");
    assert!(
        text(&outside.stdout).contains("outside the task scope (module:java:com.acme.payments)"),
        "no repository-wide authority, even in autonomous mode: {}",
        text(&outside.stdout)
    );
    let refund = repository.edit(
        "after",
        PAYMENT,
        "return charge(-amount);",
        "return charge(-Math.abs(amount));",
    );
    assert!(
        text(&refund.stdout).contains("zones payments (critical"),
        "{}",
        text(&refund.stdout)
    );
    let charge = repository.edit(
        "after",
        PAYMENT,
        "return fee(amount) + amount;",
        "return amount;",
    );
    assert_eq!(charge.status.code(), Some(2), "MUST_NOT_CHANGE is enforced");
    let session: Value =
        serde_json::from_str(&repository.crane(&["agent", "session", "show", "claude-after"]))
            .unwrap();
    assert_eq!(
        session["governance"]["task_contract"]["contract_id"], "PAY-1900@v1",
        "{session}"
    );
    assert_eq!(session["governance"]["task_contract"]["status"], "approved");
}

/** A vague task compiles to clarification_required: an error state naming what is missing, with
 * no scope, no proposal, no approval possible, and no autonomous authority for its sessions; a
 * scope covering most of the repository is vague too */
#[test]
fn vague_tasks_require_clarification() {
    let repository = Repository::new();
    repository.task(
        "PAY-1901",
        "Fix the payment bug",
        Some("Payments are broken, fix them."),
    );
    let (success, contract) = repository.compile("PAY-1901");
    assert!(!success);
    assert_eq!(contract["status"], "clarification_required");
    assert_eq!(contract["authority"]["granted"], false);
    assert!(contract["clarifications"]
        .to_string()
        .contains("no function or class that must change could be identified"));
    assert_eq!(contract["contract"]["MUST_CHANGE"], json!([]));
    assert_eq!(contract["contract"]["MAY_CHANGE"], json!([]));
    assert_eq!(contract["contract"]["TASK_SCOPE"]["files"], json!([]));
    assert_eq!(contract["proposal"], Value::Null);
    assert_eq!(contract["agentscript"], Value::Null);
    let output = repository.run(&["task", "contract", "compile", "PAY-1901"], &[], "");
    assert!(text(&output.stdout).contains("CLARIFICATION REQUIRED"));
    assert!(
        text(&output.stderr).contains("contract is clarification_required; it grants no authority")
    );
    let refused = repository.approve("PAY-1901");
    assert!(!refused.status.success());
    assert!(
        text(&refused.stderr).contains("the task is too vague to bound"),
        "{}",
        text(&refused.stderr)
    );
    repository.session("vague", "PAY-1901");
    let held = repository.edit("vague", "auth/login.py", "return user", "return None");
    assert!(
        text(&held.stdout).contains("autonomy mode is assisted"),
        "{}",
        text(&held.stdout)
    );

    // Asking for changes everywhere is vague, whatever else the task names
    repository.task(
        "PAY-1902",
        "Fix PaymentService.refund everywhere",
        Some("Fix `PaymentService.refund` everywhere as needed."),
    );
    let (_, broad) = repository.compile("PAY-1902");
    assert_eq!(broad["status"], "clarification_required");
    assert!(broad["clarifications"].to_string().contains("'everywhere'"));

    // A scope covering most of the repository is repository-wide authority
    for index in 0..10 {
        repository.write(
            &format!("services/payments/src/main/java/com/acme/payments/Ledger{index}.java"),
            &format!("package com.acme.payments;\n\npublic class Ledger{index} {{\n    public int post(int amount) {{\n        return amount;\n    }}\n}}\n"),
        );
    }
    repository.git(&["add", "."]);
    repository.git(&["commit", "-qm", "ledgers"]);
    repository.task("PAY-1903", "Fix rounding in `Ledger3.post`", None);
    let (_, wide) = repository.compile("PAY-1903");
    assert_eq!(wide["status"], "clarification_required", "{wide}");
    assert!(wide["clarifications"]
        .to_string()
        .contains("which is repository-wide authority"));
}

/** Approval needs a human, a named approver, and the digest; it activates the contract's
 * AgentScript as a task-layer policy (the persistent policy version does not move), is idempotent,
 * and is audited in a hash-chained log; rejection is final and idempotent too */
#[test]
fn approval_is_confirmed_idempotent_and_audited() {
    let repository = Repository::new();
    repository.task("PAY-1900", DUPLICATE, None);
    let (_, contract) = repository.compile("PAY-1900");
    let (_, before) = repository.json(&["policy", "status"]);
    let anonymous = repository.run(
        &[
            "task",
            "contract",
            "approve",
            "PAY-1900",
            "--confirm",
            &confirm(&contract),
        ],
        &[],
        "",
    );
    assert!(text(&anonymous.stderr).contains("requires --approver NAME"));
    let wrong = repository.run(
        &[
            "task",
            "contract",
            "approve",
            "PAY-1900",
            "--approver",
            "lead",
            "--confirm",
            "000000000000",
        ],
        &[],
        "",
    );
    assert!(text(&wrong.stderr).contains("first 12 characters of the contract digest"));
    assert_eq!(repository.show("PAY-1900")["status"], "proposed");

    assert!(repository.approve("PAY-1900").status.success());
    let approved = repository.show("PAY-1900");
    assert_eq!(approved["status"], "approved");
    assert_eq!(approved["approval"]["approver"], "payments-lead");
    assert_eq!(approved["authority"]["granted"], true);
    let again = repository.approve("PAY-1900");
    assert!(text(&again.stdout).contains("Already approved; nothing changed."));

    let (_, after) = repository.json(&["policy", "status"]);
    assert_eq!(
        after["policy_version"], before["policy_version"],
        "task contracts are a layer of their own"
    );
    assert_eq!(
        after["layers"]["task"]["policies"],
        json!(["task_pay_1900_v1"])
    );
    assert_eq!(
        after["layers"]["organization"]["policies"],
        json!(["payments_core"])
    );
    assert_eq!(
        repository.show("PAY-1900")["status"],
        "approved",
        "approving a contract never invalidates it"
    );
    let late = repository.run(
        &[
            "task",
            "contract",
            "reject",
            "PAY-1900",
            "--approver",
            "lead",
        ],
        &[],
        "",
    );
    assert!(text(&late.stderr).contains("only a contract awaiting a decision can be rejected"));

    repository.task(
        "PAY-1904",
        "Make `PaymentService.refund` reject negative amounts",
        None,
    );
    repository.compile("PAY-1904");
    let (ok, rejected) = repository.json(&[
        "task",
        "contract",
        "reject",
        "PAY-1904",
        "--approver",
        "lead",
        "--reason",
        "too risky",
    ]);
    assert!(ok);
    assert_eq!(rejected["status"], "rejected");
    let (_, twice) = repository.json(&[
        "task",
        "contract",
        "reject",
        "PAY-1904",
        "--approver",
        "lead",
    ]);
    assert_eq!(twice["already_rejected"], true);
    let (_, proposals) = repository.json(&["policy", "proposals"]);
    assert!(
        proposals.to_string().contains(
            "\"proposal_id\":\"task_pay_1904_v1\",\"revision\":1,\"status\":\"rejected\""
        ),
        "{proposals}"
    );

    let (_, history) = repository.json(&["task", "contract", "history", "PAY-1900"]);
    assert_eq!(history["chain"]["status"], "verified");
    let events = history["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["event"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(events, ["task_contract_compiled", "task_contract_approved"]);
    let (_, listed) = repository.json(&["task", "contract", "list"]);
    assert_eq!(listed["contracts"].as_array().unwrap().len(), 2);
}

/** A contract is invalidated when what it was bound to changes (zones, persistent policies,
 * checkpoint, autonomy configuration): its policy stops being enforced, it can no longer be
 * approved, sessions of the task lose their authority, and compiling again makes a new version */
#[test]
fn contracts_are_invalidated_when_bindings_change() {
    let repository = Repository::new();
    repository.task("PAY-1900", DUPLICATE, None);
    repository.compile("PAY-1900");
    assert!(repository.approve("PAY-1900").status.success());

    // Zones
    repository.write(
        ".crane/zones/billing.zone",
        "zone billing {\n    criticality sensitive;\n    autonomy delegated;\n    select subsystem billing;\n}\n",
    );
    let invalidated = repository.show("PAY-1900");
    assert_eq!(invalidated["status"], "invalidated");
    assert_eq!(
        invalidated["invalidation"]["changed"][0]["binding"],
        "zones"
    );
    assert_eq!(invalidated["authority"]["granted"], false);
    assert!(
        !repository
            .root
            .join(".crane/policies/task_pay_1900_v1.crane")
            .exists()
            && repository
                .root
                .join(".crane/retired/task_pay_1900_v1.crane")
                .exists(),
        "an invalidated contract is no longer enforced"
    );
    let refused = repository.approve("PAY-1900");
    assert!(text(&refused.stderr).contains("was invalidated: zones changed"));
    repository.session("stale", "PAY-1900");
    assert!(text(
        &repository
            .edit(
                "stale",
                PAYMENT,
                "return amount / 10;",
                "return amount / 5;"
            )
            .stdout
    )
    .contains("autonomy mode is assisted"));
    let (_, second) = repository.compile("PAY-1900");
    assert_eq!(second["contract_id"], "PAY-1900@v2");
    assert_eq!(second["status"], "proposed");
    assert_ne!(
        second["bindings"]["zone_set_version"],
        invalidated["bindings"]["zone_set_version"]
    );
    assert!(repository.approve("PAY-1900").status.success());

    // Persistent policies
    repository.write(
        ".crane/policies/refund_guard.crane",
        "policy refund_guard {\n    checkpoint baseline;\n    preserve --function PaymentService.fee;\n}\n",
    );
    let changed = repository.show("PAY-1900");
    assert_eq!(changed["status"], "invalidated");
    assert_eq!(changed["invalidation"]["changed"][0]["binding"], "policies");
    let (_, third) = repository.compile("PAY-1900");
    assert!(ids(&third, "MUST_NOT_CHANGE")
        .contains(&"symbol:java:com.acme.payments.PaymentService.fee".to_string()));
    assert!(repository.approve("PAY-1900").status.success());

    // Checkpoint
    repository.write("README.md", "# Shop v2\n");
    repository.git(&["commit", "-qam", "docs"]);
    repository.crane(&["checkpoint", "--name", "baseline"]);
    let moved = repository.show("PAY-1900");
    assert_eq!(moved["status"], "invalidated", "{moved}");
    assert_eq!(moved["invalidation"]["changed"][0]["binding"], "checkpoint");
    repository.compile("PAY-1900");
    assert!(repository.approve("PAY-1900").status.success());

    // Autonomy configuration
    repository.write(
        ".crane/autonomy.json",
        "{\"max_autonomy\": \"delegated\"}\n",
    );
    let capped = repository.show("PAY-1900");
    assert_eq!(capped["status"], "invalidated");
    assert_eq!(capped["invalidation"]["changed"][0]["binding"], "autonomy");

    let (_, history) = repository.json(&["task", "contract", "history", "PAY-1900"]);
    let statuses = history["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|version| version["status"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        ["invalidated", "invalidated", "invalidated", "invalidated"]
    );
    assert_eq!(history["chain"]["status"], "verified");
}

/** A changed task is a new contract version: the proposed contract of the old text cannot be
 * approved, compiling again supersedes it, and an approved version stays in force until the next
 * one is approved */
#[test]
fn task_changes_make_new_versions() {
    let repository = Repository::new();
    repository.task("PAY-1900", DUPLICATE, None);
    repository.compile("PAY-1900");
    repository.task(
        "PAY-1900",
        DUPLICATE,
        Some("Fix duplicate payment processing in `PaymentService.refund`."),
    );
    let stale = repository.show("PAY-1900");
    assert_eq!(stale["status"], "proposed");
    assert_eq!(stale["task_changed"], true);
    let refused = repository.approve("PAY-1900");
    assert!(text(&refused.stderr).contains("changed after contract PAY-1900@v1 was compiled"));
    let (_, second) = repository.compile("PAY-1900");
    assert_eq!(second["version"], 2);
    assert_eq!(
        ids(&second, "MUST_CHANGE"),
        ["symbol:java:com.acme.payments.PaymentService.refund"]
    );
    let (_, first) = repository.json(&["task", "contract", "show", "PAY-1900", "--version", "1"]);
    assert_eq!(first["status"], "superseded");
    let (_, proposal) = repository.json(&["policy", "show", "task_pay_1900_v1"]);
    assert_eq!(proposal["status"], "superseded");

    assert!(repository.approve("PAY-1900").status.success());
    repository.task(
        "PAY-1900",
        DUPLICATE,
        Some(
            "Fix duplicate payment processing in `PaymentService.refund` and `PaymentService.fee`.",
        ),
    );
    repository.compile("PAY-1900");
    let (_, approved) =
        repository.json(&["task", "contract", "show", "PAY-1900", "--version", "2"]);
    assert_eq!(
        approved["status"], "approved",
        "in force until v3 is approved"
    );
    assert!(repository.approve("PAY-1900").status.success());
    let (_, replaced) =
        repository.json(&["task", "contract", "show", "PAY-1900", "--version", "2"]);
    assert_eq!(replaced["status"], "superseded");
    assert!(repository
        .root
        .join(".crane/retired/task_pay_1900_v2.crane")
        .exists());
}

/** No agent may change a task contract: compile, approve, and reject refuse agent environments,
 * an agent's shell trying them is denied and quarantines its session, and contract files cannot
 * be written; reading a contract is allowed */
#[test]
fn agents_cannot_modify_their_contract() {
    let repository = Repository::new();
    repository.task("PAY-1900", DUPLICATE, None);
    let agent = [("CLAUDECODE", "1")];
    let compile = repository.run(&["task", "contract", "compile", "PAY-1900"], &agent, "");
    assert!(text(&compile.stderr).contains("refuses to run in an agent environment"));
    repository.compile("PAY-1900");
    let contract = repository.show("PAY-1900");
    let confirmation = confirm(&contract);
    for args in [
        vec![
            "task",
            "contract",
            "approve",
            "PAY-1900",
            "--approver",
            "me",
            "--confirm",
            confirmation.as_str(),
        ],
        vec!["task", "contract", "reject", "PAY-1900", "--approver", "me"],
    ] {
        let output = repository.run(&args, &agent, "");
        assert!(!output.status.success());
        assert!(text(&output.stderr).contains("an agent can never change a task contract"));
    }
    let read = repository.run(&["task", "contract", "show", "PAY-1900"], &agent, "");
    assert!(read.status.success(), "agents may read their contract");
    assert_eq!(repository.show("PAY-1900")["status"], "proposed");

    repository.session("agent", "PAY-1900");
    let write = repository.hook("agent", json!({"tool_name": "Write", "tool_input": {"file_path": repository.root.join(".crane/task-contracts/PAY-1900/v1.json").to_string_lossy(), "content": "{}"}}));
    assert_eq!(write.status.code(), Some(2));
    let shell = repository.hook("agent", json!({"tool_name": "Bash", "tool_input": {"command": format!("crane task contract approve PAY-1900 --approver me --confirm {confirmation}")}}));
    assert_eq!(shell.status.code(), Some(2));
    let (_, status) = repository.json(&["autonomy", "status", "claude-agent"]);
    assert_eq!(
        status["safety"], "quarantined",
        "trying to approve its own contract is self-escalation: {status}"
    );
    assert_eq!(repository.show("PAY-1900")["status"], "proposed");
}

/** The dashboard API serves the same contracts and decisions as the CLI */
#[test]
fn dashboard_contract_api() {
    let repository = Repository::new();
    repository.crane(&["repo", "connect"]);
    repository.task("PAY-1900", DUPLICATE, None);
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
    let (ok, compiled) = api(
        "POST",
        "/api/tasks/contracts/PAY-1900/compile",
        Some(json!({"by": "lead"})),
    );
    assert!(ok, "{compiled}");
    assert_eq!(compiled["status"], "proposed");
    let (_, listed) = api("GET", "/api/tasks/contracts", None);
    assert_eq!(listed, repository.json(&["task", "contract", "list"]).1);
    let (ok, approved) = api(
        "POST",
        "/api/tasks/contracts/PAY-1900/approve",
        Some(json!({"approver": "lead", "confirm": confirm(&compiled)})),
    );
    assert!(ok, "{approved}");
    assert_eq!(approved["status"], "approved");
    let (_, shown) = api("GET", "/api/tasks/contracts/PAY-1900", None);
    assert_eq!(shown["status"], "approved");
    let (_, history) = api("GET", "/api/tasks/contracts/PAY-1900/history", None);
    assert_eq!(history["chain"]["status"], "verified");
    let (_, activation) = api("GET", "/api/policies/activation", None);
    assert_eq!(
        activation["layers"]["task"]["policies"],
        json!(["task_pay_1900_v1"])
    );
    let (missing, _) = api("GET", "/api/tasks/contracts/NOPE-1", None);
    assert!(!missing);
}
