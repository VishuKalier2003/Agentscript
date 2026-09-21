use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn run_crane(directory: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_crane"))
        .args(args)
        .current_dir(directory)
        .output()
        .expect("Crane should execute")
}

fn run_git(directory: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .expect("Git should execute");
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn agent_verification_detects_and_accepts_repair() {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("crane-agent-workflow-{suffix}"));
    fs::create_dir_all(&directory).expect("temporary repository should be created");

    let source = "class PaymentService {\n    public void charge() {\n        return;\n    }\n}\n";
    fs::write(directory.join("PaymentService.java"), source).expect("source should be written");
    run_git(&directory, &["init", "-q"]);
    run_git(&directory, &["config", "user.email", "crane@example.com"]);
    run_git(&directory, &["config", "user.name", "Crane Test"]);
    run_git(&directory, &["add", "PaymentService.java"]);
    run_git(&directory, &["commit", "-qm", "trusted baseline"]);
    assert!(run_crane(&directory, &["init"]).status.success());
    assert!(run_crane(&directory, &["checkpoint", "--name", "baseline"])
        .status
        .success());
    assert!(run_crane(
        &directory,
        &[
            "protect",
            "--function",
            "PaymentService.charge",
            "--policy",
            "payment_service"
        ]
    )
    .status
    .success());

    fs::write(
        directory.join("PaymentService.java"),
        source.replace("return;", "return 1;"),
    )
    .expect("modified source should be written");
    let failed = run_crane(&directory, &["check", "--agent"]);
    let failed_stdout = String::from_utf8_lossy(&failed.stdout);
    assert!(!failed.status.success());
    assert!(failed_stdout.contains("\"status\": \"failed\""));
    assert!(failed_stdout.contains("\"policy_id\":\"payment_service\""));
    assert!(failed_stdout.contains("\"message\":\"Protected function was modified.\""));

    fs::write(directory.join("PaymentService.java"), source).expect("source should be restored");
    let passed = run_crane(&directory, &["check", "--agent"]);
    assert!(passed.status.success());
    assert!(String::from_utf8_lossy(&passed.stdout).contains("\"status\": \"passed\""));

    fs::remove_dir_all(directory).expect("temporary repository should be removed");
}
