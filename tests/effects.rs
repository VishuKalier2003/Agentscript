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
const PAYMENT: &str = "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    public int refund(int amount) {\n        return charge(-amount);\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n";

/** Repository files: payments, billing with a runnable test, analytics with a test that would fail
 * if it ran, auth, and the scripts an agent might run */
const FILES: &[(&str, &str)] = &[
    ("README.md", "# Shop\n"),
    ("services/payments/pom.xml", "<project/>\n"),
    (SERVICE, PAYMENT),
    ("billing/invoice.py", "def total(amount):\n    return round(amount, 2)\n"),
    (
        "billing/test_invoice.py",
        "import os\nimport sys\n\nsys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))\nfrom invoice import total\n\n\ndef test_total():\n    assert total(10) == 10\n\n\nif __name__ == \"__main__\":\n    test_total()\n",
    ),
    ("analytics/report.py", "def summarize(rows):\n    return len(rows)\n"),
    (
        "analytics/test_report.py",
        "from report import summarize\n\n\ndef test_summarize():\n    assert False, \"this test must not run\"\n\n\nif __name__ == \"__main__\":\n    test_summarize()\n",
    ),
    ("auth/login.py", "def authenticate(user):\n    return user\n"),
    (
        "scripts/rewrite.py",
        "from pathlib import Path\n\npath = Path(\"services/payments/src/main/java/com/acme/payments/PaymentService.java\")\npath.write_text(path.read_text().replace(\"return fee(amount) + amount;\", \"return fee(amount) + amount * 2;\"))\n",
    ),
    (
        "tools/gen.py",
        "import sys\nfrom pathlib import Path\n\ntarget = Path(sys.argv[1])\ntarget.parent.mkdir(parents=True, exist_ok=True)\ntarget.write_text(\"def get_invoice(client, invoice_id):\\n    return client.fetch(invoice_id)\\n\\n\\ndef list_invoices(client):\\n    return client.fetch_all()\\n\")\n",
    ),
];

/** Zones: Authentication is Restricted */
const ZONES: &str = "zone authentication {\n    criticality restricted;\n    autonomy observe;\n    select subsystem auth;\n}\n";

/** The permanent policy */
const PRESERVE: &str = "policy payments_core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n";

/** The task contract: refund must change */
const TARGET: &str = "policy task_pay_1821 {\n    checkpoint baseline;\n    target --function PaymentService.refund;\n}\n";

/** Find a Python interpreter for the scripts the tests run
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

/** A temporary repository with Crane initialized, removed on drop
 * Fields
    - root: PathBuf - repository root
    - python: String - Python interpreter
*/
struct Repository {
    root: PathBuf,
    python: String,
}

impl Repository {
    /** Create the fixture repository
     * Input
        - target: bool - also activate the task contract (refund must change)
     * Output
        - Repository
    */
    fn new(target: bool) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-effects-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Self {
            root,
            python: python(),
        };
        for (path, content) in FILES {
            repository.write(path, content);
        }
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "lead@example.com"],
            vec!["config", "user.name", "Crane Effects Test"],
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
        repository.write(".crane/zones/org.zone", ZONES);
        repository.write(".crane/policies/payments_core.crane", PRESERVE);
        if target {
            repository.write(".crane/policies/task_pay_1821.crane", TARGET);
        }
        let runner = "import runpy, sys\nfor path in sys.argv[1:]:\n    runpy.run_path(path, run_name='__main__')\n";
        repository.write(
            ".crane/testing.json",
            &json!({"timeout_seconds": 60, "commands": {"python": [repository.python, "-c", runner, "{files}"]}}).to_string(),
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

    /** Read a file relative to the root
     * Input
        - path: &str - relative path
     * Output
        - String
    */
    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }

    /** Run git in the repository and require success
     * Input
        - args: &[&str] - git arguments
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
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /** Run a program in the repository, as the agent's shell tool would
     * Input
        - program: &str - program
        - args: &[&str] - arguments
     * Output
        - None (panics if it fails)
    */
    fn run(&self, program: &str, args: &[&str]) {
        let output = Command::new(program)
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /** Run crane as a human in a directory
     * Input
        - directory: &PathBuf - working directory
        - args: &[&str] - crane arguments
        - stdin: &str - text written to stdin
     * Output
        - Output
    */
    fn crane_in(&self, directory: &PathBuf, args: &[&str], stdin: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
        command
            .args(args)
            .current_dir(directory)
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

    /** Run crane as a human in the repository
     * Input
        - args: &[&str] - crane arguments
        - stdin: &str - text written to stdin
     * Output
        - Output
    */
    fn crane(&self, args: &[&str], stdin: &str) -> Output {
        self.crane_in(&self.root.clone(), args, stdin)
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

    /** Report that a shell command ran (post-tool-use for Bash) and return the hook output
     * Input
        - command: &str - the command the agent ran
     * Output
        - Output
    */
    fn after_shell(&self, command: &str) -> Output {
        self.hook(
            "post-tool-use",
            json!({"tool_name": "Bash", "tool_input": {"command": command}}),
        )
    }

    /** Return the effect of the latest post-tool-use event
     * Input
        - None
     * Output
        - Value
    */
    fn effect(&self) -> Value {
        fs::read_to_string(
            self.root
                .join(".crane/runtime/sessions/claude-s1/journal.jsonl"),
        )
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .rfind(|event| event["event"] == "post_tool_use")
        .expect("a post-tool-use event")["effect"]
            .clone()
    }

    /** Start session s1 (the baseline every effect is measured against)
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
}

impl Drop for Repository {
    /** Remove the temporary repository and any session worktree
     * Input
        - None (uses self)
     * Output
        - None
    */
    fn drop(&mut self) {
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&self.root)
            .output();
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

/** List the changes of an effect's symbols as "symbol change" strings
 * Input
    - effect: &Value - effect
 * Output
    - Vec<String>
*/
fn symbol_changes(effect: &Value) -> Vec<String> {
    effect["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|symbol| {
            format!(
                "{} {}",
                symbol["symbol"].as_str().unwrap(),
                symbol["change"].as_str().unwrap()
            )
        })
        .collect()
}

/** A direct edit: the effect names the file and the symbol it changed, and the target the change
 * satisfies
 */
#[test]
fn direct_edit_is_verified_by_its_effect() {
    let repository = Repository::new(true);
    repository.start();
    let edit = json!({"tool_name": "Edit", "tool_input": {"file_path": repository.root.join(SERVICE).to_string_lossy(), "old_string": "return charge(-amount);", "new_string": "return charge(-Math.abs(amount));"}});
    assert!(repository
        .hook("pre-tool-use", edit.clone())
        .status
        .success());
    repository.write(
        SERVICE,
        &PAYMENT.replace(
            "return charge(-amount);",
            "return charge(-Math.abs(amount));",
        ),
    );
    let post = repository.hook("post-tool-use", edit);
    assert!(post.status.success(), "{}", text(&post.stdout));
    let effect = repository.effect();
    assert_eq!(effect["files"]["modified"], json!([SERVICE]));
    assert_eq!(
        symbol_changes(&effect),
        [format!("{SERVICE}#PaymentService.refund modified")]
    );
    assert_eq!(
        effect["targets_satisfied"],
        json!(["PaymentService.refund"])
    );
    assert_eq!(effect["violations"], json!([]));
    assert_eq!(effect["clauses_checked"], 2);
}

/** A shell command that was allowed before it ran still has its effect verified: changing
 * PaymentService.charge through the shell is caught right after the command
 */
#[test]
fn shell_modification_is_caught_after_the_command() {
    let repository = Repository::new(false);
    repository.start();
    let bash = json!({"tool_name": "Bash", "tool_input": {"command": "sed -i 's/amount;/amount * 2;/' PaymentService.java"}});
    assert!(
        repository
            .hook("pre-tool-use", bash.clone())
            .status
            .success(),
        "the shell command itself is allowed"
    );
    repository.write(
        SERVICE,
        &PAYMENT.replace(
            "return fee(amount) + amount;",
            "return fee(amount) + amount * 2;",
        ),
    );
    let post = repository.hook("post-tool-use", bash);
    assert_eq!(post.status.code(), Some(2));
    assert!(text(&post.stdout).contains("\"violation_type\":\"source_changed\""));
    let effect = repository.effect();
    assert_eq!(
        symbol_changes(&effect),
        [format!("{SERVICE}#PaymentService.charge modified")]
    );
    assert_eq!(effect["violations"], json!(["source_changed"]));
}

/** A script the agent runs changes protected code: the actual effect is caught */
#[test]
fn script_driven_change_is_caught() {
    let repository = Repository::new(false);
    repository.start();
    repository.run(&repository.python, &["scripts/rewrite.py"]);
    let post = repository.after_shell("python scripts/rewrite.py");
    assert_eq!(post.status.code(), Some(2));
    assert!(text(&post.stdout).contains("Protected function was modified."));
    assert!(symbol_changes(&repository.effect())
        .contains(&format!("{SERVICE}#PaymentService.charge modified")));
}

/** Generated code is seen as added files and symbols; generating into a Restricted zone is an
 * unauthorized effect even though the shell command was allowed
 */
#[test]
fn generated_code_is_observed_and_zones_hold() {
    let repository = Repository::new(false);
    repository.start();
    repository.run(&repository.python, &["tools/gen.py", "generated/client.py"]);
    let post = repository.after_shell("python tools/gen.py generated/client.py");
    assert!(post.status.success(), "{}", text(&post.stdout));
    let effect = repository.effect();
    assert_eq!(effect["files"]["added"], json!(["generated/client.py"]));
    assert_eq!(
        symbol_changes(&effect),
        [
            "generated/client.py#get_invoice added",
            "generated/client.py#list_invoices added"
        ]
    );
    assert_eq!(
        effect["clauses_checked"], 0,
        "no contract clause covers generated/"
    );

    repository.run(&repository.python, &["tools/gen.py", "auth/client.py"]);
    let zoned = repository.after_shell("python tools/gen.py auth/client.py");
    assert_eq!(zoned.status.code(), Some(2));
    assert!(text(&zoned.stdout).contains("unauthorized_effect"));
    let effect = repository.effect();
    assert_eq!(effect["zones_touched"], json!(["authentication"]));
    assert_eq!(effect["violations"], json!(["unauthorized_effect"]));
}

/** A rename keeps symbols (reported as moved, no violation, since the preserved method still
 * resolves); renaming the preserved method itself is caught
 */
#[test]
fn rename_is_tracked() {
    let repository = Repository::new(false);
    repository.start();
    let moved = "services/payments/src/main/java/com/acme/payments/core/PaymentService.java";
    fs::create_dir_all(
        repository
            .root
            .join("services/payments/src/main/java/com/acme/payments/core"),
    )
    .unwrap();
    repository.git(&["mv", SERVICE, moved]);
    let post = repository.after_shell("git mv PaymentService.java core/PaymentService.java");
    assert!(post.status.success(), "{}", text(&post.stdout));
    let effect = repository.effect();
    assert_eq!(
        effect["files"]["renamed"],
        json!([{"from": SERVICE, "to": moved}])
    );
    assert!(symbol_changes(&effect).contains(&format!("{moved}#PaymentService.charge moved")));
    assert_eq!(effect["violations"], json!([]));

    repository.write(moved, &PAYMENT.replace("charge(", "collect("));
    let renamed = repository.after_shell("sed -i s/charge/collect/ core/PaymentService.java");
    assert_eq!(renamed.status.code(), Some(2));
    let changes = symbol_changes(&repository.effect());
    assert!(
        changes.contains(&format!("{moved}#PaymentService.charge removed")),
        "{changes:?}"
    );
    assert!(
        changes.contains(&format!("{moved}#PaymentService.collect added")),
        "{changes:?}"
    );
}

/** Deleting the file that holds protected code is caught */
#[test]
fn file_deletion_is_caught() {
    let repository = Repository::new(false);
    repository.start();
    fs::remove_file(repository.root.join(SERVICE)).unwrap();
    let post = repository.after_shell("rm PaymentService.java");
    assert_eq!(post.status.code(), Some(2));
    let effect = repository.effect();
    assert_eq!(effect["files"]["deleted"], json!([SERVICE]));
    assert!(symbol_changes(&effect).contains(&format!("{SERVICE}#PaymentService.charge removed")));
    assert!(!effect["violations"].as_array().unwrap().is_empty());
}

/** An unrelated change verifies no contract clause (incremental) and passes */
#[test]
fn unrelated_change_checks_nothing_else() {
    let repository = Repository::new(true);
    repository.start();
    repository.write("README.md", "# Shop\n\nNotes.\n");
    let post = repository.after_shell("echo Notes >> README.md");
    assert!(post.status.success());
    let effect = repository.effect();
    assert_eq!(effect["files"]["modified"], json!(["README.md"]));
    assert_eq!(effect["symbols"], json!([]));
    assert_eq!(effect["clauses_checked"], 0);
    assert_eq!(effect["clauses_total"], 2);
    assert_eq!(effect["violations"], json!([]));
}

/** A change reverted later: the violation is reported when it happens, the revert clears it, and
 * the session's cumulative effect shows nothing left
 */
#[test]
fn revert_after_change_leaves_no_effect() {
    let repository = Repository::new(false);
    repository.start();
    repository.write(
        SERVICE,
        &PAYMENT.replace("return fee(amount) + amount;", "return amount;"),
    );
    assert_eq!(
        repository.after_shell("python patch.py").status.code(),
        Some(2)
    );
    repository.write(SERVICE, PAYMENT);
    let reverted = repository.after_shell("git checkout -- .");
    assert!(reverted.status.success(), "{}", text(&reverted.stdout));
    let effect = repository.effect();
    assert_eq!(
        symbol_changes(&effect),
        [format!("{SERVICE}#PaymentService.charge modified")]
    );
    assert_eq!(effect["violations"], json!([]));
    let stop = repository.hook("stop", json!({"stop_hook_active": false}));
    assert!(stop.status.success(), "{}", text(&stop.stdout));
    let show: Value = serde_json::from_slice(
        &repository
            .crane(&["agent", "session", "show", "claude-s1"], "")
            .stdout,
    )
    .unwrap();
    let cumulative = &show["attestation"]["effects"]["files"];
    assert_eq!(
        cumulative,
        &json!({"added": [], "modified": [], "deleted": [], "renamed": []})
    );
    assert_eq!(show["attestation"]["final_status"], "PASS");
}

/** Affected tests run at stop (not after each tool call), only for the code that changed: the
 * billing test runs and fails, the unrelated failing analytics test never runs, and fixing the code
 * passes
 */
#[test]
fn affected_tests_run_only_for_changed_code() {
    let repository = Repository::new(false);
    repository.start();
    repository.write(
        "billing/invoice.py",
        "def total(amount):\n    return round(amount, 2) + 1\n",
    );
    assert!(repository.after_shell("python edit.py").status.success());
    let journal = repository.read(".crane/runtime/sessions/claude-s1/journal.jsonl");
    assert!(!journal.contains("\"tests\""), "no tests after a tool call");

    let stop = repository.hook("stop", json!({"stop_hook_active": false}));
    assert_eq!(stop.status.code(), Some(2), "{}", text(&stop.stdout));
    assert!(text(&stop.stdout).contains("tests_failed"));
    let show: Value = serde_json::from_slice(
        &repository
            .crane(&["agent", "session", "show", "claude-s1"], "")
            .stdout,
    )
    .unwrap();
    let tests = &show["attestation"]["tests"];
    assert_eq!(tests.as_array().unwrap().len(), 1);
    assert_eq!(tests[0]["language"], "python");
    assert_eq!(tests[0]["files"], json!(["billing/test_invoice.py"]));
    assert_eq!(tests[0]["status"], "failed");
    assert!(tests[0]["output"]
        .as_str()
        .unwrap()
        .contains("AssertionError"));
    assert_eq!(show["attestation"]["final_status"], "FAIL");

    repository.write("billing/invoice.py", "def total(amount):\n    return round(amount, 2)\n\n\ndef tax(amount):\n    return amount * 0.2\n");
    let verify = repository.crane(
        &[
            "agent",
            "session",
            "verify",
            "claude-s1",
            "--level",
            "tests",
        ],
        "",
    );
    let result: Value = serde_json::from_slice(&verify.stdout).unwrap();
    assert_eq!(result["tests"][0]["status"], "passed", "{result}");
    let stop = repository.hook("stop", json!({"stop_hook_active": false}));
    assert!(stop.status.success(), "{}", text(&stop.stdout));
    let show: Value = serde_json::from_slice(
        &repository
            .crane(&["agent", "session", "show", "claude-s1"], "")
            .stdout,
    )
    .unwrap();
    assert_eq!(show["attestation"]["tests"][0]["status"], "passed");
    assert_eq!(show["attestation"]["final_status"], "PASS");
}

/** An isolated session works in its own Git worktree: the agent's changes stay out of the
 * repository, effects are verified there, and after finalization the worktree is removed while its
 * branch is kept
 */
#[test]
fn isolated_session_works_in_its_own_worktree() {
    let repository = Repository::new(true);
    let started = repository.crane(
        &[
            "agent",
            "session",
            "start",
            "--profile",
            "claude",
            "--session",
            "iso",
            "--isolate",
        ],
        "",
    );
    assert!(started.status.success(), "{}", text(&started.stderr));
    let worktree = repository.root.join(".crane/runtime/worktrees/claude-iso");
    assert!(text(&started.stdout).contains("Isolated worktree:"));
    assert!(worktree.join(SERVICE).is_file());
    let show: Value = serde_json::from_slice(
        &repository
            .crane(&["agent", "session", "show", "claude-iso"], "")
            .stdout,
    )
    .unwrap();
    assert!(show["root"]
        .as_str()
        .unwrap()
        .replace('\\', "/")
        .ends_with(".crane/runtime/worktrees/claude-iso"));

    // The agent changes protected code inside the worktree: caught there, the repository untouched
    fs::write(
        worktree.join(SERVICE),
        PAYMENT.replace("return fee(amount) + amount;", "return amount;"),
    )
    .unwrap();
    let payload = json!({"session_id": "iso", "tool_name": "Bash", "tool_input": {"command": "python rewrite.py"}}).to_string();
    let caught = repository.crane_in(
        &worktree,
        &[
            "agent",
            "hook",
            "--event",
            "post-tool-use",
            "--profile",
            "claude",
        ],
        &payload,
    );
    assert_eq!(caught.status.code(), Some(2), "{}", text(&caught.stdout));
    assert_eq!(repository.read(SERVICE), PAYMENT);

    // It repairs that and does the task
    fs::write(
        worktree.join(SERVICE),
        PAYMENT.replace(
            "return charge(-amount);",
            "return charge(-Math.abs(amount));",
        ),
    )
    .unwrap();
    let done = repository.crane_in(
        &worktree,
        &[
            "agent",
            "hook",
            "--event",
            "post-tool-use",
            "--profile",
            "claude",
        ],
        &payload,
    );
    assert!(done.status.success(), "{}", text(&done.stdout));
    let finalized = repository.crane(&["agent", "session", "finalize", "claude-iso"], "");
    assert!(
        text(&finalized.stdout).contains("PASS"),
        "{}{}",
        text(&finalized.stdout),
        text(&finalized.stderr)
    );
    assert_eq!(
        repository.read(SERVICE),
        PAYMENT,
        "the repository itself never changed"
    );

    let cleanup = repository.crane(&["agent", "session", "cleanup", "claude-iso"], "");
    assert!(cleanup.status.success(), "{}", text(&cleanup.stderr));
    assert!(!worktree.exists());
    assert!(repository
        .git(&["branch", "--list", "crane/claude-iso"])
        .contains("crane/claude-iso"));
}
