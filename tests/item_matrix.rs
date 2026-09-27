use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SERVICE: &str = "interface Payable {
    int pay(int amount);
}

class PaymentService implements Payable {
    private static final int RATE = 5;

    public int pay(int amount) {
        return amount * RATE;
    }

    public int refund(int amount) {
        return amount;
    }
}
";

/** Counter that keeps fixture directory names unique when tests run in parallel */
static FIXTURES: AtomicUsize = AtomicUsize::new(0);

/** Run a command in a directory and return its output
 * Input
    - program: &str - executable to run
    - directory: &Path - working directory
    - args: &[&str] - arguments
 * Output
    - Output of the finished process
*/
fn run(program: &str, directory: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(directory)
        .output()
        .expect("command should execute")
}

/** Create a committed Java fixture with an interface, a class, a constant, and two methods, a
 * baseline checkpoint, and a policy with the given rule
 * Input
    - rule: &str - one rule statement including its ';', or empty for no policy
 * Output
    - PathBuf of the repository root
*/
fn setup(rule: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let index = FIXTURES.fetch_add(1, Ordering::SeqCst);
    let directory = std::env::temp_dir().join(format!("crane-items-{suffix}-{index}"));
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("PaymentService.java"), SERVICE).unwrap();
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "crane@example.com"],
        &["config", "user.name", "Crane Tests"],
        &["add", "."],
        &["commit", "-qm", "trusted baseline"],
    ] {
        assert!(run("git", &directory, args).status.success());
    }
    let crane = env!("CARGO_BIN_EXE_crane");
    assert!(run(crane, &directory, &["init"]).status.success());
    assert!(
        run(crane, &directory, &["checkpoint", "--name", "baseline"])
            .status
            .success()
    );
    if !rule.is_empty() {
        fs::write(
            directory.join(".crane/policies/p.crane"),
            format!("policy p {{\n    checkpoint baseline;\n    {rule}\n}}\n"),
        )
        .unwrap();
    }
    directory
}

/** Replace text in the fixture's Java file, requiring the text to be present
 * Input
    - directory: &Path - repository root
    - from: &str - text to replace
    - to: &str - replacement
 * Output
    - None (panics if the text is missing)
*/
fn edit(directory: &Path, from: &str, to: &str) {
    assert!(SERVICE.contains(from), "fixture lacks {from:?}");
    fs::write(
        directory.join("PaymentService.java"),
        SERVICE.replace(from, to),
    )
    .unwrap();
}

/** Run crane check --json and assert a pass or a failure with the given violation type
 * Input
    - directory: &Path - repository root
    - expected: Option<&str> - None to expect a pass, or the expected violation type
    - case: &str - case name for failure messages
 * Output
    - None (panics on mismatch)
*/
fn assert_outcome(directory: &Path, expected: Option<&str>, case: &str) {
    let output = run(env!("CARGO_BIN_EXE_crane"), directory, &["check", "--json"]);
    let report = String::from_utf8_lossy(&output.stdout);
    match expected {
        None => assert!(output.status.success(), "{case}: expected pass\n{report}"),
        Some(violation_type) => {
            assert!(
                !output.status.success(),
                "{case}: expected {violation_type}\n{report}"
            );
            assert!(
                report.contains(&format!("\"violation_type\":\"{violation_type}\"")),
                "{case}: expected {violation_type}\n{report}"
            );
        }
    }
}

/** Test preserve and target rules for every item kind, by applying edits to a fresh fixture and
 * asserting which ones each rule accepts; in particular a type change breaks --variable but not
 * --data, and a value change breaks both
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn item_kinds_protect_and_target_the_right_code() {
    const PASS: Option<&str> = None;
    const CHANGED: Option<&str> = Some("source_changed");
    const UNCHANGED: Option<&str> = Some("target_unchanged");
    const MISMATCH: Option<&str> = Some("change_type_mismatch");
    let unchanged = ("", "");
    let refund_body = ("return amount;", "return amount - 1;");
    let refund_comment = ("return amount;", "return amount; // full refund");
    // A rename is semantic when it is consistent across the whole unit (here, the class)
    let refund_rename = ("amount", "value");
    let interface_method = (
        "int pay(int amount);",
        "int pay(int amount);\n    int fee();",
    );
    let rate_type = ("static final int RATE", "static final long RATE");
    let rate_value = ("RATE = 5;", "RATE = 6;");
    let pay_body = ("return amount * RATE;", "return amount * RATE + 1;");

    let cases: &[(&str, (&str, &str), Option<&str>)] = &[
        ("preserve --class PaymentService;", refund_body, CHANGED),
        ("preserve --class PaymentService;", refund_comment, PASS),
        ("preserve --class PaymentService;", interface_method, PASS),
        ("preserve --interface Payable;", interface_method, CHANGED),
        ("preserve --interface Payable;", refund_body, PASS),
        (
            "preserve --variable PaymentService.RATE;",
            rate_type,
            CHANGED,
        ),
        (
            "preserve --variable PaymentService.RATE;",
            rate_value,
            CHANGED,
        ),
        ("preserve --variable PaymentService.RATE;", pay_body, PASS),
        ("preserve --data PaymentService.RATE;", rate_type, PASS),
        ("preserve --data PaymentService.RATE;", rate_value, CHANGED),
        ("preserve --function PaymentService.pay;", refund_body, PASS),
        // A variable's flow is the functions that read it
        (
            "preserve --variable PaymentService.RATE scope flow;",
            pay_body,
            CHANGED,
        ),
        (
            "preserve --variable PaymentService.RATE scope flow;",
            refund_body,
            PASS,
        ),
        (
            "preserve --class PaymentService scope file;",
            interface_method,
            CHANGED,
        ),
        (
            "target --data PaymentService.RATE change_type logical_bn;",
            unchanged,
            UNCHANGED,
        ),
        (
            "target --data PaymentService.RATE change_type logical_bn;",
            rate_value,
            PASS,
        ),
        ("target --data PaymentService.RATE;", rate_type, UNCHANGED),
        ("target --variable PaymentService.RATE;", rate_type, PASS),
        (
            "target --class PaymentService change_type semantic;",
            refund_rename,
            PASS,
        ),
        (
            "target --class PaymentService change_type semantic;",
            refund_body,
            MISMATCH,
        ),
        ("target --interface Payable;", unchanged, UNCHANGED),
        (
            // A new method signature adds no literals, operators, calls, or loops: structure only
            "target --interface Payable change_type logical_sn;",
            interface_method,
            PASS,
        ),
        (
            "target --interface Payable change_type semantic;",
            interface_method,
            MISMATCH,
        ),
        ("target --interface Payable;", interface_method, PASS),
    ];
    for (rule, (from, to), expected) in cases {
        let directory = setup(rule);
        if !from.is_empty() {
            edit(&directory, from, to);
        }
        assert_outcome(
            &directory,
            *expected,
            &format!("{rule} / {from:?} -> {to:?}"),
        );
        fs::remove_dir_all(&directory).unwrap();
    }
}

/** Test that items missing from the checkpoint, or deleted from the worktree, fail closed with
 * target_not_found naming the item kind
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn missing_items_fail_closed() {
    let directory = setup("preserve --interface Refundable;");
    let output = run(
        env!("CARGO_BIN_EXE_crane"),
        &directory,
        &["check", "--json"],
    );
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(
        report.contains("protected interface Refundable is missing from checkpoint"),
        "{report}"
    );
    assert!(
        report.contains("\"violation_type\":\"target_not_found\""),
        "{report}"
    );
    fs::remove_dir_all(&directory).unwrap();

    let directory = setup("preserve --variable PaymentService.RATE;");
    edit(&directory, "    private static final int RATE = 5;\n", "");
    assert_outcome(&directory, Some("target_not_found"), "deleted variable");
    fs::remove_dir_all(&directory).unwrap();
}

/** Test that crane protect and crane target accept every item flag with scope and change_type,
 * and reject a missing or doubled item flag
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn commands_accept_every_item_flag() {
    let directory = setup("");
    let crane = env!("CARGO_BIN_EXE_crane");
    let written = |name: &str| {
        fs::read_to_string(directory.join(format!(".crane/policies/{name}.crane"))).unwrap()
    };

    let output = run(
        crane,
        &directory,
        &[
            "protect",
            "--class",
            "PaymentService",
            "--policy",
            "keep",
            "scope",
            "file",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(written("keep").contains("preserve --class PaymentService scope file;"));

    let output = run(
        crane,
        &directory,
        &[
            "target",
            "--data",
            "PaymentService.RATE",
            "--policy",
            "raise",
            "--change-type",
            "logical_bn",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(written("raise")
        .contains("target --data PaymentService.RATE scope block change_type logical_bn;"));
    assert_outcome(&directory, Some("target_unchanged"), "created target");
    edit(&directory, "RATE = 5;", "RATE = 6;");
    // The class rule also covers RATE, so raising it satisfies the target and breaks the class
    assert_outcome(
        &directory,
        Some("source_changed"),
        "target met, class changed",
    );

    for (args, expected) in [
        (
            vec!["protect", "--policy", "x"],
            "exactly one of --function, --data",
        ),
        (
            vec![
                "target",
                "--class",
                "PaymentService",
                "--interface",
                "Payable",
            ],
            "exactly one of --function, --data",
        ),
        (
            vec!["target", "--interface", "Refundable"],
            "protected interface Refundable is missing from checkpoint",
        ),
    ] {
        let output = run(crane, &directory, &args);
        assert!(!output.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
    fs::remove_dir_all(&directory).unwrap();
}
