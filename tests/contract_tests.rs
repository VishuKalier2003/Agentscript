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
const SERVICE: &str = "pay/service.py";

/** Payment service source */
const PAYMENT: &str = "class PaymentService:\n    def charge(self, amount):\n        return self.fee(amount) + amount\n\n    def retry(self, amount):\n        return self.charge(amount)\n\n    def fee(self, amount):\n        return amount // 10\n";

/** The organization's own test of the payment service */
const ORGANIZATIONAL_TEST: &str = "pay/test_service.py";

/** Organizational test source */
const TEST_SOURCE: &str = "import os\nimport sys\n\nsys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))\nfrom service import PaymentService\n\n\ndef test_charge():\n    assert PaymentService().charge(100) == 110\n\n\nif __name__ == \"__main__\":\n    test_charge()\n";

/** The permanent policy: charge is preserved */
const PRESERVE: &str =
    "policy payments_core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n";

/** The task policy: retry must change its business logic, anywhere in its folder */
const TARGET: &str = "policy task_pay_7 {\n    checkpoint baseline;\n    target --function PaymentService.retry scope folder change_type logical_bn;\n}\n";

/** Find a Python interpreter for the test commands
 * Input
    - None
 * Output
    - String program name
*/
fn python() -> String {
    for candidate in ["python3", "python"] {
        let works = Command::new(candidate)
            .args(["-c", "print(1)"])
            .output()
            .is_ok_and(|output| output.status.success() && output.stdout.starts_with(b"1"));
        if works {
            return candidate.into();
        }
    }
    panic!("these tests need python3 or python on PATH");
}

/** A temporary repository with Crane initialized and both policies active, removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Repository {
    root: PathBuf,
}

impl Repository {
    /** Create the fixture repository: the service, its organizational test, the checkpoint, both
     * policies, and a Python test command
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
            "crane-contract-tests-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Self { root };
        repository.write("README.md", "# Payments\n");
        repository.write(SERVICE, PAYMENT);
        repository.write(ORGANIZATIONAL_TEST, TEST_SOURCE);
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "lead@example.com"],
            vec!["config", "user.name", "Crane Contract Test"],
            vec!["config", "core.autocrlf", "false"],
            vec!["add", "."],
            vec!["commit", "-qm", "baseline"],
        ] {
            repository.git(&args);
        }
        assert!(repository.crane(&["init"], "").status.success());
        assert!(repository
            .crane(&["checkpoint", "--name", "baseline"], "")
            .status
            .success());
        repository.write(".crane/policies/payments_core.crane", PRESERVE);
        repository.write(".crane/policies/task_pay_7.crane", TARGET);
        let runner = "import runpy, sys\nfor path in sys.argv[1:]:\n    runpy.run_path(path, run_name='__main__')\n";
        repository.write(
            ".crane/testing.json",
            &json!({"timeout_seconds": 60, "commands": {"python": [python(), "-c", runner, "{files}"]}}).to_string(),
        );
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
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /** Run crane as a human in the repository
     * Input
        - args: &[&str] - crane arguments
        - stdin: &str - text written to stdin
     * Output
        - Output
    */
    fn crane(&self, args: &[&str], stdin: &str) -> Output {
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
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /** Send a Claude hook event for session s1
     * Input
        - event: &str - hook event
        - payload: Value - tool_name and tool_input, or other fields
     * Output
        - Output
    */
    fn hook(&self, event: &str, mut payload: Value) -> Output {
        payload["session_id"] = json!("s1");
        self.crane(
            &["agent", "hook", "--event", event, "--profile", "claude"],
            &payload.to_string(),
        )
    }

    /** Write a file as the agent: authorized before, written, then reported after
     * Input
        - path: &str - relative path
        - content: &str - new content
     * Output
        - None (panics if the hooks refuse it)
    */
    fn agent_write(&self, path: &str, content: &str) {
        let payload = json!({"tool_name": "Write", "tool_input": {"file_path": self.root.join(path).to_string_lossy(), "content": content}});
        let before = self.hook("pre-tool-use", payload.clone());
        assert!(before.status.success(), "{}", text(&before.stdout));
        self.write(path, content);
        let after = self.hook("post-tool-use", payload);
        assert!(after.status.success(), "{}", text(&after.stdout));
    }

    /** Start session s1 through the Claude hook
     * Input
        - None
     * Output
        - None
    */
    fn start(&self) {
        assert!(self
            .hook("session-start", json!({"source": "startup"}))
            .status
            .success());
    }

    /** Run crane test-contract --json and parse the report
     * Input
        - extra: &[&str] - more arguments
     * Output
        - (bool, Value) whether crane succeeded, and the report
    */
    fn report(&self, extra: &[&str]) -> (bool, Value) {
        let mut args = vec!["test-contract", "--json"];
        args.extend_from_slice(extra);
        let output = self.crane(&args, "");
        let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!("{error}: {}{}", text(&output.stdout), text(&output.stderr))
        });
        (output.status.success(), report)
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

/** Find one contract test by target and check
 * Input
    - report: &Value - test-contract report
    - target: &str - qualified target
    - check: &str - check name
 * Output
    - Value the test
*/
fn test_of(report: &Value, target: &str, check: &str) -> Value {
    report["contract_tests"]["tests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|test| test["target"] == target && test["check"] == check)
        .unwrap_or_else(|| panic!("no {check} test for {target}: {report}"))
        .clone()
}

/** --plan generates the contract tests without running them: checkpoint, identity, and scope for
 * preserve; changed, change type, and scope for target
 */
#[test]
fn plan_generates_tests_per_clause() {
    let repository = Repository::new();
    let (ok, report) = repository.report(&["--plan"]);
    assert!(ok, "{report}");
    let tests = report["contract_tests"]["tests"].as_array().unwrap();
    let names = tests
        .iter()
        .map(|test| {
            format!(
                "{} {} {}",
                test["rule"].as_str().unwrap(),
                test["target"].as_str().unwrap(),
                test["check"].as_str().unwrap()
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "preserve PaymentService.charge checkpoint",
            "preserve PaymentService.charge identity",
            "preserve PaymentService.charge scope",
            "target PaymentService.retry changed",
            "target PaymentService.retry change_type",
            "target PaymentService.retry scope",
        ]
    );
    assert!(tests.iter().all(|test| test["status"] == "scheduled"));
    assert!(report["ordinary_tests"].is_null());
    assert_eq!(report["status"], "scheduled");
}

/** Before the task is done, the target's tests fail (so crane fails) while the preserve tests
 * pass, and the organizational tests still run and pass on their own
 */
#[test]
fn unchanged_target_fails_its_contract_tests() {
    let repository = Repository::new();
    let (ok, report) = repository.report(&[]);
    assert!(!ok);
    assert_eq!(report["status"], "failed");
    assert_eq!(
        test_of(&report, "PaymentService.charge", "checkpoint")["status"],
        "passed"
    );
    assert_eq!(
        test_of(&report, "PaymentService.charge", "identity")["status"],
        "passed"
    );
    assert_eq!(
        test_of(&report, "PaymentService.retry", "changed")["status"],
        "failed"
    );
    assert_eq!(
        test_of(&report, "PaymentService.retry", "scope")["status"],
        "passed"
    );
    let ordinary = &report["ordinary_tests"];
    assert_eq!(ordinary["status"], "passed", "{ordinary}");
    assert_eq!(
        ordinary["organizational"][0]["files"],
        json!([ORGANIZATIONAL_TEST])
    );
    assert_eq!(ordinary["organizational"][0]["status"], "passed");
}

/** A completed task passes every contract test, and the terminal report keeps contract tests and
 * ordinary tests in separate sections
 */
#[test]
fn completed_task_passes_and_reports_sections_separately() {
    let repository = Repository::new();
    repository.write(
        SERVICE,
        &PAYMENT.replace(
            "return self.charge(amount)",
            "return self.charge(amount) + 1",
        ),
    );
    let (ok, report) = repository.report(&[]);
    assert!(ok, "{report}");
    assert_eq!(report["contract_tests"]["failed"], 0, "{report}");
    assert_eq!(
        test_of(&report, "PaymentService.retry", "change_type")["status"],
        "passed"
    );
    let output = repository.crane(&["test-contract"], "");
    let human = text(&output.stdout);
    assert!(output.status.success(), "{human}");
    let contract = human.find("CONTRACT TESTS").expect("contract section");
    let ordinary = human.find("ORDINARY TESTS").expect("ordinary section");
    assert!(contract < ordinary);
    let section = &human[contract..ordinary];
    assert!(
        section.contains("✓ PaymentService.charge preserved"),
        "{human}"
    );
    assert!(
        section.contains("✓ PaymentService.retry changed"),
        "{human}"
    );
    assert!(
        section.contains("✓ PaymentService.retry logical_bn satisfied"),
        "{human}"
    );
    assert!(
        human[ordinary..].contains("✓ organizational python: 1 test files passed"),
        "{human}"
    );
    assert!(human.contains("Result: PASSED"));
}

/** Reformatting a preserved function fails the checkpoint comparison but keeps its semantic
 * identity, so the two checks are reported apart; a behaviour change fails both
 */
#[test]
fn checkpoint_and_identity_are_separate_checks() {
    let repository = Repository::new();
    repository.write(
        SERVICE,
        &PAYMENT
            .replace(
                "        return self.fee(amount) + amount\n",
                "        return self.fee( amount )  +  amount\n",
            )
            .replace(
                "return self.charge(amount)",
                "return self.charge(amount) + 1",
            ),
    );
    let (ok, report) = repository.report(&[]);
    assert!(!ok);
    assert_eq!(
        test_of(&report, "PaymentService.charge", "checkpoint")["status"],
        "failed"
    );
    assert_eq!(
        test_of(&report, "PaymentService.charge", "identity")["status"],
        "passed",
        "{report}"
    );

    repository.write(
        SERVICE,
        &PAYMENT
            .replace("return self.fee(amount) + amount", "return amount")
            .replace(
                "return self.charge(amount)",
                "return self.charge(amount) + 1",
            ),
    );
    let (_, report) = repository.report(&[]);
    let identity = test_of(&report, "PaymentService.charge", "identity");
    assert_eq!(identity["status"], "failed");
    assert!(identity["message"]
        .as_str()
        .unwrap()
        .contains("business logic"));
}

/** A test the agent wrote is marked agent-authored and runs apart from organizational tests, and
 * on its own it cannot satisfy a target, even one whose scope covers the test's folder
 */
#[test]
fn agent_tests_alone_do_not_satisfy_contract_tests() {
    let repository = Repository::new();
    repository.start();
    repository.agent_write(
        "pay/test_retry.py",
        "def test_retry():\n    assert True\n\n\nif __name__ == \"__main__\":\n    test_retry()\n",
    );
    let (ok, report) = repository.report(&[]);
    assert!(!ok);
    let changed = test_of(&report, "PaymentService.retry", "changed");
    assert_eq!(changed["status"], "failed", "{report}");
    assert!(changed["message"]
        .as_str()
        .unwrap()
        .contains("agent-authored tests"));
    let ordinary = &report["ordinary_tests"];
    assert_eq!(
        ordinary["agent_authored"][0]["files"],
        json!(["pay/test_retry.py"])
    );
    assert_eq!(ordinary["agent_authored"][0]["status"], "passed");
    assert_eq!(
        ordinary["organizational"][0]["files"],
        json!([ORGANIZATIONAL_TEST])
    );
    assert_eq!(ordinary["status"], "passed");

    // Once the agent changes the target itself, the same contract tests pass
    repository.agent_write(
        SERVICE,
        &PAYMENT.replace(
            "return self.charge(amount)",
            "return self.charge(amount) + 1",
        ),
    );
    let (ok, report) = repository.report(&[]);
    assert!(ok, "{report}");
}

/** An organizational test an agent changes stays organizational and is flagged for review; one
 * that disappears fails the ordinary tests, because organizational tests must keep running
 */
#[test]
fn organizational_tests_keep_running() {
    let repository = Repository::new();
    repository.write(
        SERVICE,
        &PAYMENT.replace(
            "return self.charge(amount)",
            "return self.charge(amount) + 1",
        ),
    );
    repository.start();
    repository.agent_write(ORGANIZATIONAL_TEST, &TEST_SOURCE.replace("== 110", ">= 0"));
    let (ok, report) = repository.report(&[]);
    assert!(ok, "{report}");
    let ordinary = &report["ordinary_tests"];
    assert_eq!(
        ordinary["organizational"][0]["files"],
        json!([ORGANIZATIONAL_TEST])
    );
    assert!(ordinary["agent_authored"].as_array().unwrap().is_empty());
    assert_eq!(
        ordinary["findings"][0]["kind"],
        "organizational_test_modified_by_agent"
    );

    fs::remove_file(repository.root.join(ORGANIZATIONAL_TEST)).unwrap();
    let (ok, report) = repository.report(&[]);
    assert!(!ok);
    let ordinary = &report["ordinary_tests"];
    assert_eq!(ordinary["status"], "failed");
    assert!(ordinary["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["kind"] == "organizational_test_deleted"
            && finding["path"] == ORGANIZATIONAL_TEST));
    assert_eq!(
        report["contract_tests"]["failed"], 0,
        "contract compliance is judged apart from test health"
    );
}

/** --session tests the contract bound to the session, not policies added on disk afterwards */
#[test]
fn session_uses_its_bound_contract() {
    let repository = Repository::new();
    repository.start();
    repository.write(
        ".crane/policies/fees.crane",
        "policy fees {\n    checkpoint baseline;\n    preserve --function PaymentService.fee;\n}\n",
    );
    let fee = |report: &Value| {
        report["contract_tests"]["tests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|test| test["target"] == "PaymentService.fee")
    };
    let (_, repository_report) = repository.report(&["--plan"]);
    assert!(fee(&repository_report));
    let (_, session_report) = repository.report(&["--plan", "--session", "claude-s1"]);
    assert_eq!(session_report["source"], "session claude-s1");
    assert!(!fee(&session_report));
    assert_ne!(
        repository_report["contract_version"],
        session_report["contract_version"]
    );
    let unknown = repository.crane(&["test-contract", "--session", "nope"], "");
    assert!(!unknown.status.success());
}

/** Contract tests are mandatory at finalization: an unfinished task fails the session, a finished
 * one passes, and the attestation carries the contract tests either way
 */
#[test]
fn finalization_requires_contract_tests() {
    let repository = Repository::new();
    repository.start();
    let finalized = repository.crane(&["agent", "session", "finalize", "claude-s1"], "");
    let output = text(&finalized.stdout);
    assert!(
        output.contains("FAIL"),
        "{output}{}",
        text(&finalized.stderr)
    );
    assert!(output.contains("Contract tests (mandatory)"), "{output}");
    let attestation: Value = serde_json::from_str(
        &fs::read_to_string(
            repository
                .root
                .join(".crane/runtime/sessions/claude-s1/attestation.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(attestation["final_status"], "FAIL");
    assert!(attestation["contract_tests"]["failed"].as_u64().unwrap() > 0);
    assert_eq!(
        attestation["reconciliation"]["contract_tests_passed"],
        false
    );
    assert!(attestation["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["violation_type"] == "contract_test_failed"));

    let repository = Repository::new();
    repository.start();
    repository.agent_write(
        SERVICE,
        &PAYMENT.replace(
            "return self.charge(amount)",
            "return self.charge(amount) + 1",
        ),
    );
    let finalized = repository.crane(&["agent", "session", "finalize", "claude-s1"], "");
    let output = text(&finalized.stdout);
    assert!(
        output.contains("PASS"),
        "{output}{}",
        text(&finalized.stderr)
    );
    assert!(output.contains("0 failed"), "{output}");
}
