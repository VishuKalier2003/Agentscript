use crate::ir::{Clause, ContractSet, Postcondition};
use crate::model::{ItemKind, Scope, Violation};
use crate::repository::ensure_commit;
use crate::resolver::{resolve_git, resolve_worktree, supported_extensions, Resolution};
use crate::scope::{verify_scope, verify_target, ScopeContext};
use crate::util::escape_json;

/** A rule that passed
 * Fields
    - policy_id: String - policy the rule belongs to
    - rule: &'static str - "preserve" or "target"
    - target: String - item the rule is anchored on
    - checkpoint: String - checkpoint name
    - description: String - the rule in policy syntax
*/
pub(crate) struct Pass {
    pub(crate) policy_id: String,
    pub(crate) rule: &'static str,
    pub(crate) target: String,
    pub(crate) checkpoint: String,
    pub(crate) description: String,
}

/** Result of verifying a contract set; the check passes only when violations is empty
 * Fields
    - passes: Vec<Pass> - rules that passed
    - violations: Vec<Violation> - malformed policies first, then failed rules in policy-name order
*/
pub(crate) struct Report {
    pub(crate) passes: Vec<Pass>,
    pub(crate) violations: Vec<Violation>,
}

/** What a lifecycle hook should do with a verification report
 * Variants
    - Clean - nothing failed
    - Block - the agent must repair something now
    - Advisory - something failed, but only a human can repair it or it is a target still to do
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Assessment {
    Clean,
    Block,
    Advisory,
}

/** Verify the postconditions of a contract set against the worktree, by first reporting every
 * malformed policy, then checking each clause selected by relevant (clauses of a contract whose
 * checkpoint could not be loaded fail with that error) and recording it as a pass or a violation
 * Input
    - set: &ContractSet - compiled contracts, from disk or from a session
    - context: &mut ScopeContext - shared Git processes and parsed files for this run
    - relevant: &dyn Fn(usize) -> bool - selects clauses by their index in ContractSet::clauses
 * Output
    - Report
*/
pub(crate) fn verify_contracts(
    set: &ContractSet,
    context: &mut ScopeContext,
    relevant: &dyn Fn(usize) -> bool,
) -> Report {
    let mut report = Report {
        passes: Vec::new(),
        violations: set
            .malformed
            .iter()
            .map(|policy| Violation {
                policy_id: policy.policy_id.clone(),
                rule: "policy".into(),
                target: String::new(),
                checkpoint: String::new(),
                violation_type: "malformed_policy".into(),
                message: format!(
                    "{}; manual repair required because agents are blocked from modifying .crane metadata",
                    policy.message
                ),
            })
            .collect(),
    };
    for (index, contract, clause) in set.clauses() {
        if !relevant(index) {
            continue;
        }
        let result = contract
            .checkpoint_sha
            .as_ref()
            .map_err(|error| {
                format!(
                    "{error} (policy {}, target {})",
                    contract.policy_id, clause.target
                )
            })
            .and_then(|commit| verify_clause(context, commit, clause));
        match result {
            Ok(()) => report.passes.push(Pass {
                policy_id: contract.policy_id.clone(),
                rule: clause.keyword(),
                target: clause.target.clone(),
                checkpoint: contract.checkpoint.clone(),
                description: clause.rule().describe(),
            }),
            Err(message) => report.violations.push(Violation {
                policy_id: contract.policy_id.clone(),
                rule: clause.keyword().into(),
                target: clause.target.clone(),
                checkpoint: contract.checkpoint.clone(),
                violation_type: classify_violation(&message),
                message,
            }),
        }
    }
    report
}

/** Check one clause's postcondition, by comparing the item directly for a block-scope preserve,
 * delegating other preserve scopes to scope::verify_scope, and delegating targets to
 * scope::verify_target
 * Input
    - context: &mut ScopeContext - shared state for this run
    - commit: &str - checkpoint commit bound to the clause's contract
    - clause: &Clause - clause to check
 * Output
    - Result<(), String>
    - Error describing why the postcondition does not hold or could not be checked
*/
fn verify_clause(context: &mut ScopeContext, commit: &str, clause: &Clause) -> Result<(), String> {
    match clause.postcondition() {
        Postcondition::Unchanged if clause.scope == Scope::Block => {
            verify_item(commit, clause.kind, &clause.target)
        }
        Postcondition::Unchanged => {
            verify_scope(context, clause.scope, commit, clause.kind, &clause.target)
        }
        Postcondition::Changed(change_type) => verify_target(
            context,
            clause.scope,
            change_type,
            commit,
            clause.kind,
            &clause.target,
        ),
    }
}

/** Check that one protected item is unchanged, by first confirming the checkpoint commit exists,
 * then resolving the item in that commit and in the worktree (failing on missing, duplicate,
 * unsupported, or unparsable results), and finally comparing the two canonical snippets
 * Input
    - commit: &str - checkpoint commit SHA
    - kind: ItemKind - function, data, variable, class, or interface
    - target: &str - qualified target
 * Output
    - Result<(), String>
    - Error describing why the target could not be verified or that it was modified
*/
fn verify_item(commit: &str, kind: ItemKind, target: &str) -> Result<(), String> {
    ensure_commit(commit)?;
    let noun = kind.noun();
    let baseline = match resolve_git(commit, kind, target)? {
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

/** Decide what a lifecycle hook does with a report: agent-repairable violations block, except
 * unmet targets, which are work still to do and block only at stop (and not on a stop that is
 * already a forced retry, so an unreachable target cannot loop forever); anything else left is
 * advisory, so human-only failures and pending targets never trap the agent
 * Input
    - report: &Report - verification result
    - enforce_targets: bool - true only for a first (not forced-retry) stop
 * Output
    - Assessment
*/
pub(crate) fn assess(report: &Report, enforce_targets: bool) -> Assessment {
    if report.violations.iter().any(|violation| {
        repair_owner(violation) == "agent" && (enforce_targets || !is_pending_target(violation))
    }) {
        Assessment::Block
    } else if report.violations.is_empty() {
        Assessment::Clean
    } else {
        Assessment::Advisory
    }
}

/** Check whether a violation is a target rule that is not met yet (nothing changed, or the change
 * is of the wrong kind), as opposed to a failure that prevents checking the target at all
 * Input
    - violation: &Violation - violation to check
 * Output
    - bool, true for target_unchanged and change_type_mismatch
*/
pub(crate) fn is_pending_target(violation: &Violation) -> bool {
    matches!(
        violation.violation_type.as_str(),
        "target_unchanged" | "change_type_mismatch"
    )
}

/** Wrap a setup-level error as a report, by creating a single violation with policy_id crane, rule
 * verification, empty target and checkpoint, and a type classified from the message
 * Input
    - error: &str - setup error message
 * Output
    - Report with no passes and one violation
*/
pub(crate) fn setup_report(error: &str) -> Report {
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

/** Render a report in the stable JSON contract, by writing the status (passed or failed) and then
 * looping through the violations to emit their seven fields in a fixed order with escaped values
 * Input
    - report: &Report - evaluation result
 * Output
    - String of JSON
*/
pub(crate) fn render_json(report: &Report) -> String {
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

/** Decide who can repair a violation, by returning agent for source_changed violations, pending
 * targets, or any message about the worktree, and human for everything else (policy, checkpoint,
 * session, and setup failures that require editing .crane)
 * Input
    - violation: &Violation - violation to classify
 * Output
    - &'static str, "agent" or "human"
*/
pub(crate) fn repair_owner(violation: &Violation) -> &'static str {
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
pub(crate) fn classify_violation(message: &str) -> String {
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
