use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn git(directory: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .expect("git should execute");
    assert!(output.status.success());
}

fn crane(directory: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_crane"))
        .args(args)
        .current_dir(directory)
        .output()
        .expect("crane should execute")
}

#[test]
fn adapter_initializes_and_returns_repair_feedback() {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("crane-adapter-{suffix}"));
    fs::create_dir_all(&directory).unwrap();
    let source = "class PaymentService {\n    public void charge() { return; }\n}\n";
    fs::write(directory.join("PaymentService.java"), source).unwrap();
    git(&directory, &["init", "-q"]);
    git(&directory, &["config", "user.email", "crane@example.com"]);
    git(&directory, &["config", "user.name", "Crane Adapter Test"]);
    git(&directory, &["add", "."]);
    git(&directory, &["commit", "-qm", "baseline"]);

    assert!(crane(&directory, &["agent", "init", "--profile", "claude"])
        .status
        .success());
    assert!(crane(&directory, &["checkpoint", "--name", "baseline"])
        .status
        .success());
    fs::write(
        directory
            .join(".crane")
            .join("policies")
            .join("payment.crane"),
        "policy payment {\n checkpoint baseline\n preserve --function PaymentService.charge\n}\n",
    )
    .unwrap();
    fs::write(
        directory.join("PaymentService.java"),
        source.replace("return;", "return 1;"),
    )
    .unwrap();

    let failed = crane(&directory, &["agent", "verify", "--profile", "claude"]);
    let output = String::from_utf8_lossy(&failed.stdout);
    assert!(!failed.status.success());
    assert!(output.contains("\"status\": \"failed\""));
    assert!(output.contains("\"violation_type\":\"source_changed\""));
    let hook_failed = crane(
        &directory,
        &[
            "agent",
            "hook",
            "--event",
            "post-tool-use",
            "--profile",
            "claude",
        ],
    );
    assert_eq!(hook_failed.status.code(), Some(2));

    fs::write(directory.join("PaymentService.java"), source).unwrap();
    assert!(
        crane(&directory, &["agent", "verify", "--profile", "codex"])
            .status
            .success()
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn claude_hook_install_writes_project_local_settings() {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("crane-hook-{suffix}"));
    fs::create_dir_all(&directory).unwrap();
    git(&directory, &["init", "-q"]);

    let installed = crane(&directory, &["agent", "install", "--profile", "claude"]);
    assert!(installed.status.success());
    let settings = fs::read_to_string(directory.join(".claude").join("settings.local.json"))
        .expect("Claude settings should be created");
    assert!(settings.contains("SessionStart"));
    assert!(settings.contains("UserPromptSubmit"));
    assert!(settings.contains("user-prompt-submit"));
    assert!(settings.contains("post-tool-use"));
    assert!(settings.contains("crane agent hook --event stop"));

    let second_install = crane(&directory, &["agent", "install", "--profile", "claude"]);
    assert!(!second_install.status.success());
    assert!(String::from_utf8_lossy(&second_install.stderr).contains("already exists"));
    fs::remove_dir_all(directory).unwrap();
}
