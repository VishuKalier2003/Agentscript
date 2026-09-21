use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

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

fn crane(directory: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_crane"))
        .args(args)
        .current_dir(directory)
        .output()
        .expect("crane should execute");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn setup(files: &[(&str, &str)], target: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("crane-preserve-matrix-{suffix}"));
    fs::create_dir_all(&directory).expect("temporary repository should be created");
    for (name, content) in files {
        fs::write(directory.join(name), content).expect("fixture should be written");
    }
    git(&directory, &["init", "-q"]);
    git(&directory, &["config", "user.email", "crane@example.com"]);
    git(&directory, &["config", "user.name", "Crane Tests"]);
    git(&directory, &["add", "."]);
    git(&directory, &["commit", "-qm", "trusted baseline"]);
    assert!(crane(&directory, &["init"]).contains("Initialized"));
    assert!(crane(&directory, &["checkpoint", "--name", "baseline"]).contains("Created checkpoint"));
    let policy = format!(
        "policy test_policy {{\n    checkpoint baseline\n    preserve --function {target}\n}}\n"
    );
    fs::write(
        directory.join(".crane").join("policies").join("test.crane"),
        policy,
    )
    .expect("policy should be written");
    directory
}

fn assert_check(directory: &Path, expected_status: &str, expected_type: Option<&str>) {
    let output = Command::new(env!("CARGO_BIN_EXE_crane"))
        .args(["check", "--json"])
        .current_dir(directory)
        .output()
        .expect("crane should execute");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.success(),
        expected_status == "passed",
        "unexpected status: {text}"
    );
    assert!(text.contains(&format!("\"status\": \"{expected_status}\"")));
    if let Some(violation_type) = expected_type {
        assert!(
            text.contains(&format!("\"violation_type\":\"{violation_type}\"")),
            "{text}"
        );
    }
}

type MatrixCase = (
    &'static str,
    Box<dyn Fn(&Path)>,
    &'static str,
    Option<&'static str>,
);
type ConfigurationCase = (
    &'static str,
    Vec<(&'static str, &'static str)>,
    &'static str,
    Option<&'static str>,
);

const JAVA: &str = r#"class PaymentService {
    public void charge() {
        return;
    }

    public void audit() {
        return;
    }
}
"#;

#[test]
fn preserve_adversarial_matrix() {
    let cases: Vec<MatrixCase> = vec![
        ("unchanged", Box::new(|_| {}), "passed", None),
        (
            "body changed",
            Box::new(|dir| {
                fs::write(
                    dir.join("PaymentService.java"),
                    JAVA.replace("return;", "return 1;"),
                )
                .unwrap()
            }),
            "failed",
            Some("source_changed"),
        ),
        (
            "deleted",
            Box::new(|dir| {
                fs::write(dir.join("PaymentService.java"), "class PaymentService {}\n").unwrap()
            }),
            "failed",
            Some("target_not_found"),
        ),
        (
            "renamed",
            Box::new(|dir| {
                fs::write(
                    dir.join("PaymentService.java"),
                    JAVA.replace("charge", "bill"),
                )
                .unwrap()
            }),
            "failed",
            Some("target_not_found"),
        ),
        (
            "unrelated function changed",
            Box::new(|dir| {
                fs::write(
                    dir.join("PaymentService.java"),
                    JAVA.replace("audit", "auditChanged"),
                )
                .unwrap()
            }),
            "passed",
            None,
        ),
        (
            "formatting changed",
            Box::new(|dir| {
                fs::write(
                    dir.join("PaymentService.java"),
                    JAVA.replace("        return;", "    return;"),
                )
                .unwrap()
            }),
            "passed",
            None,
        ),
        (
            "comment changed",
            Box::new(|dir| {
                fs::write(
                    dir.join("PaymentService.java"),
                    JAVA.replace("return;", "// changed\n        return;"),
                )
                .unwrap()
            }),
            "passed",
            None,
        ),
        (
            "dirty repository",
            Box::new(|dir| fs::write(dir.join("unrelated.txt"), "dirty").unwrap()),
            "passed",
            None,
        ),
        (
            "detached head",
            Box::new(|dir| {
                let head = Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .current_dir(dir)
                    .output()
                    .unwrap();
                let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
                git(dir, &["checkout", "--detach", &sha]);
            }),
            "passed",
            None,
        ),
    ];

    for (name, change, status, violation_type) in cases {
        let directory = setup(&[("PaymentService.java", JAVA)], "PaymentService.charge");
        change(&directory);
        assert_check(&directory, status, violation_type);
        fs::remove_dir_all(&directory).unwrap_or_else(|error| panic!("{name}: {error}"));
    }
}

#[test]
fn preserve_fails_closed_for_resolution_and_configuration_errors() {
    let cases: Vec<ConfigurationCase> = vec![
        (
            "duplicate target",
            vec![
                ("A.java", "class A { void charge() {} }\n"),
                ("B.java", "class B { void charge() {} }\n"),
            ],
            "charge",
            Some("duplicate_target"),
        ),
        (
            "unsupported language",
            vec![("PaymentService.go", "func charge() {}\n")],
            "charge",
            Some("unsupported_language"),
        ),
    ];
    for (name, files, target, expected) in cases {
        let directory = setup(&files, target);
        assert_check(&directory, "failed", expected);
        fs::remove_dir_all(&directory).unwrap_or_else(|error| panic!("{name}: {error}"));
    }

    let directory = setup(&[("PaymentService.java", JAVA)], "PaymentService.charge");
    fs::write(
        directory.join(".crane").join("policies").join("test.crane"),
        "policy test_policy {\n checkpoint baseline\n preserve --function PaymentService.charge\n",
    )
    .unwrap();
    assert_check(&directory, "failed", Some("malformed_policy"));
    fs::remove_dir_all(&directory).unwrap();

    let directory = setup(&[("PaymentService.java", JAVA)], "PaymentService.charge");
    fs::remove_file(
        directory
            .join(".crane")
            .join("checkpoints")
            .join("baseline.json"),
    )
    .unwrap();
    assert_check(&directory, "failed", Some("checkpoint_error"));
    fs::remove_dir_all(&directory).unwrap();

    let directory = setup(&[("PaymentService.java", JAVA)], "PaymentService.charge");
    fs::write(
        directory.join(".crane").join("config.toml"),
        "unsupported = true\n",
    )
    .unwrap();
    assert_check(&directory, "failed", Some("verification_error"));
    fs::remove_dir_all(&directory).unwrap();

    let directory = setup(&[("PaymentService.java", JAVA)], "PaymentService.charge");
    fs::write(
        directory
            .join(".crane")
            .join("checkpoints")
            .join("baseline.json"),
        "{\"name\":\"baseline\",\"commit\":\"not-a-commit\"}\n",
    )
    .unwrap();
    assert_check(&directory, "failed", Some("checkpoint_error"));
    fs::remove_dir_all(&directory).unwrap();

    let directory = setup(&[("PaymentService.java", JAVA)], "PaymentService.charge");
    fs::write(
        directory.join("PaymentService.java"),
        "class PaymentService { public void charge( { return; } }\n",
    )
    .unwrap();
    assert_check(&directory, "failed", Some("parse_failure"));
    fs::remove_dir_all(&directory).unwrap();
}
