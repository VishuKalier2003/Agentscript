use std::fs;

use crate::model::{ItemKind, Rule, Scope, Violation};
use crate::policy::parse_file;
use crate::repository::{ensure_commit, ensure_initialized, load_checkpoint, root};
use crate::resolver::{resolve_git, resolve_worktree, supported_extensions, Resolution};
use crate::scope::{verify_scope, verify_target, ScopeContext};
use crate::util::{escape_json, io_error};

/** A rule that passed, used only for human-readable output
 * Fields
    - policy_id: String - policy the rule belongs to
    - description: String - the rule in policy syntax
*/
struct Pass {
    policy_id: String,
    description: String,
}

/** Result of evaluating every policy; the check passes only when violations is empty
 * Fields
    - passes: Vec<Pass> - rules that passed
    - violations: Vec<Violation> - malformed policies first, then failed rules in policy-name order
*/
struct Report {
    passes: Vec<Pass>,
    violations: Vec<Violation>,
}

/** Verify all policies and report the result, by first confirming Crane is initialized, then
 * evaluating every policy into a report, and finally printing it as JSON (json/agent modes, with
 * violations echoed to stderr in agent mode) or as human-readable lines
 * Input
    - json: bool - print the stable JSON contract
    - agent: bool - JSON plus stderr feedback for agents
 * Output
    - Result<(), String>
    - Error if setup fails or any policy is violated
*/
pub(crate) fn run(json: bool, agent: bool) -> Result<(), String> {
    // Verify policies without changing source, checkpoints, or repository state
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

/** Run verification for a Claude Code hook, by first building a report (turning setup errors into
 * a setup violation), then deciding which violations block: agent-repairable violations always
 * block, except unmet target rules, which are work still to do and block only at stop (and not
 * on a stop that is already a forced retry, so an unreachable target cannot loop forever);
 * anything left is reported with non-blocking hook JSON so human-only failures and pending
 * targets never trap the agent
 * Input
    - event: &str - hook event name (user-prompt-submit, post-tool-use, or stop)
    - stop_hook_active: bool - Claude Code is already continuing because a stop hook blocked
 * Output
    - Result<(), String>
    - HOOK_BLOCK error if the agent must repair a violation now
*/
pub(crate) fn run_hook(event: &str, stop_hook_active: bool) -> Result<(), String> {
    let report = match ensure_initialized().and_then(|()| evaluate()) {
        Ok(report) => report,
        Err(error) => setup_report(&error),
    };
    let enforce_targets = event == "stop" && !stop_hook_active;
    if report.violations.iter().any(|violation| {
        repair_owner(violation) == "agent" && (enforce_targets || !is_pending_target(violation))
    }) {
        print_json(&report);
        eprintln!("{}", render_json(&report));
        return Err("HOOK_BLOCK:one or more Crane policies failed".into());
    }
    if report.violations.is_empty() {
        print_json(&report);
        return Ok(());
    }
    println!("{}", render_non_blocking_hook(event, &report));
    Ok(())
}

/** Check whether a violation is a target rule that is not met yet (nothing changed, or the change
 * is of the wrong kind), as opposed to a failure that prevents checking the target at all
 * Input
    - violation: &Violation - violation to check
 * Output
    - bool, true for target_unchanged and change_type_mismatch
*/
fn is_pending_target(violation: &Violation) -> bool {
    matches!(
        violation.violation_type.as_str(),
        "target_unchanged" | "change_type_mismatch"
    )
}

/** Build the non-blocking hook JSON for violations that must not block now, by writing a
 * systemMessage for the user covering human-only failures and pending targets, and for events
 * that support it adding additionalContext that tells the agent not to touch human-only failures,
 * lists the targets its task still has to satisfy, and includes the full JSON report
 * Input
    - event: &str - hook event name
    - report: &Report - report whose violations are human-owned or pending targets
 * Output
    - String of Claude Code hook JSON
*/
fn render_non_blocking_hook(event: &str, report: &Report) -> String {
    let list = |pending: bool| {
        report
            .violations
            .iter()
            .filter(|violation| is_pending_target(violation) == pending)
            .map(|violation| format!("{}: {}", violation.policy_id, violation.message))
            .collect::<Vec<_>>()
            .join("; ")
    };
    let (human, targets) = (list(false), list(true));
    let mut message = Vec::new();
    let mut context = Vec::new();
    if !human.is_empty() {
        message.push(format!(
            "Crane cannot fully verify this repository until a human repairs .crane metadata: {human}"
        ));
        context.push("Crane reported problems that only a human can repair (repair_owner \"human\"). Agents are blocked from .crane, so do not try to fix or work around them; continue the task and mention them to the user.".to_string());
    }
    if !targets.is_empty() {
        message.push(format!("Crane targets not yet satisfied: {targets}"));
        context.push(format!(
            "Crane target rules require these changes before the task is complete: {targets}"
        ));
    }
    let mut output = serde_json::json!({ "systemMessage": message.join(" | ") });
    let hook_event_name = match event {
        "user-prompt-submit" => Some("UserPromptSubmit"),
        "post-tool-use" => Some("PostToolUse"),
        _ => None,
    };
    if let Some(hook_event_name) = hook_event_name {
        output["hookSpecificOutput"] = serde_json::json!({
            "hookEventName": hook_event_name,
            "additionalContext": format!("{}\n{}", context.join("\n"), render_json(report)),
        });
    }
    output.to_string()
}

/** Print a setup failure in the standard JSON contract, by wrapping it in a setup report and
 * writing the JSON to both stdout and stderr
 * Input
    - error: &str - setup error message
 * Output
    - None (writes to stdout and stderr)
*/
fn print_error_json(error: &str) {
    // Convert setup failures into the same violation shape as policy failures
    let report = setup_report(error);
    print_json(&report);
    eprintln!("{}", render_json(&report));
}

/** Wrap a setup-level error as a report, by creating a single violation with policy_id crane, rule
 * verification, empty target and checkpoint, and a type classified from the message
 * Input
    - error: &str - setup error message
 * Output
    - Report with no passes and one violation
*/
fn setup_report(error: &str) -> Report {
    Report {
        passes: Vec::new(),
        violations: vec![Violation {
            policy_id: "crane".into(),
            rule: "verification".into(),
            target: String::new(),
            checkpoint: String::new(),
            violation_type: classify_violation(error),
            message: error.into(),
        }],
    }
}

/** Evaluate every policy into a report, by first reading and sorting the policies directory,
 * parsing each .crane file (recording malformed files as malformed_policy violations), then
 * sorting parsed policies by name, and finally verifying each preserve rule into a pass or violation
 * Input
    - None
 * Output
    - Result<Report, String>
    - Error if .crane or the policies directory cannot be read
*/
fn evaluate() -> Result<Report, String> {
    // Parse policies in deterministic order before evaluating each rule independently
    let directory = root()?.join("policies");
    // Read all files in the policies directory, converts to vector of paths
    let mut paths = fs::read_dir(directory)
        .map_err(io_error)?
        .map(|entry| entry.map(|value| value.path()).map_err(io_error))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort(); // gives deterministic order, preventing any ambiguity
    let mut policies = Vec::new();
    let mut policy_errors = Vec::new();
    for path in paths {
        if path.extension().and_then(|value| value.to_str()) == Some("crane") {
            // parse the policy files, ensuring that policies are checked
            match parse_file(&path) {
                Ok(policy) => policies.push(policy),
                Err(message) => policy_errors.push(Violation {  // detailed violation message for the AI Agent
                    policy_id: path
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or("unknown")
                        .into(),
                    rule: "policy".into(),
                    target: String::new(),
                    checkpoint: String::new(),
                    violation_type: "malformed_policy".into(),
                    message: format!(
                        "{message}; manual repair required because agents are blocked from modifying .crane metadata"
                    ),
                }),
            }
        }
    }
    // The parsed policies are sorted again for deterministic behavior
    policies.sort_by(|left, right| left.name.cmp(&right.name));
    // Wrapping up the report
    // One scope context per run shares Git processes and parsed files across rules
    let mut scope_context = ScopeContext::new();
    let mut report = Report {
        passes: Vec::new(),
        violations: policy_errors,
    };
    for policy in policies {
        for rule in policy.rules {
            let checkpoint_name = policy.checkpoint.clone();
            let description = rule.describe();
            // Match the policy with the rule
            let (keyword, target, result) = match rule {
                Rule::Preserve {
                    kind,
                    target,
                    scope,
                } => {
                    let result = verify_rule(
                        &mut scope_context,
                        &policy.name,
                        &checkpoint_name,
                        kind,
                        &target,
                        scope,
                    );
                    ("preserve", target, result)
                }
                Rule::Target {
                    kind,
                    target,
                    scope,
                    change_type,
                } => {
                    let result = load_checkpoint(&checkpoint_name)
                        .map_err(|error| {
                            format!("{error} (policy {}, target {target})", policy.name)
                        })
                        .and_then(|checkpoint| {
                            verify_target(
                                &mut scope_context,
                                scope,
                                change_type,
                                &checkpoint.commit,
                                kind,
                                &target,
                            )
                        });
                    ("target", target, result)
                }
            };
            match result {
                Ok(()) => report.passes.push(Pass {
                    policy_id: policy.name.clone(),
                    description,
                }),
                Err(message) => report.violations.push(Violation {
                    policy_id: policy.name.clone(),
                    rule: keyword.into(),
                    target,
                    checkpoint: checkpoint_name,
                    violation_type: classify_violation(&message),
                    message,
                }),
            }
        }
    }
    Ok(report) // Pass the completed report
}

/** Verify one preserve rule at its scope, by comparing the item's node directly for block scope
 * and otherwise loading the checkpoint and delegating to scope::verify_scope
 * Input
    - scope_context: &mut ScopeContext - state shared by all rules in this run
    - policy_id: &str - policy name, used in error messages
    - checkpoint_name: &str - checkpoint the policy compares against
    - kind: ItemKind - function, data, variable, class, or interface
    - target: &str - qualified target
    - scope: Scope - how much code around the target is protected
 * Output
    - Result<(), String>
    - Error describing why the scope could not be verified or what was modified
*/
fn verify_rule(
    scope_context: &mut ScopeContext,
    policy_id: &str,
    checkpoint_name: &str,
    kind: ItemKind,
    target: &str,
    scope: Scope,
) -> Result<(), String> {
    if scope == Scope::Block {
        return verify_item(policy_id, checkpoint_name, kind, target);
    }
    let checkpoint = load_checkpoint(checkpoint_name)
        .map_err(|error| format!("{error} (policy {policy_id}, target {target})"))?;
    verify_scope(scope_context, scope, &checkpoint.commit, kind, target)
}

/** Check that one protected item is unchanged, by first loading the checkpoint and confirming
 * its commit exists, then resolving the item in the checkpoint commit and in the worktree
 * (failing on missing, duplicate, unsupported, or unparsable results), and finally comparing the
 * two canonical snippets
 * Input
    - policy_id: &str - policy name, used in error messages
    - checkpoint_name: &str - checkpoint the policy compares against
    - kind: ItemKind - function, data, variable, class, or interface
    - target: &str - qualified target
 * Output
    - Result<(), String>
    - Error describing why the target could not be verified or that it was modified
*/
fn verify_item(
    policy_id: &str,
    checkpoint_name: &str,
    kind: ItemKind,
    target: &str,
) -> Result<(), String> {
    // Compare one protected target from the trusted commit against the worktree
    let checkpoint = load_checkpoint(checkpoint_name)
        .map_err(|error| format!("{error} (policy {policy_id}, target {target})"))?;
    ensure_commit(&checkpoint.commit)?;
    let noun = kind.noun();
    let baseline = match resolve_git(&checkpoint.commit, kind, target)? {
        Resolution::Found(value) => value,
        Resolution::Missing => {
            return Err(format!(
            "protected {noun} {target} is missing from checkpoint; supported source extensions: {}",
            supported_extensions()
        ))
        }
        Resolution::Duplicate(count) => {
            return Err(format!(
                "protected {noun} {target} is ambiguous in checkpoint ({count} matches)"
            ))
        }
        Resolution::Unsupported => {
            return Err(format!(
                "checkpoint source language is unsupported for {target}"
            ))
        }
        Resolution::ParseFailure(error) => {
            return Err(format!("checkpoint source could not be parsed: {error}"))
        }
    };
    let current = match resolve_worktree(kind, target)? {
        Resolution::Found(value) => value,
        Resolution::Missing => {
            return Err(format!(
                "protected {noun} {target} is missing from the worktree; supported source extensions: {}",
                supported_extensions()
            ))
        }
        Resolution::Duplicate(count) => {
            return Err(format!("protected {noun} {target} is ambiguous in worktree ({count} matches)"))
        }
        Resolution::Unsupported => return Err(format!("worktree source language is unsupported for {target}")),
        Resolution::ParseFailure(error) => return Err(format!("worktree source could not be parsed: {error}")),
    };
    if baseline.snippet == current.snippet {
        Ok(())
    } else {
        Err(format!("Protected {noun} was modified."))
    }
}

/** Print a report for terminal use, by writing a PASS line for each pass, a FAIL line for each
 * violation, and a final overall PASS/FAIL line
 * Input
    - report: &Report - evaluation result
 * Output
    - None (writes to stdout)
*/
fn print_human(report: &Report) {
    // Render a concise report for interactive terminal use
    for pass in &report.passes {
        println!("PASS {}: {}", pass.policy_id, pass.description);
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

/** Print a report as JSON on stdout, by rendering it with render_json
 * Input
    - report: &Report - evaluation result
 * Output
    - None (writes to stdout)
*/
fn print_json(report: &Report) {
    // Keep JSON output on stdout for scripts and agent integrations
    println!("{}", render_json(report));
}

/** Render a report in the stable JSON contract, by writing the status (passed or failed) and then
 * looping through the violations to emit their seven fields in a fixed order with escaped values
 * Input
    - report: &Report - evaluation result
 * Output
    - String of JSON
*/
fn render_json(report: &Report) -> String {
    // Serialize fields in a fixed order to keep the agent contract deterministic
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
            "    {{\"policy_id\":\"{}\",\"rule\":\"{}\",\"target\":\"{}\",\"checkpoint\":\"{}\",\"violation_type\":\"{}\",\"repair_owner\":\"{}\",\"message\":\"{}\"}}",
            escape_json(&violation.policy_id),
            escape_json(&violation.rule),
            escape_json(&violation.target),
            escape_json(&violation.checkpoint),
            escape_json(&violation.violation_type),
            repair_owner(violation),
            escape_json(&violation.message)
        ));
    }

    output.push_str("\n  ]\n}");
    output
}

/** Decide who can repair a violation, by returning agent for source_changed violations or any
 * message about the worktree, and human for everything else (policy, checkpoint, and setup
 * failures that require editing .crane)
 * Input
    - violation: &Violation - violation to classify
 * Output
    - &'static str, "agent" or "human"
*/
fn repair_owner(violation: &Violation) -> &'static str {
    // Only worktree source problems are agent-repairable; policy, checkpoint, and setup failures need .crane edits
    if violation.violation_type == "source_changed"
        || is_pending_target(violation)
        || violation.message.contains("worktree")
    {
        "agent"
    } else {
        "human"
    }
}

/** Map a verifier message to a machine-readable violation type, by checking it for known phrases
 * in priority order and falling back to verification_error
 * Input
    - message: &str - verifier error message
 * Output
    - String violation type such as source_changed or checkpoint_error
*/
fn classify_violation(message: &str) -> String {
    // Map stable verifier messages to machine-readable failure categories
    if message.starts_with("Target ") && message.contains(" was not changed within ") {
        // Checked first: target messages list changed paths, which may contain any other keyword
        "target_unchanged"
    } else if message.starts_with("Target change within ") {
        "change_type_mismatch"
    } else if message.starts_with("Protected ") && message.contains(" was modified") {
        // Checked first: scope messages list changed paths, which may contain any other keyword
        "source_changed"
    } else if message.contains("ambiguous") {
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
