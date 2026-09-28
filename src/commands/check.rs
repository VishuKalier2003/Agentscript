use crate::ir::compile;
use crate::repository::ensure_initialized;
use crate::scope::ScopeContext;
use crate::verify::{render_json, setup_report, verify_contracts, Report};

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
    let report = match ensure_initialized().and_then(|()| evaluate()) {
        Ok(report) => report,
        Err(error) => {
            if json || agent {
                print_error_json(&error);
            }
            return Err(error);
        }
    };
    if json || agent {
        println!("{}", render_json(&report));
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

/** Evaluate every policy on disk, by compiling .crane/policies into the contract IR and verifying
 * every clause with one shared scope context
 * Input
    - None
 * Output
    - Result<Report, String>
    - Error if .crane or the policies directory cannot be read
*/
pub(crate) fn evaluate() -> Result<Report, String> {
    let set = compile()?;
    Ok(verify_contracts(&set, &mut ScopeContext::new(), &|_| true))
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
    println!("{}", render_json(&report));
    eprintln!("{}", render_json(&report));
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
