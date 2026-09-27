use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SERVICE: &str = "class PaymentService:
    def charge(self, items):
        fee = compute_fee(items)
        tax = compute_tax(items)
        total = 0
        for item in items:
            total = total + item
        return total + fee + tax * 2


def compute_fee(items):
    return len(items)


def compute_tax(items):
    return 1


def unrelated():
    return 3
";

const OTHER: &str = "def helper():
    return 2
";

const HANDLER: &str = "from payments.service import PaymentService


def handle(request):
    return PaymentService().charge(request)


def health():
    return \"ok\"
";

/** Counter that keeps fixture directory names unique when tests run in parallel */
static FIXTURES: AtomicUsize = AtomicUsize::new(0);

/** Run git in a fixture repository and require success
 * Input
    - directory: &Path - repository root
    - args: &[&str] - git arguments
 * Output
    - None (panics if git fails)
*/
fn git(directory: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/** Run the crane binary in a fixture repository, optionally writing a hook payload to stdin
 * Input
    - directory: &Path - repository root
    - args: &[&str] - crane arguments
    - stdin: &str - text written to crane's stdin
 * Output
    - Output of the finished process
*/
fn crane(directory: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_crane"))
        .args(args)
        .current_dir(directory)
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

/** Write a file in the fixture repository
 * Input
    - directory: &Path - repository root
    - path: &str - repository-relative path
    - content: &str - file contents
 * Output
    - None (panics on failure)
*/
fn write(directory: &Path, path: &str, content: &str) {
    fs::write(directory.join(path), content).unwrap();
}

/** Create a committed fixture repository with a baseline checkpoint and the given policy body, by
 * writing a service with a flow handle -> charge -> compute_fee / compute_tax, a second file in
 * the same folder, a caller in another folder, and a README
 * Input
    - rules: &str - rule statements placed inside the policy block
 * Output
    - PathBuf of the repository root
*/
fn setup(rules: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let index = FIXTURES.fetch_add(1, Ordering::SeqCst);
    let directory = std::env::temp_dir().join(format!("crane-target-{suffix}-{index}"));
    fs::create_dir_all(directory.join("src").join("payments")).unwrap();
    fs::create_dir_all(directory.join("src").join("api")).unwrap();
    write(&directory, "README.md", "readme\n");
    write(&directory, "src/payments/service.py", SERVICE);
    write(&directory, "src/payments/other.py", OTHER);
    write(&directory, "src/api/handler.py", HANDLER);
    git(&directory, &["init", "-q"]);
    git(&directory, &["config", "user.email", "crane@example.com"]);
    git(&directory, &["config", "user.name", "Crane Tests"]);
    git(&directory, &["add", "."]);
    git(&directory, &["commit", "-qm", "trusted baseline"]);
    assert!(crane(&directory, &["init"], "").status.success());
    assert!(crane(&directory, &["checkpoint", "--name", "baseline"], "")
        .status
        .success());
    write(
        &directory,
        ".crane/policies/p.crane",
        &format!("policy p {{\n    checkpoint baseline;\n    {rules}\n}}\n"),
    );
    directory
}

/** Run crane check --json and return whether it passed together with its report
 * Input
    - directory: &Path - repository root
 * Output
    - (bool, String) pass flag and JSON report
*/
fn check(directory: &Path) -> (bool, String) {
    let output = crane(directory, &["check", "--json"], "");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/** Assert a check outcome: a pass, or a failure with the given violation type
 * Input
    - directory: &Path - repository root
    - expected: Option<&str> - None to expect a pass, or the expected violation type
    - case: &str - case name for failure messages
 * Output
    - None (panics on mismatch)
*/
fn assert_outcome(directory: &Path, expected: Option<&str>, case: &str) {
    let (passed, report) = check(directory);
    match expected {
        None => assert!(passed, "{case}: expected pass\n{report}"),
        Some(violation_type) => {
            assert!(!passed, "{case}: expected {violation_type}\n{report}");
            assert!(
                report.contains(&format!("\"violation_type\":\"{violation_type}\"")),
                "{case}: expected {violation_type}\n{report}"
            );
        }
    }
}

type Edit = fn(&Path);

/** Edit the service file by replacing text, requiring the text to be present
 * Input
    - directory: &Path - repository root
    - from: &str - text to replace
    - to: &str - replacement
 * Output
    - None (panics if the text is missing)
*/
fn edit_service(directory: &Path, from: &str, to: &str) {
    assert!(SERVICE.contains(from), "fixture lacks {from:?}");
    write(
        directory,
        "src/payments/service.py",
        &SERVICE.replace(from, to),
    );
}

/** Test every change type against every kind of edit to the target function (block scope), by
 * applying each edit to a fresh fixture and asserting pass or the expected violation type
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn change_types_classify_each_kind_of_edit() {
    let unchanged: Edit = |_| {};
    let rename: Edit = |dir| edit_service(dir, "total", "subtotal");
    let comment: Edit = |dir| {
        edit_service(
            dir,
            "        fee = compute_fee",
            "        # fee first\n        fee = compute_fee",
        )
    };
    let reformat: Edit = |dir| edit_service(dir, "total = total + item", "total = total+item");
    let swap: Edit = |dir| {
        edit_service(
            dir,
            "        fee = compute_fee(items)\n        tax = compute_tax(items)",
            "        tax = compute_tax(items)\n        fee = compute_fee(items)",
        )
    };
    let literal: Edit = |dir| edit_service(dir, "tax * 2", "tax * 3");
    let operator: Edit = |dir| edit_service(dir, "total + item", "total - item");
    let loop_removed: Edit = |dir| {
        edit_service(
            dir,
            "        total = 0\n        for item in items:\n            total = total + item\n",
            "        total = sum(items)\n",
        )
    };

    const PASS: Option<&str> = None;
    const UNCHANGED: Option<&str> = Some("target_unchanged");
    const MISMATCH: Option<&str> = Some("change_type_mismatch");
    // (edit name, edit, [any, logical_bn, logical_cn, logical_sn, semantic])
    let cases: &[(&str, Edit, [Option<&str>; 5])] = &[
        ("unchanged", unchanged, [UNCHANGED; 5]),
        ("rename", rename, [PASS, MISMATCH, MISMATCH, MISMATCH, PASS]),
        (
            "comment",
            comment,
            [PASS, MISMATCH, MISMATCH, MISMATCH, PASS],
        ),
        (
            "reformat",
            reformat,
            [PASS, MISMATCH, MISMATCH, MISMATCH, PASS],
        ),
        ("swap", swap, [PASS, MISMATCH, MISMATCH, PASS, MISMATCH]),
        (
            "literal",
            literal,
            [PASS, PASS, MISMATCH, MISMATCH, MISMATCH],
        ),
        (
            "operator",
            operator,
            [PASS, PASS, MISMATCH, MISMATCH, MISMATCH],
        ),
        (
            "loop removed",
            loop_removed,
            [PASS, PASS, PASS, MISMATCH, MISMATCH],
        ),
    ];
    let change_types = ["", "logical_bn", "logical_cn", "logical_sn", "semantic"];
    for (name, edit, expected) in cases {
        for (change_type, expected) in change_types.iter().zip(expected) {
            let suffix = if change_type.is_empty() {
                String::new()
            } else {
                format!(" change_type {change_type}")
            };
            let directory = setup(&format!("target --function PaymentService.charge{suffix};"));
            edit(&directory);
            assert_outcome(
                &directory,
                *expected,
                &format!(
                    "{name} / {}",
                    if suffix.is_empty() {
                        "any"
                    } else {
                        change_type
                    }
                ),
            );
            fs::remove_dir_all(&directory).unwrap();
        }
    }
}

/** Test where each scope looks for the required change, by asserting that edits inside the scope
 * satisfy the target and edits outside it leave the target unchanged
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn scopes_bound_where_the_change_must_happen() {
    let same_file: Edit = |dir| edit_service(dir, "return 3", "return 4");
    let callee: Edit = |dir| edit_service(dir, "return len(items)", "return len(items) + 1");
    let caller: Edit = |dir| {
        write(
            dir,
            "src/api/handler.py",
            &HANDLER.replace(".charge(request)", ".charge(request * 2)"),
        )
    };
    let caller_sibling: Edit = |dir| {
        write(
            dir,
            "src/api/handler.py",
            &HANDLER.replace("\"ok\"", "\"up\""),
        )
    };
    let same_folder: Edit = |dir| write(dir, "src/payments/other.py", &OTHER.replace('2', "5"));
    let new_in_folder: Edit = |dir| write(dir, "src/payments/new.py", "x = 1\n");
    let readme: Edit = |dir| write(dir, "README.md", "changed readme\n");

    const PASS: Option<&str> = None;
    const UNCHANGED: Option<&str> = Some("target_unchanged");
    let cases: &[(&str, &str, Edit, Option<&str>)] = &[
        ("block", "same file, other function", same_file, UNCHANGED),
        ("file", "same file, other function", same_file, PASS),
        ("file", "same folder, other file", same_folder, UNCHANGED),
        ("flow", "downstream callee", callee, PASS),
        ("flow", "upstream caller", caller, PASS),
        (
            "flow",
            "caller's sibling outside the flow",
            caller_sibling,
            UNCHANGED,
        ),
        ("flow", "same file, outside the flow", same_file, UNCHANGED),
        ("folder", "same folder, other file", same_folder, PASS),
        ("folder", "file added to folder", new_in_folder, PASS),
        ("folder", "other folder", caller, UNCHANGED),
        ("all", "readme", readme, PASS),
        ("all", "other folder", caller_sibling, PASS),
    ];
    for (scope, case, edit, expected) in cases {
        let directory = setup(&format!(
            "target --function PaymentService.charge scope {scope};"
        ));
        edit(&directory);
        assert_outcome(&directory, *expected, &format!("{scope}: {case}"));
        fs::remove_dir_all(&directory).unwrap();
    }

    // Wider scopes still classify: a README rewrite is a wording change only
    let directory =
        setup("target --function PaymentService.charge scope all change_type semantic;");
    readme(&directory);
    assert_outcome(&directory, None, "all: readme as semantic");
    fs::remove_dir_all(&directory).unwrap();

    let directory =
        setup("target --function PaymentService.charge scope all change_type logical_bn;");
    readme(&directory);
    assert_outcome(
        &directory,
        Some("change_type_mismatch"),
        "all: readme as logical_bn",
    );
    fs::remove_dir_all(&directory).unwrap();

    let directory =
        setup("target --function PaymentService.charge scope flow change_type logical_bn;");
    callee(&directory);
    assert_outcome(&directory, None, "flow: callee literal as logical_bn");
    fs::remove_dir_all(&directory).unwrap();
}

/** Test failures that stop a target from being checked at all, by asserting that deleting or
 * renaming the target is the agent's to repair, a target missing from the checkpoint is the
 * human's, and a target works alongside preserve in the same policy
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn target_sad_paths() {
    let directory = setup("target --function PaymentService.charge;");
    edit_service(&directory, "def charge(", "def bill(");
    let (passed, report) = check(&directory);
    assert!(!passed, "{report}");
    assert!(
        report.contains("\"violation_type\":\"target_not_found\""),
        "{report}"
    );
    assert!(report.contains("\"repair_owner\":\"agent\""), "{report}");
    fs::remove_dir_all(&directory).unwrap();

    let directory = setup("target --function PaymentService.refund;");
    let (passed, report) = check(&directory);
    assert!(!passed, "{report}");
    assert!(
        report.contains("\"violation_type\":\"target_not_found\""),
        "{report}"
    );
    assert!(report.contains("\"repair_owner\":\"human\""), "{report}");
    fs::remove_dir_all(&directory).unwrap();

    let directory = setup("target --function PaymentService.charge change_type Loigcal_bn;");
    let (passed, report) = check(&directory);
    assert!(!passed, "{report}");
    assert!(
        report.contains("\"violation_type\":\"malformed_policy\""),
        "{report}"
    );
    fs::remove_dir_all(&directory).unwrap();

    // preserve and target together: the target function must change, the callee must not
    let rules = "target --function PaymentService.charge;\n    preserve --function compute_fee;";
    let directory = setup(rules);
    edit_service(&directory, "tax * 2", "tax * 3");
    assert_outcome(&directory, None, "target met, preserve kept");
    write(
        &directory,
        "src/payments/service.py",
        &SERVICE
            .replace("tax * 2", "tax * 3")
            .replace("return len(items)", "return 0"),
    );
    let (passed, report) = check(&directory);
    assert!(!passed, "{report}");
    assert!(
        report.contains("\"violation_type\":\"source_changed\""),
        "{report}"
    );
    assert!(
        !report.contains("target_unchanged"),
        "target still met: {report}"
    );
    fs::remove_dir_all(&directory).unwrap();
}

/** Run one Claude Code hook event and return its exit code and stdout
 * Input
    - directory: &Path - repository root
    - event: &str - hook event name
    - stdin: &str - hook payload
 * Output
    - (Option<i32>, String) exit code and stdout
*/
fn hook(directory: &Path, event: &str, stdin: &str) -> (Option<i32>, String) {
    let output = crane(
        directory,
        &["agent", "hook", "--event", event, "--profile", "claude"],
        stdin,
    );
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/** Test how hooks treat pending targets, by asserting that an unmet target never blocks the
 * user's prompt or edits, blocks at stop, lets a forced-retry stop through with a warning, and
 * stops blocking once met; while a preserve violation still blocks a forced-retry stop
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn hooks_enforce_targets_only_at_stop() {
    let directory = setup("target --function PaymentService.charge change_type logical_bn;");

    for event in ["user-prompt-submit", "post-tool-use"] {
        let (code, stdout) = hook(&directory, event, "{}");
        assert_eq!(code, Some(0), "{event}: {stdout}");
        let payload: serde_json::Value = serde_json::from_str(&stdout).expect("hook JSON");
        assert!(
            payload["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("Crane target rules require these changes"),
            "{event}: {stdout}"
        );
    }

    let (code, _) = hook(&directory, "stop", "{\"stop_hook_active\":false}");
    assert_eq!(code, Some(2), "stop must block while the target is unmet");

    let (code, stdout) = hook(&directory, "stop", "{\"stop_hook_active\":true}");
    assert_eq!(code, Some(0), "a forced retry must not loop: {stdout}");
    assert!(
        stdout.contains("Crane targets not yet satisfied"),
        "{stdout}"
    );

    let context = crane(&directory, &["context"], "");
    let context = String::from_utf8_lossy(&context.stdout);
    assert!(
        context.contains(
            "rule: target\nkind: function\ntarget: PaymentService.charge\nscope: block\nchange_type: logical_bn"
        ),
        "{context}"
    );

    edit_service(&directory, "tax * 2", "tax * 3");
    let (code, _) = hook(&directory, "stop", "{}");
    assert_eq!(code, Some(0), "stop passes once the target is met");
    fs::remove_dir_all(&directory).unwrap();

    let directory = setup("preserve --function PaymentService.charge;");
    edit_service(&directory, "tax * 2", "tax * 3");
    let (code, _) = hook(&directory, "stop", "{\"stop_hook_active\":true}");
    assert_eq!(
        code,
        Some(2),
        "preserve violations still block a forced retry"
    );
    fs::remove_dir_all(&directory).unwrap();
}

/** Read a policy file written by a crane command
 * Input
    - directory: &Path - repository root
    - name: &str - policy name
 * Output
    - String policy text
*/
fn policy(directory: &Path, name: &str) -> String {
    fs::read_to_string(
        directory
            .join(".crane")
            .join("policies")
            .join(format!("{name}.crane")),
    )
    .unwrap()
}

/** Test the happy paths of crane target, by creating rules with policy-style options, flag-style
 * options, and defaults, asserting the written policy text, and checking that a created rule
 * fails until the target is changed in the required way
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn target_command_writes_rules() {
    let directory = setup("preserve --function compute_tax;");
    fs::remove_file(directory.join(".crane/policies/p.crane")).unwrap();

    let output = crane(
        &directory,
        &[
            "target",
            "--function",
            "PaymentService.charge",
            "--policy",
            "fix_charge",
            "scope",
            "flow",
            "change_type",
            "logical_bn",
        ],
        "",
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("src/payments/service.py"), "{stdout}");
    assert_eq!(
        policy(&directory, "fix_charge"),
        "policy fix_charge {\n    checkpoint baseline;\n    target --function PaymentService.charge scope flow change_type logical_bn;\n}\n"
    );
    assert_outcome(
        &directory,
        Some("target_unchanged"),
        "created rule before the change",
    );
    edit_service(&directory, "tax * 2", "tax * 3");
    assert_outcome(&directory, None, "created rule after a business change");
    fs::remove_file(directory.join(".crane/policies/fix_charge.crane")).unwrap();

    let output = crane(
        &directory,
        &[
            "target",
            "--function",
            "compute_fee",
            "--policy",
            "flags",
            "--scope",
            "file",
            "--change-type",
            "Semantic",
        ],
        "",
    );
    assert!(output.status.success(), "{output:?}");
    assert!(policy(&directory, "flags")
        .contains("target --function compute_fee scope file change_type semantic;"));

    let output = crane(&directory, &["target", "--function", "compute_tax"], "");
    assert!(output.status.success(), "{output:?}");
    assert!(policy(&directory, "target_compute_tax")
        .contains("target --function compute_tax scope block;"));
    fs::remove_dir_all(&directory).unwrap();
}

/** Test the sad paths of crane target, by asserting the error for every invalid invocation and
 * that no policy file is written when the command fails
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn target_command_rejects_invalid_input() {
    let directory = setup("preserve --function compute_tax;");
    let cases: &[(&[&str], &str)] = &[
        (&["target"], "target requires exactly one of --function, --data, --variable, --class, or --interface"),
        (&["target", "--function", "A..b"], "invalid function target"),
        (
            &[
                "target",
                "--function",
                "PaymentService.charge",
                "change_type",
                "Loigcal_bn",
            ],
            "invalid change_type 'Loigcal_bn'",
        ),
        (
            &[
                "target",
                "--function",
                "PaymentService.charge",
                "change_type",
            ],
            "change_type requires a value",
        ),
        (
            &[
                "target",
                "--function",
                "PaymentService.charge",
                "--scope",
                "module",
            ],
            "invalid scope 'module'",
        ),
        (
            &["target", "--function", "PaymentService.refund"],
            "protected function PaymentService.refund is missing from checkpoint",
        ),
        (
            &[
                "target",
                "--function",
                "PaymentService.charge",
                "--checkpoint",
                "missing",
            ],
            "checkpoint 'missing' does not exist",
        ),
        (
            &[
                "target",
                "--function",
                "PaymentService.charge",
                "--policy",
                "../escape",
            ],
            "invalid identifier '../escape'",
        ),
    ];
    for (args, expected) in cases {
        let output = crane(&directory, args, "");
        assert!(!output.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
    let written = fs::read_dir(directory.join(".crane/policies"))
        .unwrap()
        .count();
    assert_eq!(written, 1, "failed commands must not write policies");
    fs::remove_dir_all(&directory).unwrap();
}
