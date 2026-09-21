use std::fs;

use crate::model::{Rule, Violation};
use crate::policy::parse_file;
use crate::repository::{ensure_commit, ensure_initialized, load_checkpoint, root};
use crate::resolver::{resolve_git, resolve_worktree};
use crate::util::{escape_json, io_error};

pub(crate) fn run(json: bool) -> Result<(), String> {
    ensure_initialized()?;
    let directory = root()?.join("policies");
    let mut policies = Vec::new();
    for entry in fs::read_dir(directory).map_err(io_error)? {
        let path = entry.map_err(io_error)?.path();
        if path.extension().and_then(|value| value.to_str()) == Some("crane") {
            policies.push(parse_file(&path)?);
        }
    }
    policies.sort_by(|left, right| left.name.cmp(&right.name));
    let mut passes = Vec::new();
    let mut failures = Vec::new();
    for policy in policies {
        for rule in policy.rules {
            match rule {
                Rule::PreserveFunction { target } => {
                    let checkpoint = load_checkpoint(&policy.checkpoint)?;
                    ensure_commit(&checkpoint.commit)?;
                    let baseline = resolve_git(&checkpoint.commit, &target)?.ok_or_else(|| {
                        format!("cannot locate {target} in checkpoint {}", checkpoint.name)
                    })?;
                    let current = resolve_worktree(&target)?
                        .ok_or_else(|| format!("protected function {target} is missing"))?;
                    if baseline.snippet == current.snippet {
                        passes.push((policy.name.clone(), target));
                    } else {
                        failures.push(Violation {
                            policy: policy.name.clone(),
                            rule: "preserve".into(),
                            target: target.clone(),
                            checkpoint: checkpoint.name,
                            message: format!("{target} changed"),
                        });
                    }
                }
            }
        }
    }
    if json {
        print_json(&passes, &failures);
    } else {
        for (policy, target) in &passes {
            println!("PASS {policy}: preserve --function {target}");
        }
        for violation in &failures {
            println!(
                "FAIL {}: {} (checkpoint {})",
                violation.policy, violation.message, violation.checkpoint
            );
        }
        println!(
            "Crane check: {}",
            if failures.is_empty() { "PASS" } else { "FAIL" }
        );
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err("one or more Crane policies failed".into())
    }
}

fn print_json(passes: &[(String, String)], violations: &[Violation]) {
    println!(
        "{{\n  \"status\": \"{}\",\n  \"passes\": [",
        if violations.is_empty() {
            "pass"
        } else {
            "fail"
        }
    );
    for (index, (policy, target)) in passes.iter().enumerate() {
        if index > 0 {
            print!(",");
        }
        print!(
            "\n    {{\"policy\":\"{}\",\"target\":\"{}\"}}",
            escape_json(policy),
            escape_json(target)
        );
    }
    println!("\n  ],\n  \"violations\": [");
    for (index, violation) in violations.iter().enumerate() {
        if index > 0 {
            print!(",");
        }
        print!(
            "\n    {{\"policy\":\"{}\",\"rule\":\"{}\",\"target\":\"{}\",\"checkpoint\":\"{}\",\"message\":\"{}\"}}",
            escape_json(&violation.policy),
            escape_json(&violation.rule),
            escape_json(&violation.target),
            escape_json(&violation.checkpoint),
            escape_json(&violation.message)
        );
    }
    println!("\n  ]\n}}");
}
