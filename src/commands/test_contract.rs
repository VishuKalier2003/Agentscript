use std::collections::BTreeSet;

use crate::contract_tests::{
    agent_authored, generate, ordinary_tests, render, report, run_contract_tests,
};
use crate::effects::{within, workspace_of};
use crate::ir::compile;
use crate::repository::{ensure_initialized, git};
use crate::session::ContractSession;

/** Usage of crane test-contract */
const USAGE: &str = "crane test-contract [--json] [--plan] [--session ID] [--no-ordinary]";

/** Run the contract tests generated from the active contracts (or a session's bound contract,
 * inside its worktree), then the ordinary tests, and report them separately
 * Input
    - args: &[String] - --json, --plan (generate only), --session ID, --no-ordinary
 * Output
    - Result<(), String>
    - Error if an option is unknown, the session does not exist, or any test failed
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    let (mut json, mut plan, mut ordinary, mut session) = (false, false, true, None);
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--json" => json = true,
            "--plan" => plan = true,
            "--no-ordinary" => ordinary = false,
            "--session" => {
                session = Some(
                    arguments
                        .next()
                        .ok_or_else(|| format!("--session needs a session id; use '{USAGE}'"))?
                        .clone(),
                )
            }
            other => {
                return Err(format!(
                    "unknown test-contract option '{other}'; use '{USAGE}'"
                ))
            }
        }
    }
    let bound = match &session {
        Some(id) => {
            Some(ContractSession::load(id)?.ok_or_else(|| format!("no contract session '{id}'"))?)
        }
        None => None,
    };
    let set = match &bound {
        Some(session) => session.contracts().clone(),
        None => compile()?,
    };
    let source = session
        .as_ref()
        .map_or("repository".to_string(), |id| format!("session {id}"));
    let work = || -> Result<_, String> {
        if plan {
            let tests = generate(&set);
            return Ok((tests, None));
        }
        let repository = git(&["rev-parse", "--show-toplevel"])?;
        let agent = agent_authored(&repository)?;
        let tests = run_contract_tests(&set, &agent);
        let checkpoints = set
            .contracts
            .iter()
            .filter_map(|contract| contract.checkpoint_sha.clone().ok())
            .collect::<BTreeSet<_>>();
        let ordinary = if ordinary {
            Some(ordinary_tests(&checkpoints, &agent)?)
        } else {
            None
        };
        Ok((tests, ordinary))
    };
    let (tests, ordinary) = match bound.as_ref().and_then(workspace_of) {
        Some(directory) => within(&directory, work)?,
        None => work()?,
    };
    let report = report(&source, &set, &tests, ordinary);
    if let Some(session) = bound
        .as_ref()
        .filter(|_| !plan && report["status"] == "passed")
    {
        crate::budget::manage::regenerate(session, "contract_tests_passed", "")?;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
    } else {
        print!("{}", render(&report));
    }
    if report["status"] == "failed" {
        Err("contract tests or ordinary tests failed".into())
    } else {
        Ok(())
    }
}
