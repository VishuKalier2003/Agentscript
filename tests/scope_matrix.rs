use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const SERVICE: &str = "class PaymentService:
    def charge(self, amount):
        return validate(amount)


def validate(amount):
    return normalize(amount) > 0


def normalize(amount):
    return amount


def unrelated():
    return 1
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

/** Run the crane binary in a fixture repository
 * Input
    - directory: &Path - repository root
    - args: &[&str] - crane arguments
 * Output
    - Output of the finished process
*/
fn crane(directory: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crane"))
        .args(args)
        .current_dir(directory)
        .output()
        .expect("crane should execute")
}

/** Create a committed fixture repository with a baseline checkpoint and one preserve policy, by
 * writing a call chain handle -> charge -> validate -> normalize across two folders plus
 * unrelated code, committing it, and protecting PaymentService.charge at the given scope
 * Input
    - scope: &str - scope keyword for the policy
 * Output
    - PathBuf of the repository root
*/
fn setup(scope: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("crane-scope-{scope}-{suffix}"));
    fs::create_dir_all(directory.join("src").join("payments")).unwrap();
    fs::create_dir_all(directory.join("src").join("api")).unwrap();
    fs::write(directory.join("README.md"), "readme\n").unwrap();
    write(&directory, "src/payments/service.py", SERVICE);
    write(&directory, "src/payments/other.py", OTHER);
    write(&directory, "src/api/handler.py", HANDLER);
    git(&directory, &["init", "-q"]);
    git(&directory, &["config", "user.email", "crane@example.com"]);
    git(&directory, &["config", "user.name", "Crane Tests"]);
    git(&directory, &["add", "."]);
    git(&directory, &["commit", "-qm", "trusted baseline"]);
    assert!(crane(&directory, &["init"]).status.success());
    assert!(crane(&directory, &["checkpoint", "--name", "baseline"])
        .status
        .success());
    fs::write(
        directory.join(".crane").join("policies").join("p.crane"),
        format!(
            "policy p {{\n    checkpoint baseline;\n    preserve --function PaymentService.charge scope {scope};\n}}\n"
        ),
    )
    .unwrap();
    directory
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

/** Run crane check --json and assert whether it passed
 * Input
    - directory: &Path - repository root
    - passed: bool - expected outcome
    - case: &str - case name for failure messages
 * Output
    - None (panics on mismatch)
*/
fn assert_check(directory: &Path, passed: bool, case: &str) {
    let output = crane(directory, &["check", "--json"]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.success(), passed, "{case}: {text}");
    if !passed {
        assert!(
            text.contains("\"violation_type\":\"source_changed\""),
            "{case}: {text}"
        );
    }
}

type Change = fn(&Path);

/** Test every scope against the same set of edits, by applying each edit to a fresh fixture and
 * asserting whether the scope protects the edited code
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn scopes_protect_the_expected_code() {
    let edit_charge: Change = |dir| {
        write(
            dir,
            "src/payments/service.py",
            &SERVICE.replace("return validate", "return  validate"),
        )
    };
    let edit_unrelated: Change = |dir| {
        write(
            dir,
            "src/payments/service.py",
            &SERVICE.replace("return 1", "return 2"),
        )
    };
    let edit_callee: Change = |dir| {
        write(
            dir,
            "src/payments/service.py",
            &SERVICE.replace("return amount", "return -amount"),
        )
    };
    let comment_callee: Change = |dir| {
        write(
            dir,
            "src/payments/service.py",
            &SERVICE.replace("return amount", "return amount  # note"),
        )
    };
    let edit_caller: Change = |dir| {
        write(
            dir,
            "src/api/handler.py",
            &HANDLER.replace(".charge(request)", ".charge(request * 2)"),
        )
    };
    let edit_caller_sibling: Change = |dir| {
        write(
            dir,
            "src/api/handler.py",
            &HANDLER.replace("\"ok\"", "\"up\""),
        )
    };
    let edit_same_folder: Change =
        |dir| write(dir, "src/payments/other.py", &OTHER.replace('2', "3"));
    let add_to_folder: Change = |dir| write(dir, "src/payments/new.py", "x = 1\n");
    let edit_readme: Change = |dir| write(dir, "README.md", "changed\n");

    // (scope, case, change, expected pass)
    let cases: &[(&str, &str, Change, bool)] = &[
        ("block", "target edited", edit_charge, false),
        ("block", "same file edited", edit_unrelated, true),
        ("block", "callee edited", edit_callee, true),
        ("file", "same file edited", edit_unrelated, false),
        ("file", "same folder edited", edit_same_folder, true),
        ("file", "comment added", comment_callee, true),
        ("flow", "transitive callee edited", edit_callee, false),
        ("flow", "caller edited", edit_caller, false),
        ("flow", "comment added in flow", comment_callee, true),
        (
            "flow",
            "function outside flow in same file",
            edit_unrelated,
            true,
        ),
        ("flow", "caller's sibling edited", edit_caller_sibling, true),
        ("folder", "same folder edited", edit_same_folder, false),
        ("folder", "file added to folder", add_to_folder, false),
        ("folder", "other folder edited", edit_caller, true),
        ("folder", "readme edited", edit_readme, true),
        ("all", "readme edited", edit_readme, false),
        ("all", "other folder edited", edit_caller_sibling, false),
        ("all", "comment added", comment_callee, true),
    ];
    for (scope, case, change, passed) in cases {
        let directory = setup(scope);
        assert_check(&directory, true, &format!("{scope}: unchanged"));
        change(&directory);
        assert_check(&directory, *passed, &format!("{scope}: {case}"));
        fs::remove_dir_all(&directory).unwrap();
    }
}

/** Test that crane protect accepts a trailing scope and writes the ';'-terminated syntax, by
 * protecting with "scope flow" and checking both the policy text and a passing check
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn protect_writes_scope_and_semicolons() {
    let directory = setup("block");
    fs::remove_file(directory.join(".crane").join("policies").join("p.crane")).unwrap();
    let output = crane(
        &directory,
        &[
            "protect",
            "--function",
            "PaymentService.charge",
            "--policy",
            "payment",
            "scope",
            "flow",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let policy = fs::read_to_string(
        directory
            .join(".crane")
            .join("policies")
            .join("payment.crane"),
    )
    .unwrap();
    assert_eq!(
        policy,
        "policy payment {\n    checkpoint baseline;\n    preserve --function PaymentService.charge scope flow;\n}\n"
    );
    assert_check(&directory, true, "protect flow");

    let invalid = crane(
        &directory,
        &[
            "protect",
            "--function",
            "PaymentService.charge",
            "scope",
            "module",
        ],
    );
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid scope 'module'"));

    // The flag spelling works too, and block scope is the default
    let output = crane(
        &directory,
        &[
            "protect",
            "--function",
            "PaymentService.charge",
            "--policy",
            "folder_rule",
            "--scope",
            "folder",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let policy = fs::read_to_string(
        directory
            .join(".crane")
            .join("policies")
            .join("folder_rule.crane"),
    )
    .unwrap();
    assert!(policy.contains("preserve --function PaymentService.charge scope folder;"));

    let output = crane(
        &directory,
        &["protect", "--function", "PaymentService.charge"],
    );
    assert!(output.status.success(), "{output:?}");
    let policy = fs::read_to_string(
        directory
            .join(".crane")
            .join("policies")
            .join("preserve_paymentservice_charge.crane"),
    )
    .unwrap();
    assert!(policy.contains("preserve --function PaymentService.charge;"));

    // Sad paths: a scope keyword without a value, and names that would escape .crane
    for (args, expected) in [
        (
            vec!["protect", "--function", "PaymentService.charge", "scope"],
            "scope requires a value",
        ),
        (
            vec![
                "protect",
                "--function",
                "PaymentService.charge",
                "--policy",
                "../escape",
            ],
            "invalid identifier '../escape'",
        ),
        (
            vec![
                "protect",
                "--function",
                "PaymentService.charge",
                "--checkpoint",
                "../baseline",
            ],
            "invalid identifier '../baseline'",
        ),
    ] {
        let output = crane(&directory, &args);
        assert!(!output.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
    fs::remove_dir_all(&directory).unwrap();
}

/** Run crane check --json and return whether it passed together with its stdout
 * Input
    - directory: &Path - repository root
 * Output
    - (bool, String) pass flag and JSON report
*/
fn check_json(directory: &Path) -> (bool, String) {
    let output = crane(directory, &["check", "--json"]);
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/** Test that edits Git's own change detection would miss are still caught, by hiding an edit
 * behind assume-unchanged and skip-worktree flags, and by editing a file without changing its size
 * and then restoring its modification time
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn hidden_edits_are_detected() {
    for scope in ["folder", "all"] {
        for flag in ["--assume-unchanged", "--skip-worktree"] {
            let directory = setup(scope);
            git(&directory, &["update-index", flag, "src/payments/other.py"]);
            write(
                &directory,
                "src/payments/other.py",
                &OTHER.replace('2', "3"),
            );
            assert_check(&directory, false, &format!("{scope}: {flag}"));
            fs::remove_dir_all(&directory).unwrap();
        }

        let directory = setup(scope);
        let path = directory.join("src/payments/other.py");
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        write(
            &directory,
            "src/payments/other.py",
            &OTHER.replace('2', "3"),
        );
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_check(&directory, false, &format!("{scope}: same size and mtime"));
        fs::remove_dir_all(&directory).unwrap();
    }
}

/** Test deletions and line endings in folder scope, by asserting that deleting a tracked file
 * fails while rewriting a file with CRLF line endings passes
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn folder_scope_handles_deletion_and_line_endings() {
    let directory = setup("folder");
    fs::remove_file(directory.join("src/payments/other.py")).unwrap();
    assert_check(&directory, false, "folder: file deleted");
    fs::remove_dir_all(&directory).unwrap();

    let directory = setup("folder");
    write(
        &directory,
        "src/payments/other.py",
        &OTHER.replace('\n', "\r\n"),
    );
    assert_check(&directory, true, "folder: CRLF rewrite");
    fs::remove_dir_all(&directory).unwrap();
}

/** Test which unparsable files affect flow scope, by asserting that a broken file unrelated to the
 * flow is ignored while a broken file that calls the traced function fails with parse_failure
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn flow_scope_parses_only_files_in_the_flow() {
    let directory = setup("flow");
    write(&directory, "src/api/broken.py", "def broken(:\n    pass\n");
    assert_check(&directory, true, "flow: unrelated file does not parse");

    write(
        &directory,
        "src/api/broken.py",
        "def broken(:\n    return PaymentService().charge(1)\n",
    );
    let (passed, text) = check_json(&directory);
    assert!(!passed, "{text}");
    assert!(
        text.contains("\"violation_type\":\"parse_failure\""),
        "{text}"
    );
    fs::remove_dir_all(&directory).unwrap();
}
