use std::fs;

use crate::model::{Rule, Violation};
use crate::policy::parse_file;
use crate::repository::{ensure_commit, ensure_initialized, load_checkpoint, root};
use crate::resolver::{resolve_git, resolve_worktree, supported_extensions, Resolution};
use crate::util::{escape_json, io_error};

struct Pass {
    policy_id: String,
    target: String,
}

struct Report {
    passes: Vec<Pass>,
    violations: Vec<Violation>,
}

pub(crate) fn run(json: bool, agent: bool) -> Result<(), String> {
    if let Err(error) = ensure_initialized() {
        if json || agent {
            print_error_json(&error);
        }
        return Err(error);
    }
    let report = match evaluate() {
        Ok(report) => report,
        Err(error) => {
            if json || agent {
                print_error_json(&error);
            }
            return Err(error);
        }
    };
    if json || agent {
        print_json(&report);
        if agent && !report.violations.is_empty() {
            eprintln!("{}", render_json(&report));
        }
    } else {
        print_human(&report);
    }
    if report.violations.is_empty() {
        Ok(())
    } else {
        Err("one or more Crane policies failed".into())
    }
}

fn print_error_json(error: &str) {
    let report = Report {
        passes: Vec::new(),
        violations: vec![Violation {
            policy_id: "crane".into(),
            rule: "verification".into(),
            target: String::new(),
            checkpoint: String::new(),
            violation_type: classify_violation(error),
            message: error.into(),
        }],
    };
    print_json(&report);
    eprintln!("{}", render_json(&report));
}

fn evaluate() -> Result<Report, String> {
    let directory = root()?.join("policies");
    let mut paths = fs::read_dir(directory)
        .map_err(io_error)?
        .map(|entry| entry.map(|value| value.path()).map_err(io_error))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    let mut policies = Vec::new();
    let mut policy_errors = Vec::new();
    for path in paths {
        if path.extension().and_then(|value| value.to_str()) == Some("crane") {
            match parse_file(&path) {
                Ok(policy) => policies.push(policy),
                Err(message) => policy_errors.push(Violation {
                    policy_id: path
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or("unknown")
                        .into(),
                    rule: "policy".into(),
                    target: String::new(),
                    checkpoint: String::new(),
                    violation_type: "malformed_policy".into(),
                    message,
                }),
            }
        }
    }
    policies.sort_by(|left, right| left.name.cmp(&right.name));

    let mut report = Report {
        passes: Vec::new(),
        violations: policy_errors,
    };
    for policy in policies {
        for rule in policy.rules {
            match rule {
                Rule::PreserveFunction { target } => {
                    let checkpoint_name = policy.checkpoint.clone();
                    let result = verify_function(&policy.name, &checkpoint_name, &target);
                    match result {
                        Ok(()) => report.passes.push(Pass {
                            policy_id: policy.name.clone(),
                            target,
                        }),
                        Err(message) => report.violations.push(Violation {
                            policy_id: policy.name.clone(),
                            rule: "preserve".into(),
                            target,
                            checkpoint: checkpoint_name,
                            violation_type: classify_violation(&message),
                            message,
                        }),
                    }
                }
            }
        }
    }
    Ok(report)
}

fn verify_function(policy_id: &str, checkpoint_name: &str, target: &str) -> Result<(), String> {
    let checkpoint = load_checkpoint(checkpoint_name)
        .map_err(|error| format!("{error} (policy {policy_id}, target {target})"))?;
    ensure_commit(&checkpoint.commit)?;
    let baseline = match resolve_git(&checkpoint.commit, target)? {
        Resolution::Found(value) => value,
        Resolution::Missing => {
            return Err(format!(
                "protected function {target} is missing from checkpoint; supported source extensions: {}",
                supported_extensions()
            ))
        }
        Resolution::Duplicate(count) => {
            return Err(format!("protected function {target} is ambiguous in checkpoint ({count} matches)"))
        }
        Resolution::Unsupported => return Err(format!("checkpoint source language is unsupported for {target}")),
        Resolution::ParseFailure(error) => return Err(format!("checkpoint source could not be parsed: {error}")),
    };
    let current = match resolve_worktree(target)? {
        Resolution::Found(value) => value,
        Resolution::Missing => {
            return Err(format!(
                "protected function {target} is missing from the worktree; supported source extensions: {}",
                supported_extensions()
            ))
        }
        Resolution::Duplicate(count) => {
            return Err(format!("protected function {target} is ambiguous in worktree ({count} matches)"))
        }
        Resolution::Unsupported => return Err(format!("worktree source language is unsupported for {target}")),
        Resolution::ParseFailure(error) => return Err(format!("worktree source could not be parsed: {error}")),
    };
    if baseline.snippet == current.snippet {
        Ok(())
    } else {
        Err("Protected function was modified.".into())
    }
}

fn print_human(report: &Report) {
    for pass in &report.passes {
        println!(
            "PASS {}: preserve --function {}",
            pass.policy_id, pass.target
        );
    }
    for violation in &report.violations {
        println!(
            "FAIL {}: {} (checkpoint {})",
            violation.policy_id, violation.message, violation.checkpoint
        );
    }
    println!(
        "Crane check: {}",
        if report.violations.is_empty() {
            "PASS"
        } else {
            "FAIL"
        }
    );
}

fn print_json(report: &Report) {
    println!("{}", render_json(report));
}

fn render_json(report: &Report) -> String {
    let status = if report.violations.is_empty() {
        "passed"
    } else {
        "failed"
    };
    let mut output = format!("{{\n  \"status\": \"{status}\",\n  \"violations\": [");
    for (index, violation) in report.violations.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(&format!(
            "    {{\"policy_id\":\"{}\",\"rule\":\"{}\",\"target\":\"{}\",\"checkpoint\":\"{}\",\"violation_type\":\"{}\",\"message\":\"{}\"}}",
            escape_json(&violation.policy_id),
            escape_json(&violation.rule),
            escape_json(&violation.target),
            escape_json(&violation.checkpoint),
            escape_json(&violation.violation_type),
            escape_json(&violation.message)
        ));
    }

    output.push_str("\n  ]\n}");
    output
}

fn classify_violation(message: &str) -> String {
    if message.contains("ambiguous") {
        "duplicate_target"
    } else if message.contains("missing from") {
        "target_not_found"
    } else if message.contains("unsupported") {
        "unsupported_language"
    } else if message.contains("could not be parsed") {
        "parse_failure"
    } else if message.contains("checkpoint") {
        "checkpoint_error"
    } else if message.contains("modified") {
        "source_changed"
    } else {
        "verification_error"
    }
    .into()
}
