use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
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

/** Repository files: payments and billing services (billing with two modules), auth, analytics */
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
        "package com.acme.billing;\n\npublic class InvoiceService {\n    public double total(double amount) {\n        return amount * 1.0;\n    }\n\n    public double issue(double amount) {\n        return total(amount);\n    }\n\n    public int cancel(int amount) {\n        return refund(amount);\n    }\n\n    String label() {\n        return \"invoice\";\n    }\n}\n",
    ),
    (
        "services/billing/src/main/java/com/acme/billing/tax/TaxCalculator.java",
        "package com.acme.billing.tax;\n\npublic class TaxCalculator {\n    public double rate(double amount) {\n        return amount * 0.2;\n    }\n}\n",
    ),
    (
        "services/billing/src/test/java/com/acme/billing/InvoiceServiceTest.java",
        "package com.acme.billing;\n\nclass InvoiceServiceTest {\n    @Test\n    void totals() {\n        new InvoiceService().total(1.0);\n    }\n}\n",
    ),
    ("auth/login.py", "def authenticate(user):\n    return user\n"),
    ("analytics/report.py", "def charge(rows):\n    return len(rows)\n"),
];

/** Organization zones: Payments is Critical, Authentication is Restricted */
const ZONES: &str = "zone payments {\n    criticality critical;\n    autonomy assisted;\n    select subsystem payments;\n}\n\nzone authentication {\n    criticality restricted;\n    autonomy observe;\n    select subsystem auth;\n}\n";

/** A permanent organization policy: charge never changes */
const POLICY: &str = "policy payments_core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n";

/** A temporary repository named shop (by its origin remote), with Crane initialized, a baseline
 * checkpoint, zones, and a permanent policy, removed on drop
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
            "crane-tasks-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Self { root };
        for (path, content) in FILES {
            repository.write(path, content);
        }
        repository.git(&["init", "-q"]);
        repository.git(&["config", "user.email", "lead@example.com"]);
        repository.git(&["config", "user.name", "Crane Tasks Test"]);
        repository.git(&["config", "core.autocrlf", "false"]);
        repository.git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/shop.git",
        ]);
        repository.git(&["add", "."]);
        repository.git(&["commit", "-qm", "baseline"]);
        assert!(repository.crane(&["init"]).status.success());
        assert!(repository
            .crane(&["checkpoint", "--name", "baseline"])
            .status
            .success());
        repository.write(".crane/zones/org.zone", ZONES);
        repository.write(".crane/policies/payments_core.crane", POLICY);
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

    /** Run git in the repository and require success
     * Input
        - args: &[&str] - git arguments
     * Output
        - None
    */
    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("git should execute");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /** Run crane as a human (agent markers removed)
     * Input
        - args: &[&str] - crane arguments
     * Output
        - Output
    */
    fn crane(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
        command.args(args).current_dir(&self.root);
        for marker in AGENT_MARKERS {
            command.env_remove(marker);
        }
        command.output().expect("crane should execute")
    }

    /** Write a task file and plan it as JSON, returning the exit status and the plan
     * Input
        - task: Value - normalized task (its task_id names the file)
        - extra: &[&str] - extra arguments
     * Output
        - (bool, Value) success and plan
    */
    fn plan(&self, task: Value, extra: &[&str]) -> (bool, Value) {
        let id = task["task_id"].as_str().unwrap().to_string();
        self.write(
            &format!(".crane/tasks/{id}.json"),
            &serde_json::to_string_pretty(&task).unwrap(),
        );
        let mut args = vec!["task", "plan", id.as_str(), "--json"];
        args.extend_from_slice(extra);
        let output = self.crane(&args);
        let plan = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "plan JSON expected: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.success(), plan)
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

/** List the ids of one contract section
 * Input
    - plan: &Value - plan
    - section: &str - MUST_CHANGE or MUST_NOT_CHANGE
 * Output
    - Vec<String>
*/
fn ids(plan: &Value, section: &str) -> Vec<String> {
    plan["contract"][section]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_string())
        .collect()
}

/** List the fields of every clarification
 * Input
    - plan: &Value - plan
 * Output
    - Vec<String>
*/
fn clarification_fields(plan: &Value) -> Vec<String> {
    plan["clarifications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["field"].as_str().unwrap().to_string())
        .collect()
}

/** Build a task with the usual fields
 * Input
    - id: &str - task id
    - title: &str - title
    - description: &str - description
    - criteria: &[&str] - acceptance criteria
 * Output
    - Value
*/
fn task(id: &str, title: &str, description: &str, criteria: &[&str]) -> Value {
    json!({
        "task_format": 1,
        "task_id": id,
        "title": title,
        "description": description,
        "acceptance_criteria": criteria,
        "repositories": ["acme/shop"],
        "requester": "pm@example.com",
        "team": "billing",
        "priority": "high",
        "labels": ["billing"],
    })
}

/** Id prefix of billing symbols */
const BILLING: &str = "symbol:java:com.acme.billing.InvoiceService";

/** Id prefix of payment symbols */
const PAYMENTS: &str = "symbol:java:com.acme.payments.PaymentService";

/** A well-written task becomes a bounded contract: its target, what it says to keep, the module
 * in scope with what may change there, the tests to run, and deterministic AgentScript; no zone
 * asks for approval
 */
#[test]
fn well_written_task_is_planned() {
    let repository = Repository::new();
    let bill = task(
        "BILL-7",
        "Round invoice totals to cents",
        "Change `InvoiceService.total` so that it rounds to two decimals. `InvoiceService.issue` must not change.",
        &["`InvoiceService.total` returns 10.01 for 10.005"],
    );
    let (success, plan) = repository.plan(bill.clone(), &[]);
    assert!(success, "{plan}");
    assert_eq!(plan["status"], "planned");
    assert_eq!(plan["clarifications"], json!([]));
    assert_eq!(ids(&plan, "MUST_CHANGE"), [format!("{BILLING}.total")]);
    assert_eq!(
        plan["contract"]["MUST_CHANGE"][0]["rule"],
        "target --function InvoiceService.total;"
    );
    assert_eq!(ids(&plan, "MUST_NOT_CHANGE"), [format!("{BILLING}.issue")]);
    let may = plan["contract"]["MAY_CHANGE"].as_array().unwrap();
    assert!(may.contains(&json!(format!("{BILLING}.cancel"))));
    assert!(!may
        .iter()
        .any(|id| id.as_str().unwrap().contains("payments")));
    assert_eq!(
        plan["contract"]["TASK_SCOPE"]["modules"],
        json!(["module:java:com.acme.billing"])
    );
    assert_eq!(
        plan["contract"]["TASK_SCOPE"]["services"],
        json!(["service:services/billing"])
    );
    assert_eq!(plan["contract"]["REQUIRES_APPROVAL"], json!([]));
    assert_eq!(
        plan["contract"]["EXPECTED_TESTS"],
        json!([
            {"test": "symbol:java:com.acme.billing.InvoiceServiceTest.totals", "covers": format!("{BILLING}.total")},
            {"test_file": "services/billing/src/test/java/com/acme/billing/InvoiceServiceTest.java", "run": true},
        ])
    );
    assert_eq!(plan["zones"], json!([]));
    assert_eq!(
        plan["agentscript"],
        "policy task_bill_7 {\n    checkpoint baseline;\n    target --function InvoiceService.total;\n    preserve --function InvoiceService.issue;\n}\n"
    );
    assert_eq!(plan["policy_packs"]["policies"], json!(["payments_core"]));
    assert_eq!(plan["checkpoint"]["name"], "baseline");
    assert_eq!(plan["active"], false);
    assert!(!repository
        .root
        .join(".crane/policies/task_bill_7.crane")
        .exists());

    // Deterministic, and readable for humans
    assert_eq!(repository.plan(bill, &[]).1, plan);
    let human =
        String::from_utf8_lossy(&repository.crane(&["task", "plan", "BILL-7"]).stdout).into_owned();
    for text in [
        "Status: planned",
        "MUST_CHANGE (1):",
        "InvoiceService.total",
        "TASK_SCOPE: services [service:services/billing]",
        "run symbol:java:com.acme.billing.InvoiceServiceTest.totals",
        "target --function InvoiceService.total;",
    ] {
        assert!(human.contains(text), "{text}\n{human}");
    }
}

/** Ambiguous and vague tasks get task_needs_clarification with exactly what is missing; plain
 * words never become authority
 */
#[test]
fn ambiguous_and_vague_tasks_need_clarification() {
    let repository = Repository::new();
    let (success, plan) = repository.plan(
        json!({
            "task_id": "AMB-1",
            "title": "Improve charging",
            "description": "Fix the `charge` logic everywhere as needed.",
            "references": {"symbols": ["Ghost.run"]},
        }),
        &[],
    );
    assert!(!success);
    assert_eq!(plan["status"], "task_needs_clarification");
    let fields = clarification_fields(&plan);
    for field in [
        "acceptance_criteria",
        "repositories",
        "description",
        "references.symbols",
    ] {
        assert!(fields.contains(&field.to_string()), "{field}: {fields:?}");
    }
    let messages = plan["clarifications"].to_string();
    assert!(
        messages
            .contains("symbol `Ghost.run` (references.symbols) does not exist in this repository"),
        "{messages}"
    );
    assert!(messages.contains("symbol `charge` (description) is ambiguous: it matches symbol:python:analytics.report.charge, symbol:java:com.acme.payments.PaymentService.charge"), "{messages}");
    assert!(messages.contains("'everywhere'"), "{messages}");
    assert!(messages.contains(", shop)"), "{messages}");
    assert_eq!(plan["agentscript"], Value::Null);
    assert_eq!(plan["contract"]["MUST_CHANGE"], json!([]));

    // Plain words name nothing: no authority comes from them
    let (_, vague) = repository.plan(
        task(
            "VAGUE-1",
            "Make refunds faster",
            "Make the refund flow and authenticate faster.",
            &["refunds are faster"],
        ),
        &[],
    );
    assert_eq!(vague["status"], "task_needs_clarification");
    assert_eq!(clarification_fields(&vague), ["references.symbols"]);
    assert_eq!(vague["contract"]["MAY_CHANGE"], json!([]));
    assert_eq!(vague["contract"]["TASK_SCOPE"]["modules"], json!([]));
}

/** A task for another repository is unrelated and yields no contract */
#[test]
fn unrelated_task_is_reported() {
    let repository = Repository::new();
    let mut web = task(
        "WEB-3",
        "Update landing page copy",
        "Change `Hero.render` to show the new slogan.",
        &["the slogan shows"],
    );
    web["repositories"] = json!(["acme/website"]);
    let (success, plan) = repository.plan(web, &[]);
    assert!(!success);
    assert_eq!(plan["status"], "task_unrelated");
    assert_eq!(plan["repository"]["related"], false);
    assert_eq!(plan["agentscript"], Value::Null);
    let output = repository.crane(&["task", "plan", "WEB-3"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("task WEB-3 is task_unrelated"));
}

/** A multi-module task scopes every module it touches; within one service it needs no approval,
 * across services it needs human approval
 */
#[test]
fn multi_module_task_scopes_each_module() {
    let repository = Repository::new();
    let (success, plan) = repository.plan(
        task(
            "BILL-9",
            "Tax-inclusive totals",
            "Change `InvoiceService.total` to include tax and update `TaxCalculator.rate` to 21%.",
            &["totals include 21% tax"],
        ),
        &[],
    );
    assert!(success, "{plan}");
    assert_eq!(
        plan["contract"]["TASK_SCOPE"]["modules"],
        json!([
            "module:java:com.acme.billing",
            "module:java:com.acme.billing.tax"
        ])
    );
    assert_eq!(
        plan["contract"]["TASK_SCOPE"]["services"],
        json!(["service:services/billing"])
    );
    assert_eq!(
        ids(&plan, "MUST_CHANGE"),
        [
            format!("{BILLING}.total"),
            "symbol:java:com.acme.billing.tax.TaxCalculator.rate".to_string()
        ]
    );
    assert_eq!(plan["contract"]["REQUIRES_APPROVAL"], json!([]));
    assert!(plan["contract"]["EXPECTED_TESTS"]
        .to_string()
        .contains("\"add_test_for\":\"symbol:java:com.acme.billing.tax.TaxCalculator.rate\""));

    let (success, cross) = repository.plan(
        task(
            "BILL-10",
            "Fee-aware totals",
            "Change `InvoiceService.total` and `PaymentService.fee` together.",
            &["totals include the fee"],
        ),
        &[],
    );
    assert!(success, "{cross}");
    assert_eq!(
        cross["contract"]["TASK_SCOPE"]["services"],
        json!(["service:services/billing", "service:services/payments"])
    );
    let approvals = cross["contract"]["REQUIRES_APPROVAL"].to_string();
    assert!(approvals.contains("spans 2 services"), "{approvals}");
    assert!(
        approvals.contains("zones payments (critical) require human approval"),
        "{approvals}"
    );
}

/** A task touching a Critical zone is planned with approval required, keeps code outside its scope
 * and permanently preserved code unchanged, and can be stored as a proposal only a human
 * activates
 */
#[test]
fn critical_zone_task_requires_approval() {
    let repository = Repository::new();
    let (success, plan) = repository.plan(
        task(
            "PAY-1821",
            "Reject negative refunds",
            "Make `PaymentService.refund` reject negative amounts without changing `PaymentService.charge`.",
            &["`PaymentService.refund` throws for amounts below zero"],
        ),
        &["--propose"],
    );
    assert!(success, "{plan}");
    assert_eq!(plan["status"], "planned");
    assert_eq!(ids(&plan, "MUST_CHANGE"), [format!("{PAYMENTS}.refund")]);
    let must_not = &plan["contract"]["MUST_NOT_CHANGE"];
    assert_eq!(
        ids(&plan, "MUST_NOT_CHANGE"),
        [format!("{BILLING}.cancel"), format!("{PAYMENTS}.charge")]
    );
    assert!(must_not[0]["because"]
        .to_string()
        .contains("outside the task scope"));
    assert!(must_not[1]["because"]
        .to_string()
        .contains("permanent policy payments_core"));
    let approval = &plan["contract"]["REQUIRES_APPROVAL"][0];
    assert_eq!(approval["entity"], format!("{PAYMENTS}.refund"));
    assert_eq!(approval["zones"], json!(["payments"]));
    assert_eq!(plan["zones"][0]["zone_id"], "payments");
    assert_eq!(plan["zones"][0]["criticality"], "critical");
    assert!(plan["contract"]["EXPECTED_TESTS"]
        .to_string()
        .contains(&format!("\"add_test_for\":\"{PAYMENTS}.refund\"")));
    // charge is already enforced by the permanent policy, so the task contract does not repeat it
    let script = "policy task_pay_1821 {\n    checkpoint baseline;\n    target --function PaymentService.refund;\n    preserve --function InvoiceService.cancel;\n}\n";
    assert_eq!(plan["agentscript"], script);

    // Stored as a pending proposal: not active until a human approves it
    assert_eq!(plan["proposal"]["status"], "pending");
    assert!(!repository
        .root
        .join(".crane/policies/task_pay_1821.crane")
        .exists());
    let proposal: Value = serde_json::from_slice(
        &repository
            .crane(&["policy", "show", "task_pay_1821", "--json"])
            .stdout,
    )
    .unwrap();
    assert_eq!(
        proposal["origin"],
        json!({"kind": "task", "task_id": "PAY-1821"})
    );
    assert_eq!(proposal["policy"], script);
    let digest = proposal["policy_digest"].as_str().unwrap()[7..19].to_string();
    let mut agent = Command::new(env!("CARGO_BIN_EXE_crane"));
    agent
        .args([
            "policy",
            "approve",
            "task_pay_1821",
            "--approver",
            "agent",
            "--confirm",
            &digest,
        ])
        .current_dir(&repository.root)
        .env("CLAUDECODE", "1");
    assert!(!agent.output().unwrap().status.success());
    assert!(repository
        .crane(&["policy", "regenerate", "task_pay_1821"])
        .status
        .success());
    let approved = repository.crane(&[
        "policy",
        "approve",
        "task_pay_1821",
        "--approver",
        "lead",
        "--confirm",
        &digest,
    ]);
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    assert_eq!(
        fs::read_to_string(repository.root.join(".crane/policies/task_pay_1821.crane")).unwrap(),
        script
    );
    let (_, again) = repository.plan(
        task(
            "PAY-1821",
            "Reject negative refunds",
            "Make `PaymentService.refund` reject negative amounts without changing `PaymentService.charge`.",
            &["`PaymentService.refund` throws for amounts below zero"],
        ),
        &[],
    );
    assert!(again["previous_contracts"]
        .to_string()
        .contains("\"proposal\":\"task_pay_1821\""));
}

/** A task that must change what a permanent policy preserves, or what a Restricted zone only lets
 * agents observe, conflicts and yields no contract
 */
#[test]
fn task_conflicting_with_permanent_policy() {
    let repository = Repository::new();
    let (success, plan) = repository.plan(
        task(
            "PAY-1900",
            "Add a surcharge",
            "Add a surcharge in `PaymentService.charge`.",
            &["charges include a 2% surcharge"],
        ),
        &["--propose"],
    );
    assert!(!success);
    assert_eq!(plan["status"], "task_conflicts_with_policy");
    let conflict = &plan["conflicts"][0];
    assert_eq!(conflict["kind"], "permanent_policy");
    assert_eq!(conflict["policy"], "payments_core");
    assert_eq!(conflict["entity"], format!("{PAYMENTS}.charge"));
    assert!(conflict["message"]
        .as_str()
        .unwrap()
        .contains("unless a human changes that policy"));
    assert_eq!(plan["agentscript"], Value::Null);
    assert_eq!(plan["proposal"], Value::Null);
    assert!(!repository
        .root
        .join(".crane/proposals/task_pay_1900.json")
        .exists());

    let (_, auth) = repository.plan(
        task(
            "AUTH-5",
            "Log failed logins",
            "Change `authenticate` to log failures.",
            &["failures are logged"],
        ),
        &[],
    );
    assert_eq!(auth["status"], "task_conflicts_with_policy");
    assert_eq!(auth["conflicts"][0]["kind"], "zone");
    assert_eq!(auth["conflicts"][0]["zones"], json!(["authentication"]));
}
