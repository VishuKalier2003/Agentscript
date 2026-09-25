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
    assert!(output.contains("\"repair_owner\":\"agent\""));
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
    assert!(settings.contains("PreToolUse"));
    assert!(settings.contains("pre-tool-use"));
    assert!(settings.contains("crane agent hook --event stop"));

    let second_install = crane(&directory, &["agent", "install", "--profile", "claude"]);
    assert!(!second_install.status.success());
    assert!(String::from_utf8_lossy(&second_install.stderr).contains("already exists"));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn claude_pre_tool_hook_blocks_crane_metadata_edits() {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("crane-hook-guard-{suffix}"));
    fs::create_dir_all(&directory).unwrap();
    git(&directory, &["init", "-q"]);
    assert!(crane(&directory, &["init"]).status.success());

    let output = pre_tool_hook(
        &directory,
        r#"{"tool_name":"Write","tool_input":{"file_path":".crane/policies/payment.crane"}}"#,
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("may not modify .crane"));

    let blocked = [
        r#"{"tool_name":"Edit","tool_input":{"file_path":"D:\\repo\\.CRANE\\checkpoints\\baseline.json"}}"#,
        r#"{"tool_name":"Write","tool_input":{"file_path":".claude/settings.local.json"}}"#,
        r#"{"tool_name":"Bash","tool_input":{"command":"git commit -am wip && crane checkpoint --name baseline"}}"#,
        r#"{"tool_name":"Bash","tool_input":{"command":"./target/release/crane protect --function A.b"}}"#,
        r#"{"tool_name":"Bash","tool_input":{"command":"python -c \"open('.crane/policies/x.crane','w')\""}}"#,
        r#"{"tool_name":"PowerShell","tool_input":{"command":"Remove-Item -Recurse .crane"}}"#,
        r#"{"tool_name":"mcp__fs__write_file","tool_input":{"path":"./.crane/config.toml"}}"#,
    ];
    for payload in blocked {
        assert_eq!(
            pre_tool_hook(&directory, payload).status.code(),
            Some(2),
            "{payload}"
        );
    }

    let allowed = [
        r#"{"tool_name":"Read","tool_input":{"file_path":".crane/policies/payment.crane"}}"#,
        r#"{"tool_name":"Edit","tool_input":{"file_path":"README.md","new_string":"see .crane/policies"}}"#,
        r#"{"tool_name":"Bash","tool_input":{"command":"crane check --agent && crane parse examples/payment.crane"}}"#,
        r#"{"tool_name":"Write","tool_input":{"file_path":"src/my.crane"}}"#,
    ];
    for payload in allowed {
        assert!(
            pre_tool_hook(&directory, payload).status.success(),
            "{payload}"
        );
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn claude_hooks_do_not_loop_on_human_only_failures() {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("crane-hook-human-{suffix}"));
    fs::create_dir_all(&directory).unwrap();
    git(&directory, &["init", "-q"]);
    assert!(crane(&directory, &["init"]).status.success());
    fs::write(
        directory
            .join(".crane")
            .join("policies")
            .join("broken.crane"),
        "policy broken {\n preserve --function\n",
    )
    .unwrap();

    // Direct verification still fails so humans and CI see the malformed policy
    let verify = crane(&directory, &["agent", "verify", "--profile", "claude"]);
    assert!(!verify.status.success());
    let verify_output = String::from_utf8_lossy(&verify.stdout);
    assert!(verify_output.contains("\"violation_type\":\"malformed_policy\""));
    assert!(verify_output.contains("\"repair_owner\":\"human\""));

    // Hooks must not block the agent on something only a human can repair
    for event in ["user-prompt-submit", "post-tool-use", "stop"] {
        let hook = crane(
            &directory,
            &["agent", "hook", "--event", event, "--profile", "claude"],
        );
        assert_eq!(hook.status.code(), Some(0), "{event}");
        let stdout = String::from_utf8_lossy(&hook.stdout);
        let payload: serde_json::Value =
            serde_json::from_str(&stdout).expect("hook output should be Claude hook JSON");
        assert!(payload["systemMessage"]
            .as_str()
            .unwrap()
            .contains("human repairs .crane"));
        assert_eq!(
            payload.get("hookSpecificOutput").is_some(),
            event != "stop",
            "{event}"
        );
    }
    fs::remove_dir_all(directory).unwrap();
}

fn pre_tool_hook(directory: &std::path::Path, payload: &str) -> std::process::Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_crane"))
        .args([
            "agent",
            "hook",
            "--event",
            "pre-tool-use",
            "--profile",
            "claude",
        ])
        .current_dir(directory)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("crane should execute");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}
