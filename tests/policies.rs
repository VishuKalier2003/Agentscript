// AgentScript policies: the language, validation, preserve and target semantics, moving commands
// between policies, policy contexts, and policy status.

mod common;

use common::{Fixture, PAYMENTS};

/** Syntax errors, commands outside a policy, and policy files outside .crane/policies fail
 * validation
 * Input
    - None
 * Output
    - None (panics on failure)
*/
#[test]
fn validate_checks_syntax_and_placement() {
    let fixture = Fixture::ready();
    assert!(fixture.ok(&["validate"]).contains("Crane validate: PASS"));
    let default = fixture.read(".crane/policies/default.crane");
    for (content, expected) in [
        ("preserve K7M2P9RX;\n", "inside a policy block"),
        ("Policy default {\n}\n", "lowercase"),
        ("policy default {\n", "not closed"),
        (
            "policy default {\n  target k7m2;\n}\n",
            "invalid selection marker",
        ),
    ] {
        fixture.write(".crane/policies/default.crane", content);
        let output = fixture.fails(&["validate"]);
        assert!(
            output.contains("policy_syntax") && output.contains(expected),
            "{content}: {output}"
        );
    }
    fixture.write(".crane/policies/default.crane", &default);
    fixture.write("rules/extra.crane", "policy extra {\n}\n");
    let outside = fixture.fails(&["validate"]);
    assert!(outside.contains("policy_outside_folder"), "{outside}");
    std::fs::remove_file(fixture.work.join("rules/extra.crane")).unwrap();
    fixture.write(".crane/policies/other.crane", "policy DEFAULT {\n}\n");
    let duplicate = fixture.fails(&["validate"]);
    assert!(duplicate.contains("policy_duplicate"), "{duplicate}");
}

/** Commands written by hand that are not in the signed registry, or that disagree with it, fail
 * Input
    - None
 * Output
    - None (panics on failure)
*/
#[test]
fn policy_commands_must_match_the_registry() {
    let fixture = Fixture::ready();
    let id = fixture.protect(&["app/ledger.py"]);
    let policy = fixture.read(".crane/policies/default.crane");
    fixture.write(
        ".crane/policies/default.crane",
        &policy.replace(&format!("preserve {id};"), &format!("target {id};")),
    );
    let mismatch = fixture.fails(&["validate"]);
    assert!(mismatch.contains("policy_registry_mismatch"), "{mismatch}");
    fixture.write(
        ".crane/policies/default.crane",
        &policy.replace(&format!("preserve {id};"), ""),
    );
    let removed = fixture.fails(&["validate"]);
    assert!(removed.contains("selection_unreferenced"), "{removed}");
    fixture.write(
        ".crane/policies/default.crane",
        &policy.replace(
            &format!("preserve {id};"),
            &format!("preserve {id};\n    preserve QQQQ1111;"),
        ),
    );
    let forged = fixture.fails(&["validate"]);
    assert!(forged.contains("policy_unregistered_selection"), "{forged}");
}

/** A policy passes only when every command passes
 * Input
    - None
 * Output
    - None (panics on failure)
*/
#[test]
fn one_failing_command_fails_the_policy() {
    let fixture = Fixture::ready();
    fixture.ok(&["create", "policy", "payments", "payments"]);
    let keep = fixture.protect(&[
        "app/payments.py",
        "policy",
        "payments",
        "start-line",
        "1",
        "end-line",
        "4",
    ]);
    let change = fixture.target(&[
        "app/payments.py",
        "policy",
        "payments",
        "start-line",
        "9",
        "end-line",
        "10",
    ]);
    let failing = fixture.fails(&["test", "."]);
    assert!(failing.contains("FAIL policy payments"), "{failing}");
    assert!(
        failing.contains(&format!("PASS preserve {keep}")),
        "{failing}"
    );
    assert!(
        failing.contains(&format!("FAIL target {change}")),
        "{failing}"
    );
    assert!(failing.contains("PASS policy default"), "{failing}");
    let text = fixture.read("app/payments.py");
    fixture.write(
        "app/payments.py",
        &text.replace(
            "    return -amount\n",
            "    if amount < 0:\n        raise ValueError(amount)\n    return -amount\n",
        ),
    );
    let passing = fixture.ok(&["test", "."]);
    assert!(passing.contains("PASS policy payments"), "{passing}");
    let (passed, report) = fixture.json(&["test", ".", "--json"]);
    assert!(passed);
    assert_eq!(report["policies"].as_array().unwrap().len(), 2);
}

/** Target semantics: whitespace-only and comment-only changes are insufficient, change types are
 * enforced, and changes outside the selection do not count
 * Input
    - None
 * Output
    - None (panics on failure)
*/
#[test]
fn target_changes_must_be_meaningful() {
    let fixture = Fixture::ready();
    let id = fixture.target(&[
        "app/payments.py",
        "start-line",
        "1",
        "end-line",
        "4",
        "change_type",
        "logical_bn",
    ]);
    assert!(fixture
        .read(".crane/policies/default.crane")
        .contains(&format!("target {id} change_type logical_bn;")));
    let base = fixture.read("app/payments.py");
    let check = |content: &str, passes: bool, expected: &str| {
        fixture.write("app/payments.py", content);
        let output = fixture.run(&["test", "."]);
        assert_eq!(
            output.status.success(),
            passes,
            "{expected}: {}",
            common::text(&output)
        );
        assert!(
            common::text(&output).contains(expected),
            "{expected}: {}",
            common::text(&output)
        );
    };
    check(&base, false, "has not changed yet");
    check(
        &base.replace("fee = amount * 0.03", "fee  =  amount * 0.03"),
        false,
        "only whitespace changed",
    );
    check(
        &base.replace("    fee = amount", "    # fee rate\n    fee = amount"),
        false,
        "is not logical_bn",
    );
    check(
        &base.replace("return -amount", "return -abs(amount)"),
        false,
        "has not changed yet",
    );
    check(
        &base.replace(
            "fee = amount * 0.03\n    total = amount + fee",
            "rate = amount * 0.03\n    total = amount + rate",
        ),
        false,
        "is not logical_bn",
    );
    check(
        &base.replace("0.03", "0.025"),
        true,
        "target changed as required",
    );
}

/** add policy moves a marker's command between policies, including back to default
 * Input
    - None
 * Output
    - None (panics on failure)
*/
#[test]
fn add_policy_moves_commands() {
    let fixture = Fixture::ready();
    let id = fixture.protect(&["app/ledger.py"]);
    fixture.ok(&["create", "policy", "ledger", "ledger"]);
    let moved = fixture.ok(&["add", "policy", "ledger", "marker", &id]);
    assert!(
        moved.contains("from policy default to policy ledger"),
        "{moved}"
    );
    assert!(!fixture.read(".crane/policies/default.crane").contains(&id));
    assert!(fixture
        .read(".crane/policies/ledger.crane")
        .contains(&format!("preserve {id};")));
    assert!(fixture.ok(&["validate"]).contains("PASS"));
    assert!(fixture
        .ok(&["add", "policy", "ledger", "marker", &id])
        .contains("already in policy"));
    fixture.ok(&["add", "policy", "default", "marker", &id]);
    assert!(fixture
        .read(".crane/policies/default.crane")
        .contains(&format!("preserve {id};")));
    assert!(!fixture.read(".crane/policies/ledger.crane").contains(&id));
    let unknown = fixture.fails(&["add", "policy", "default", "marker", "ZZZZZZZZ"]);
    assert!(unknown.contains("not a registered selection"), "{unknown}");
    let missing = fixture.fails(&["add", "policy", "nope", "marker", &id]);
    assert!(missing.contains("does not exist"), "{missing}");
}

/** Policy contexts bind text files to policies and are shown with --view; binary files and
 * missing policies are refused
 * Input
    - None
 * Output
    - None (panics on failure)
*/
#[test]
fn policy_contexts() {
    let fixture = Fixture::ready();
    fixture.ok(&["add", "policy-context", "file", "docs/guide.md"]);
    let view = fixture.ok(&["policy-context", "default", "--view"]);
    assert!(view.contains("== docs/guide.md (current)"), "{view}");
    assert!(view.contains("Use the payment API carefully."), "{view}");
    fixture.write("docs/guide.md", "# Guide\n\nChanged.\n");
    assert!(fixture
        .ok(&["policy-context", "default", "--view"])
        .contains("STALE"));
    fixture.write("docs/blob.txt", "a\u{0}b");
    let binary = fixture.fails(&["add", "policy-context", "file", "docs/blob.txt"]);
    assert!(binary.contains("not a text file"), "{binary}");
    let policy = fixture.fails(&[
        "add",
        "policy-context",
        "file",
        "docs/guide.md",
        "policy",
        "nope",
    ]);
    assert!(policy.contains("does not exist"), "{policy}");
    let path = fixture.fails(&["add", "policy-context", "file", "docs/missing.md"]);
    assert!(path.contains("no file named"), "{path}");
    let usage = fixture.fails(&["policy-context", "default"]);
    assert!(usage.contains("--view"), "{usage}");
}

/** policy status validates, tests, and simulates the hook boundary for a policy
 * Input
    - None
 * Output
    - None (panics on failure)
*/
#[test]
fn policy_status_exercises_the_boundary() {
    let fixture = Fixture::ready();
    let keep = fixture.protect(&["app/payments.py", "start-line", "1", "end-line", "4"]);
    let change = fixture.target(&["app/payments.py", "start-line", "9", "end-line", "10"]);
    fixture.write(
        "app/payments.py",
        &fixture
            .read("app/payments.py")
            .replace("return -amount", "return 0 - amount"),
    );
    let (_, report) = fixture.json(&["policy", "default", "status", "--json"]);
    let checks = report["checks"].as_array().unwrap();
    let status = |name: &str| {
        checks
            .iter()
            .find(|check| check["check"].as_str().unwrap().contains(name))
            .unwrap_or_else(|| panic!("check {name} in {report}"))["status"]
            .clone()
    };
    assert_eq!(status(&format!("pre-write preserve {keep}")), "PASS");
    assert_eq!(status(&format!("pre-write target {change}")), "PASS");
    assert_eq!(status(&format!("marker removal {keep}")), "PASS");
    assert_eq!(status("write .crane/map"), "PASS");
    assert_eq!(status("agent runs crane protect"), "PASS");
    assert_eq!(status("ci/cd"), "WARN");
    assert_eq!(status("hooks"), "WARN");
    let missing = fixture.fails(&["policy", "nope", "status"]);
    assert!(missing.contains("does not exist"), "{missing}");
    fixture.write("app/payments.py", PAYMENTS);
    let broken = fixture.fails(&["policy", "default", "status"]);
    assert!(broken.contains("FAIL"), "{broken}");
}

/** crane test . also runs the configured smoke-test commands
 * Input
    - None
 * Output
    - None (panics on failure)
*/
#[test]
fn test_requires_dot() {
    let fixture = Fixture::ready();
    let usage = fixture.fails(&["test"]);
    assert!(usage.contains("crane test ."), "{usage}");
    assert!(fixture.ok(&["test", "."]).contains("Crane test: PASS"));
}
