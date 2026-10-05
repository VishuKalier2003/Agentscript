// Delivery: connect a validated session to a merged change. After the session reconciles (it is
// finalized with a PASS attestation), Crane puts its changes on a branch, runs the final contract
// tests, the repository's tests, and the configured lint and security checks on that branch,
// opens or updates the pull request with the contract summary and attestation, and notifies Slack.
// Merging waits for the organization's merge policy (approvals per autonomy and zone criticality,
// with scoped, temporary, audited exceptions); after a verified merge Crane records the merge
// commit, makes it a trusted checkpoint, completes the task (and only then its Jira or Asana
// issue), and re-finalizes the session attestation. Every delivery step is an event in an
// append-only, hash-chained delivery journal; nothing here changes a permanent policy.

pub(crate) mod policy;
pub(crate) mod slack;

#[cfg(test)] // Compile the module only when running tests, not in production builds
mod tests;

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use self::policy::{evaluate, DeliveryConfig, Exception, Facts, CONTRACT_TESTS, REPOSITORY_TESTS};
use crate::autonomy::manage::apply;
use crate::autonomy::{Actor, Trigger, SELF_ESCALATION};
use crate::evidence::{attest, link, GENESIS};
use crate::proposals::store::agent_environment;
use crate::repository::root;
use crate::session::{ContractSession, Lifecycle};
use crate::util::{io_error, now_unix, sha256};
use crate::zones::model::{Autonomy, Criticality};

/** Version of the delivery journal layout */
pub(crate) const DELIVERY_FORMAT: u64 = 1;

/** Return a delivery's directory
 * Input
    - id: &str - delivery id (the session id)
 * Output
    - Result<PathBuf, String>
*/
fn directory(id: &str) -> Result<PathBuf, String> {
    Ok(root()?.join("runtime").join("delivery").join(id))
}

/** Run git in a directory
 * Input
    - directory: &Path - working directory
    - args: &[&str] - arguments
 * Output
    - Result<String, String> trimmed stdout
    - Error with stderr when git fails
*/
fn git_in(directory: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/** Read a delivery's journal
 * Input
    - id: &str - delivery id
 * Output
    - Result<Vec<Value>, String>
*/
pub(crate) fn journal(id: &str) -> Result<Vec<Value>, String> {
    match fs::read_to_string(directory(id)?.join("journal.jsonl")) {
        Ok(text) => text
            .lines()
            .map(|line| {
                serde_json::from_str(line)
                    .map_err(|error| format!("delivery journal of {id}: {error}"))
            })
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(io_error(error)),
    }
}

/** Append an event to a delivery's journal, sequenced, timed, and chained like session journals
 * Input
    - id: &str - delivery id
    - event: Value - event fields (with "kind")
 * Output
    - Result<Value, String> the event as written
*/
fn record(id: &str, mut event: Value) -> Result<Value, String> {
    let directory = directory(id)?;
    fs::create_dir_all(&directory).map_err(io_error)?;
    let events = journal(id)?;
    let previous = events
        .last()
        .and_then(|last| last["chain"].as_str().map(String::from))
        .unwrap_or_else(|| GENESIS.to_string());
    event["delivery_format"] = json!(DELIVERY_FORMAT);
    event["delivery_id"] = json!(id);
    event["seq"] = json!(events.len() + 1);
    event["at"] = json!(now_unix());
    event["chain"] = json!(link(&previous, &event));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("journal.jsonl"))
        .map_err(io_error)?;
    writeln!(file, "{event}").map_err(io_error)?;
    Ok(event)
}

/** Replay a delivery's journal into its current state: the latest round (head, checks, tests,
 * change, pull request), the approvals, rejections, change requests, and exceptions, the merge,
 * and the task completion
 * Input
    - events: &[Value] - delivery journal
 * Output
    - Value
*/
pub(crate) fn state(events: &[Value]) -> Value {
    let mut state = json!({"rounds": 0, "approvals": [], "blocks": [], "exceptions": [], "unauthorized": [], "merged": null, "task_completion": null, "pull_request": null});
    for event in events {
        let entry = |event: &Value| {
            let mut entry = event.clone();
            if let Some(object) = entry.as_object_mut() {
                for key in ["delivery_format", "delivery_id", "chain"] {
                    object.remove(key);
                }
            }
            entry
        };
        match event["kind"].as_str() {
            Some("submitted") => {
                for key in [
                    "session",
                    "task",
                    "branch",
                    "base",
                    "head",
                    "files",
                    "criticality",
                    "zones",
                    "autonomy",
                    "safety",
                    "decision",
                    "attestation_digest",
                    "contract_version",
                    "contract_tests",
                    "checks",
                ] {
                    state[key] = event[key].clone();
                }
                state["rounds"] = json!(state["rounds"].as_u64().unwrap_or(0) + 1);
                state["delivery_id"] = event["delivery_id"].clone();
            }
            Some("pull_request") => state["pull_request"] = entry(event),
            Some("approval") => state["approvals"]
                .as_array_mut()
                .unwrap()
                .push(entry(event)),
            Some("rejection" | "changes_requested") => {
                state["blocks"].as_array_mut().unwrap().push(entry(event))
            }
            Some("exception") => state["exceptions"]
                .as_array_mut()
                .unwrap()
                .push(entry(event)),
            Some("unauthorized_action") => state["unauthorized"]
                .as_array_mut()
                .unwrap()
                .push(entry(event)),
            Some("merged") => state["merged"] = entry(event),
            Some("task_completed") => state["task_completion"] = entry(event),
            Some("attestation_finalized") => state["attestation_digest"] = event["digest"].clone(),
            _ => {}
        }
    }
    state["events"] = json!(events.len());
    // Delivery journals chain the same way session journals do
    state["chain"] = crate::evidence::verify_chain(events);
    state
}

/** Gather the facts merge eligibility needs from a delivery's state
 * Input
    - state: &Value - delivery state
    - now: u64 - Unix seconds
 * Output
    - Result<Facts, String>
*/
fn facts(state: &Value, now: u64) -> Result<Facts, String> {
    let branch_head = state["branch"].as_str().and_then(|branch| {
        git_in(
            &root_directory().ok()?,
            &["rev-parse", &format!("refs/heads/{branch}")],
        )
        .ok()
    });
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    Ok(Facts {
        session: text(&state["session"]),
        decision: text(&state["decision"]),
        autonomy: Autonomy::parse(state["autonomy"].as_str().unwrap_or("observe"))?,
        safety: text(&state["safety"]),
        criticality: Criticality::parse(state["criticality"].as_str().unwrap_or("restricted"))?,
        contract_failed: state["contract_tests"]["failed"].as_u64().unwrap_or(1),
        checks: state["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|check| (text(&check["name"]), text(&check["status"])))
            .collect(),
        head: text(&state["head"]),
        branch_head,
        approvals: state["approvals"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|approval| (text(&approval["by"]), text(&approval["head"])))
            .collect(),
        blocks: state["blocks"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|block| {
                (
                    text(&block["kind"]),
                    text(&block["by"]),
                    text(&block["head"]),
                )
            })
            .collect(),
        exceptions: state["exceptions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|exception| Exception {
                id: text(&exception["id"]),
                check: text(&exception["check"]),
                session: text(&exception["session"]),
                head: text(&exception["head"]),
                approver: text(&exception["by"]),
                reason: text(&exception["reason"]),
                expires_at: exception["expires_at"].as_u64().unwrap_or(0),
            })
            .collect(),
        merged: !state["merged"].is_null(),
        now,
    })
}

/** Return the repository root (the directory holding .crane)
 * Input
    - None
 * Output
    - Result<PathBuf, String>
*/
fn root_directory() -> Result<PathBuf, String> {
    root()?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "the repository root cannot be found".to_string())
}

/** Refuse a delivery action in an agent environment, recording the attempt on the session as a
 * critical self-escalation
 * Input
    - session: Option<&ContractSession> - the delivery's session, if it exists
    - what: &str - the attempted action
 * Output
    - Result<(), String>
*/
fn refuse_agents(session: Option<&ContractSession>, what: &str) -> Result<(), String> {
    let Some(marker) = agent_environment() else {
        return Ok(());
    };
    if let Some(session) = session {
        let _ = apply(
            session,
            Trigger::Violation(SELF_ESCALATION.into()),
            Actor::Crane,
            &format!("an agent tried to {what} its own delivery"),
        );
    }
    Err(format!("crane deliver {what} refuses to run in an agent environment ({marker} is set); a human or a trusted organization process delivers changes"))
}

/** Load a session for delivery
 * Input
    - id: &str - session id
 * Output
    - Result<ContractSession, String>
*/
fn session(id: &str) -> Result<ContractSession, String> {
    ContractSession::load(id)?.ok_or_else(|| format!("no contract session '{id}'"))
}

/** Write a message to the outbox, from which a forwarder posts it to Slack, Jira, or Asana
 * (Crane makes no outbound network calls itself)
 * Input
    - channel: &str - slack, jira, or asana
    - delivery: &str - delivery id
    - request: Value - the API request (method, path, body) or Slack message
 * Output
    - Result<String, String> the outbox file name
*/
fn outbox(channel: &str, delivery: &str, request: Value) -> Result<String, String> {
    let directory = root()?.join("runtime").join("delivery").join("outbox");
    fs::create_dir_all(&directory).map_err(io_error)?;
    let number = fs::read_dir(&directory).map_err(io_error)?.count() + 1;
    let name = format!("{number:05}-{channel}.json");
    fs::write(
        directory.join(&name),
        serde_json::to_string_pretty(&json!({"outbox_format": 1, "channel": channel, "delivery": delivery, "created_at": now_unix(), "request": request})).map_err(io_error)?,
    )
    .map_err(io_error)?;
    Ok(name)
}

/** Fill a link template
 * Input
    - template: Option<&String> - template with {number}, {branch}, {session}
    - fallback: String - link without a template
    - state: &Value - delivery state
 * Output
    - String
*/
fn link_for(template: Option<&String>, fallback: String, state: &Value) -> String {
    template.map_or(fallback, |template| {
        template
            .replace("{number}", &state["pull_request"]["number"].to_string())
            .replace("{branch}", state["branch"].as_str().unwrap_or_default())
            .replace("{session}", state["session"].as_str().unwrap_or_default())
    })
}

/** Notify every configured Slack target about the delivery's current round
 * Input
    - config: &DeliveryConfig - configuration
    - id: &str - delivery id
    - state: &Value - delivery state
    - eligibility: &Value - merge evaluation
 * Output
    - Result<(), String>
*/
fn announce(
    config: &DeliveryConfig,
    id: &str,
    state: &Value,
    eligibility: &Value,
) -> Result<(), String> {
    let pr_url = state["pull_request"]["url"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let contract_url = link_for(
        config.contract_url_template.as_ref(),
        format!("{pr_url}#contract"),
        state,
    );
    let exceptable = state["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|check| check["status"] != "passed" && check["status"] != "not_configured")
        .filter_map(|check| check["name"].as_str().map(String::from))
        .filter(|name| config.excepted_checks.contains(name))
        .collect::<Vec<_>>();
    for target in &config.notify {
        let file = outbox(
            "slack",
            id,
            slack::announcement(
                target,
                state,
                eligibility,
                &pr_url,
                &contract_url,
                &exceptable,
            ),
        )?;
        record(
            id,
            json!({"kind": "notified", "target": target, "outbox": file}),
        )?;
    }
    Ok(())
}

/** Post a status line to every configured Slack target
 * Input
    - config: &DeliveryConfig - configuration
    - id: &str - delivery id
    - text: &str - message
 * Output
    - Result<(), String>
*/
fn tell(config: &DeliveryConfig, id: &str, text: &str) -> Result<(), String> {
    for target in &config.notify {
        let file = outbox("slack", id, slack::message(target, text))?;
        record(
            id,
            json!({"kind": "notified", "target": target, "outbox": file}),
        )?;
    }
    Ok(())
}

/** Render the pull request body: the task, the contract and its tests, the checks, the
 * attestation, and the merge policy that applies
 * Input
    - session: &ContractSession - session
    - round: &Value - the submitted round
    - attestation: &Value - the session attestation
    - eligibility: &Value - merge evaluation
 * Output
    - String Markdown
*/
fn body(
    session: &ContractSession,
    round: &Value,
    attestation: &Value,
    eligibility: &Value,
) -> String {
    let mut out = format!(
        "Delivered by Crane from contract session `{}`{}.\n\n## Contract\n\nContract version `{}`\n\n",
        session.id(),
        round["task"].as_str().map_or(String::new(), |task| format!(" for task {task}")),
        session.contracts().version
    );
    for (_, contract, clause) in session.contracts().clauses() {
        out.push_str(&format!(
            "- `{}`: {} {} `{}` (scope {}) at checkpoint `{}`\n",
            contract.policy_id,
            clause.keyword(),
            clause.kind.noun(),
            clause.target,
            clause.scope.name(),
            contract.checkpoint
        ));
    }
    out.push_str(&format!(
        "\n## Contract tests\n\n{} passed, {} failed, {} not applicable (decided by Crane, never by test files)\n\n## Checks\n\n",
        round["contract_tests"]["passed"], round["contract_tests"]["failed"], round["contract_tests"]["not_applicable"]
    ));
    for check in round["checks"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "- {} ({}): {}\n",
            check["name"].as_str().unwrap_or_default(),
            check["kind"].as_str().unwrap_or_default(),
            check["status"].as_str().unwrap_or_default()
        ));
    }
    out.push_str(&format!(
        "\n## Attestation\n\n- digest `{}`\n- final decision {}\n- {} actions authorized, {} denied, {} violations, {} repairs\n- change criticality {} by a {} session\n\n## Merge policy\n\nRule `{}`: {} approval(s){}{}.\n",
        attestation["attestation_digest"].as_str().unwrap_or_default(),
        attestation["final_decision"]["decision"].as_str().unwrap_or_default(),
        attestation["action_summary"]["authorizations"],
        attestation["denied_actions"].as_array().map_or(0, Vec::len),
        attestation["violations"].as_array().map_or(0, Vec::len),
        attestation["repairs"].as_array().map_or(0, Vec::len),
        round["criticality"].as_str().unwrap_or_default(),
        round["autonomy"].as_str().unwrap_or_default(),
        eligibility["rule"]["name"].as_str().unwrap_or_default(),
        eligibility["approvals"]["required"],
        eligibility["rule"]["approvers"].as_array().filter(|approvers| !approvers.is_empty()).map_or(String::new(), |approvers| format!(" from {}", approvers.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))),
        if eligibility["rule"]["auto_merge"] == true { ", merged automatically once eligible" } else { "" },
    ));
    out
}

/** Put the session's changes on its delivery branch and return where to run the checks: an
 * isolated session commits in its worktree on its own branch; otherwise the changes move from the
 * repository's base branch onto a new branch, are committed there, and the base is checked out
 * again after the checks
 * Input
    - session: &ContractSession - session
    - config: &DeliveryConfig - configuration
    - files: &[String] - files the session changed
 * Output
    - Result<(PathBuf, String, String, bool), String> working directory, branch, base, and
      whether the base must be checked out again afterwards
*/
fn prepare_branch(
    session: &ContractSession,
    config: &DeliveryConfig,
    files: &[String],
) -> Result<(PathBuf, String, String, bool), String> {
    let repository = root_directory()?;
    let current = git_in(&repository, &["branch", "--show-current"])?;
    let base = config
        .base_branch
        .clone()
        .unwrap_or_else(|| current.clone());
    let (directory, branch, restore) = match crate::effects::workspace_of(session) {
        Some(worktree) => {
            let branch = git_in(&worktree, &["branch", "--show-current"])?;
            (worktree, branch, false)
        }
        None => {
            let branch = format!("{}{}", config.branch_prefix, session.id());
            if current == base {
                let exists = git_in(
                    &repository,
                    &[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("refs/heads/{branch}"),
                    ],
                )
                .is_ok();
                git_in(
                    &repository,
                    &if exists {
                        vec!["switch", branch.as_str()]
                    } else {
                        vec!["switch", "-c", branch.as_str()]
                    },
                )?;
            } else if current != branch {
                return Err(format!("check out {base} (or {branch}) before delivering; the repository is on {current}"));
            }
            (repository, branch, true)
        }
    };
    for file in files {
        git_in(&directory, &["add", "-A", "--", file])?;
    }
    let staged = git_in(&directory, &["diff", "--cached", "--name-only"])?;
    if !staged.is_empty() {
        let message = format!(
            "{}{}\n\nDelivered by Crane from contract session {} (contract {}).",
            session
                .identity()
                .1
                .map_or(String::new(), |task| format!("{task}: ")),
            session
                .governance()
                .task_title
                .clone()
                .unwrap_or_else(|| "agent change".into()),
            session.id(),
            session.contracts().version
        );
        git_in(&directory, &["commit", "-q", "-m", &message])?;
    }
    Ok((directory, branch, base, restore))
}

/** Open or update the pull request: the local provider keeps the record and body in the delivery
 * directory and merges with git; the github provider pushes the branch and uses the gh CLI
 * Input
    - config: &DeliveryConfig - configuration
    - id: &str - delivery id
    - state: &Value - delivery state (with the new round)
    - title: &str - title
    - text: &str - body
 * Output
    - Result<Value, String> {action, number, url, title, body_digest, provider}
*/
fn open_pull_request(
    config: &DeliveryConfig,
    id: &str,
    state: &Value,
    title: &str,
    text: &str,
) -> Result<Value, String> {
    let file = directory(id)?.join("pull_request.md");
    fs::write(&file, text).map_err(io_error)?;
    let existing = &state["pull_request"];
    let branch = state["branch"].as_str().unwrap_or_default();
    let base = state["base"].as_str().unwrap_or_default();
    let (number, url) = match config.provider.as_str() {
        "github" => {
            let repository = root_directory()?;
            git_in(&repository, &["push", "-u", "origin", branch])?;
            let path = file.to_string_lossy().to_string();
            if let Some(number) = existing["number"].as_u64() {
                gh(&[
                    "pr",
                    "edit",
                    &number.to_string(),
                    "--title",
                    title,
                    "--body-file",
                    &path,
                ])?;
                (
                    number,
                    existing["url"].as_str().unwrap_or_default().to_string(),
                )
            } else {
                let url = gh(&[
                    "pr",
                    "create",
                    "--base",
                    base,
                    "--head",
                    branch,
                    "--title",
                    title,
                    "--body-file",
                    &path,
                ])?;
                let number = url
                    .rsplit('/')
                    .next()
                    .and_then(|number| number.trim().parse::<u64>().ok())
                    .ok_or_else(|| format!("gh did not return a pull request URL: {url}"))?;
                (number, url.trim().to_string())
            }
        }
        _ => {
            let number = match existing["number"].as_u64() {
                Some(number) => number,
                None => {
                    // Local numbers count the deliveries that opened a pull request
                    let deliveries = root()?.join("runtime").join("delivery");
                    fs::read_dir(&deliveries)
                        .map_err(io_error)?
                        .filter_map(|entry| entry.ok())
                        .filter(|entry| entry.path().join("pull_request.md").exists())
                        .count() as u64
                }
            };
            let fallback = format!("local://{}/pull/{number}", branch);
            (
                number,
                config
                    .pr_url_template
                    .as_ref()
                    .map_or(fallback, |template| {
                        template
                            .replace("{number}", &number.to_string())
                            .replace("{branch}", branch)
                    }),
            )
        }
    };
    Ok(json!({
        "action": if existing.is_null() { "opened" } else { "updated" },
        "provider": config.provider,
        "number": number,
        "url": url,
        "title": title,
        "branch": branch,
        "base": base,
        "body_digest": sha256(text.as_bytes()),
    }))
}

/** Run the gh CLI
 * Input
    - args: &[&str] - arguments
 * Output
    - Result<String, String> stdout
*/
fn gh(args: &[&str]) -> Result<String, String> {
    let output = Command::new("gh")
        .args(args)
        .current_dir(root_directory()?)
        .output()
        .map_err(|error| format!("gh: {error} (the github provider needs the GitHub CLI)"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/** Deliver a session: require its reconciliation (finalizing it when needed, which runs the
 * mandatory contract tests), put its changes on a branch, run the final contract tests, the
 * repository tests, and the configured checks there, open or update the pull request with the
 * contract summary and attestation, notify Slack, and merge at once when the merge policy allows
 * autonomous merge
 * Input
    - id: &str - session id
 * Output
    - Result<Value, String> the delivery status
*/
pub(crate) fn run(id: &str) -> Result<Value, String> {
    let session = session(id)?;
    refuse_agents(Some(&session), "run")?;
    let config = policy::load()?;
    if session.lifecycle() != Lifecycle::Finalized {
        crate::agent_session::finalize(id)?;
    }
    let session = self::session(id)?;
    let attestation = attest(&session.document(), &session.events())?;
    let decision = attestation["final_decision"]["decision"]
        .as_str()
        .unwrap_or("UNKNOWN")
        .to_string();
    if decision != "PASS" {
        record(
            id,
            json!({"kind": "blocked", "reason": format!("the session attestation is {decision}")}),
        )?;
        return Err(format!(
            "contract session {id} did not reconcile ({decision}); nothing is delivered"
        ));
    }
    let files = attestation["action_summary"]["files_changed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|file| !file.starts_with(".crane/"))
        .map(String::from)
        .collect::<Vec<_>>();
    let governance = session.governance();
    let criticality = files
        .iter()
        .map(|file| {
            governance
                .constraint(file)
                .map_or(Criticality::Routine, |constraint| constraint.criticality)
        })
        .max()
        .unwrap_or(Criticality::Routine);
    let zones = files
        .iter()
        .filter_map(|file| governance.constraint(file))
        .flat_map(|constraint| constraint.zones)
        .collect::<BTreeSet<_>>();
    let (workdir, branch, base, restore) = prepare_branch(&session, &config, &files)?;
    let head = git_in(&workdir, &["rev-parse", "HEAD"])?;
    // Final contract tests, repository tests, and checks, all on the delivery branch
    let results = crate::effects::within(&workdir, || {
        let repository = crate::repository::git(&["rev-parse", "--show-toplevel"])?;
        let agent = crate::contract_tests::agent_authored(&repository)?;
        let contract = crate::contract_tests::run_contract_tests(session.contracts(), &agent);
        let checkpoints = session
            .contracts()
            .contracts
            .iter()
            .filter_map(|contract| contract.checkpoint_sha.clone().ok())
            .collect::<BTreeSet<_>>();
        let ordinary = crate::contract_tests::ordinary_tests(&checkpoints, &agent)?;
        let checks = config
            .checks
            .iter()
            .map(|check| {
                let result = crate::effects::run(&workdir, &check.command, check.timeout);
                json!({"name": check.name, "kind": check.kind, "status": result["status"], "output_digest": sha256(result["output"].as_str().unwrap_or_default().as_bytes())})
            })
            .collect::<Vec<_>>();
        Ok((contract, ordinary, checks))
    });
    if restore {
        git_in(&root_directory()?, &["switch", &base])?;
    }
    let (contract, ordinary, mut checks) = results?;
    let summary = crate::contract_tests::summary(&contract);
    let repository_tests = if ordinary["organizational"]
        .as_array()
        .is_some_and(|results| !results.is_empty())
        || ordinary["agent_authored"]
            .as_array()
            .is_some_and(|results| !results.is_empty())
    {
        ordinary["status"].as_str().unwrap_or("failed").to_string()
    } else {
        "not_configured".to_string()
    };
    checks.insert(0, json!({"name": REPOSITORY_TESTS, "kind": "tests", "status": repository_tests, "output_digest": sha256(ordinary.to_string().as_bytes())}));
    let final_state = &attestation["final_state"];
    record(
        id,
        json!({
            "kind": "submitted",
            "session": id,
            "task": session.identity().1,
            "branch": branch,
            "base": base,
            "head": head,
            "files": files,
            "criticality": criticality.name(),
            "zones": zones,
            "autonomy": final_state["autonomy"],
            "safety": final_state["safety"],
            "decision": decision,
            "attestation_digest": attestation["attestation_digest"],
            "contract_version": session.contracts().version,
            "contract_tests": {"passed": summary["passed"], "failed": summary["failed"], "not_applicable": summary["not_applicable"], "check": CONTRACT_TESTS},
            "checks": checks,
        }),
    )?;
    let mut current = state(&journal(id)?);
    let eligibility = evaluate(&config, &facts(&current, now_unix())?);
    let title = format!(
        "{}{}",
        session
            .identity()
            .1
            .map_or(String::new(), |task| format!("{task}: ")),
        governance
            .task_title
            .clone()
            .unwrap_or_else(|| format!("Changes from contract session {id}"))
    );
    let pull_request = open_pull_request(
        &config,
        id,
        &current,
        &title,
        &body(&session, &current, &attestation, &eligibility),
    )?;
    let mut event = pull_request.clone();
    event["kind"] = json!("pull_request");
    record(id, event)?;
    current = state(&journal(id)?);
    session.record(json!({"event": "delivery_submitted", "branch": current["branch"], "head": current["head"], "pull_request": {"number": pull_request["number"], "url": pull_request["url"]}, "checks": current["checks"], "contract_tests": current["contract_tests"], "criticality": current["criticality"], "rule": eligibility["rule"]["name"]}))?;
    if let Some(task) = session.identity().1 {
        crate::orchestration::delivery_progress(task, None)?;
    }
    announce(&config, id, &current, &eligibility)?;
    if eligibility["auto_merge"] == true {
        return merge(id, "crane (auto-merge policy)");
    }
    status(id)
}

/** Report a delivery: its state, its merge evaluation, and its chain
 * Input
    - id: &str - delivery id
 * Output
    - Result<Value, String>
*/
pub(crate) fn status(id: &str) -> Result<Value, String> {
    let events = journal(id)?;
    if events.is_empty() {
        return Err(format!(
            "no delivery for '{id}'; run 'crane deliver run {id}' first"
        ));
    }
    let config = policy::load()?;
    let mut current = state(&events);
    current["eligibility"] = if current["head"].is_null() {
        Value::Null
    } else {
        evaluate(&config, &facts(&current, now_unix())?)
    };
    Ok(current)
}

/** Resolve who acts: a CLI human by name, or a Slack user mapped to an approver
 * Input
    - config: &DeliveryConfig - configuration
    - by: &str - name, or "slack:USER_ID"
 * Output
    - Option<String> the approver name, None for an unmapped Slack user
*/
fn actor(config: &DeliveryConfig, by: &str) -> Option<String> {
    match by.strip_prefix("slack:") {
        Some(user) => config.slack_users.get(user).cloned(),
        None => Some(by.to_string()),
    }
}

/** Record a human decision on the delivery's current round (approve, reject, or request
 * changes), bound to the commit the checks ran on; an approval that completes the merge policy of
 * an auto-merge rule merges
 * Input
    - id: &str - delivery id
    - kind: &str - approval, rejection, or changes_requested
    - by: &str - approver name (CLI) or "slack:USER_ID"
    - head: Option<&str> - the commit the decision is about (Slack buttons carry it)
    - reason: &str - why
    - via: &str - cli or slack
 * Output
    - Result<Value, String> the delivery status
*/
pub(crate) fn decide(
    id: &str,
    kind: &str,
    by: &str,
    head: Option<&str>,
    reason: &str,
    via: &str,
) -> Result<Value, String> {
    let session = session(id)?;
    refuse_agents(Some(&session), kind)?;
    let config = policy::load()?;
    let current = status(id)?;
    if !current["merged"].is_null() {
        return Err(format!("delivery {id} is already merged"));
    }
    let Some(name) = actor(&config, by) else {
        record(
            id,
            json!({"kind": "unauthorized_action", "by": by, "action": kind, "via": via}),
        )?;
        return Err(format!(
            "{by} is not mapped to an approver in .crane/delivery.json; the {kind} was not counted"
        ));
    };
    let round = current["head"].as_str().unwrap_or_default();
    if head.is_some_and(|head| head != round) {
        record(
            id,
            json!({"kind": "unauthorized_action", "by": name, "action": kind, "via": via, "reason": "stale commit"}),
        )?;
        return Err(format!(
            "that message is about an earlier commit; the delivery is now at {round}"
        ));
    }
    record(
        id,
        json!({"kind": kind, "by": name, "head": round, "reason": reason, "via": via}),
    )?;
    session.record(json!({"event": format!("delivery_{kind}"), "by": name, "head": round, "reason": reason, "via": via}))?;
    let after = status(id)?;
    let text = match kind {
        "approval" => format!("{name} approved delivery {id}."),
        "rejection" => format!("{name} rejected delivery {id}: {reason}"),
        _ => format!("{name} requested changes on delivery {id}: {reason}"),
    };
    tell(&config, id, &text)?;
    if kind == "approval" && after["eligibility"]["auto_merge"] == true {
        return merge(id, &format!("crane (auto-merge after {name}'s approval)"));
    }
    Ok(after)
}

/** Grant a scoped exception to one failing check of the current round: only checks the
 * configuration lets humans except (never the contract tests), bound to this session and commit,
 * for at most the configured duration, by an approver of the covering rule; it changes no policy
 * Input
    - id: &str - delivery id
    - check: &str - check name
    - by: &str - approver name (CLI) or "slack:USER_ID"
    - head: Option<&str> - the commit (Slack buttons carry it)
    - reason: &str - why
    - duration: Option<u64> - seconds (default: the configured default)
    - via: &str - cli or slack
 * Output
    - Result<Value, String> the delivery status
*/
pub(crate) fn except(
    id: &str,
    check: &str,
    by: &str,
    head: Option<&str>,
    reason: &str,
    duration: Option<u64>,
    via: &str,
) -> Result<Value, String> {
    let session = session(id)?;
    refuse_agents(Some(&session), "exception")?;
    let config = policy::load()?;
    let current = status(id)?;
    if check == CONTRACT_TESTS {
        return Err("contract tests can never be excepted".into());
    }
    if !config.excepted_checks.contains(check) {
        return Err(format!("check '{check}' cannot be excepted; .crane/delivery.json exceptions.allowed_checks does not list it"));
    }
    let failing = current["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|entry| entry["name"] == check && entry["status"] != "passed");
    if !failing {
        return Err(format!(
            "check '{check}' is not failing in the current round"
        ));
    }
    let Some(name) = actor(&config, by) else {
        record(
            id,
            json!({"kind": "unauthorized_action", "by": by, "action": "exception", "via": via}),
        )?;
        return Err(format!(
            "{by} is not mapped to an approver; no exception was granted"
        ));
    };
    let approvers = current["eligibility"]["rule"]["approvers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !approvers.is_empty() && !approvers.iter().any(|approver| approver == name.as_str()) {
        return Err(format!(
            "{name} is not an approver of rule {}",
            current["eligibility"]["rule"]["name"]
        ));
    }
    let round = current["head"].as_str().unwrap_or_default().to_string();
    if head.is_some_and(|head| head != round) {
        return Err(format!(
            "that message is about an earlier commit; the delivery is now at {round}"
        ));
    }
    if reason.trim().is_empty() {
        return Err("an exception needs a reason".into());
    }
    let duration = duration.unwrap_or(config.default_exception);
    if duration == 0 || duration > config.max_exception {
        return Err(format!(
            "an exception lasts between 1 and {} seconds",
            config.max_exception
        ));
    }
    let expires_at = now_unix() + duration;
    let exception_id = sha256(format!("{id}:{check}:{round}:{name}:{expires_at}").as_bytes())
        .trim_start_matches("sha256:")
        .chars()
        .take(12)
        .collect::<String>();
    record(
        id,
        json!({"kind": "exception", "id": exception_id, "check": check, "session": id, "head": round, "by": name, "reason": reason, "expires_at": expires_at, "via": via, "scope": format!("check {check} of delivery {id} at {round}")}),
    )?;
    session.record(json!({"event": "delivery_exception", "id": exception_id, "check": check, "head": round, "by": name, "reason": reason, "expires_at": expires_at, "via": via}))?;
    tell(&config, id, &format!("{name} approved a scoped exception for {check} on delivery {id} (until {expires_at}): {reason}"))?;
    let after = status(id)?;
    if after["eligibility"]["auto_merge"] == true {
        return merge(id, &format!("crane (auto-merge after {name}'s exception)"));
    }
    Ok(after)
}

/** Handle a signed Slack interaction: verify it came from Slack, then carry out the click
 * (viewing is only recorded; approve, reject, request changes, and exceptions go through the same
 * rules as the CLI, by the approver the Slack user is mapped to)
 * Input
    - body: &str - raw request body
    - timestamp: &str - X-Slack-Request-Timestamp
    - signature: &str - X-Slack-Signature
 * Output
    - Result<Value, String> the delivery status
*/
pub(crate) fn slack_action(body: &str, timestamp: &str, signature: &str) -> Result<Value, String> {
    if let Some(marker) = agent_environment() {
        return Err(format!(
            "Slack actions are not accepted in an agent environment ({marker} is set)"
        ));
    }
    let config = policy::load()?;
    let secret = std::env::var(&config.signing_secret_env)
        .ok()
        .filter(|secret| !secret.is_empty())
        .ok_or_else(|| {
            format!(
                "{} is not set; Slack actions cannot be verified",
                config.signing_secret_env
            )
        })?;
    slack::verify(&secret, timestamp, body, signature, now_unix())?;
    let click = slack::parse(body)?;
    let by = format!("slack:{}", click.user);
    let reason = format!("via Slack by {}", click.user);
    match click.action.as_str() {
        "view_pr" | "view_contract" => {
            record(
                &click.delivery,
                json!({"kind": "viewed", "by": by, "what": click.action}),
            )?;
            status(&click.delivery)
        }
        "approve" => decide(
            &click.delivery,
            "approval",
            &by,
            Some(&click.head),
            &reason,
            "slack",
        ),
        "reject" => decide(
            &click.delivery,
            "rejection",
            &by,
            Some(&click.head),
            &reason,
            "slack",
        ),
        "request_changes" => decide(
            &click.delivery,
            "changes_requested",
            &by,
            Some(&click.head),
            &reason,
            "slack",
        ),
        _ => {
            let check = click.check.ok_or("the exception button names no check")?;
            except(
                &click.delivery,
                &check,
                &by,
                Some(&click.head),
                &format!("approved in Slack by {}", click.user),
                None,
                "slack",
            )
        }
    }
}

/** Merge a delivery once the merge policy allows it, then establish the new trusted state: verify
 * that the merge commit contains the checked commit, record it, make it a trusted checkpoint,
 * complete the task (and only then its Jira or Asana issue), and re-finalize the session
 * attestation with the delivery
 * Input
    - id: &str - delivery id
    - by: &str - who merges
 * Output
    - Result<Value, String> the delivery status
*/
pub(crate) fn merge(id: &str, by: &str) -> Result<Value, String> {
    let session = session(id)?;
    refuse_agents(Some(&session), "merge")?;
    let config = policy::load()?;
    let current = status(id)?;
    let eligibility = &current["eligibility"];
    if eligibility["eligible"] != true {
        return Err(format!(
            "delivery {id} is not eligible to merge: {}",
            eligibility["missing"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let repository = root_directory()?;
    let branch = current["branch"].as_str().unwrap_or_default();
    let base = current["base"].as_str().unwrap_or_default();
    let head = current["head"].as_str().unwrap_or_default();
    let merge_sha = match config.provider.as_str() {
        "github" => {
            let number = current["pull_request"]["number"].to_string();
            gh(&["pr", "merge", &number, "--merge"])?;
            git_in(&repository, &["fetch", "origin", base])?;
            git_in(&repository, &["rev-parse", &format!("origin/{base}")])?
        }
        _ => {
            if git_in(&repository, &["branch", "--show-current"])? != base {
                return Err(format!("check out {base} to merge delivery {id}"));
            }
            let dirty = git_in(
                &repository,
                &["status", "--porcelain", "--untracked-files=no"],
            )?;
            if dirty.lines().any(|line| !line[3..].starts_with(".crane/")) {
                return Err(format!(
                    "{base} has uncommitted changes; commit or stash them before merging"
                ));
            }
            let title = current["pull_request"]["title"]
                .as_str()
                .unwrap_or_default();
            let number = &current["pull_request"]["number"];
            if let Err(error) = git_in(
                &repository,
                &[
                    "merge",
                    "--no-ff",
                    "--no-edit",
                    "-m",
                    &format!("Merge pull request #{number} from {branch}\n\n{title}"),
                    branch,
                ],
            ) {
                let _ = git_in(&repository, &["merge", "--abort"]);
                return Err(format!("the merge failed and was aborted: {error}"));
            }
            git_in(&repository, &["rev-parse", "HEAD"])?
        }
    };
    // A merge counts only when the merged history contains exactly the commit that was checked
    let verified = git_in(
        &repository,
        &["merge-base", "--is-ancestor", head, &merge_sha],
    )
    .is_ok();
    if !verified {
        record(
            id,
            json!({"kind": "blocked", "reason": format!("merge {merge_sha} does not contain the checked commit {head}")}),
        )?;
        return Err(format!("merge {merge_sha} does not contain the checked commit {head}; the task is not completed"));
    }
    let name = format!(
        "trusted_{}",
        id.chars()
            .map(|character| if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            })
            .collect::<String>()
    );
    let checkpoint = crate::commands::checkpoint::create_at(&name, &merge_sha, base)?;
    record(
        id,
        json!({"kind": "merged", "sha": merge_sha, "by": by, "head": head, "base": base, "verified": true, "trusted_checkpoint": checkpoint.name, "rule": eligibility["rule"]["name"], "approvals": eligibility["approvals"], "excepted": eligibility["excepted"]}),
    )?;
    let mut completion = Value::Null;
    if let Some(task) = session.identity().1 {
        if let Some(tracker) = crate::orchestration::delivery_progress(task, Some(&merge_sha))? {
            let messages = tracker_completion(&config, &tracker, task, &merge_sha, &current)?;
            completion = json!({"task": task, "tracker": tracker["source"], "external_id": tracker["external_id"], "outbox": messages});
        } else {
            completion = json!({"task": task, "tracker": null, "outbox": []});
        }
        record(
            id,
            json!({"kind": "task_completed", "task": task, "tracker": completion["tracker"], "external_id": completion["external_id"], "outbox": completion["outbox"]}),
        )?;
    }
    session.record(json!({"event": "delivery_merged", "merge_sha": merge_sha, "head": head, "by": by, "trusted_checkpoint": checkpoint.name, "pull_request": current["pull_request"]["number"], "rule": eligibility["rule"]["name"], "approvals": eligibility["approvals"]["by"], "excepted": eligibility["excepted"], "task_completion": completion}))?;
    let finalized = attest(&session.document(), &session.events())?;
    if let Some(directory) = session.directory() {
        fs::write(
            directory.join("final_attestation.json"),
            serde_json::to_string_pretty(&finalized).map_err(io_error)?,
        )
        .map_err(io_error)?;
    }
    record(
        id,
        json!({"kind": "attestation_finalized", "digest": finalized["attestation_digest"]}),
    )?;
    tell(
        &config,
        id,
        &format!(
            "Delivery {id} merged into {base} as {merge_sha}; trusted checkpoint {}.",
            checkpoint.name
        ),
    )?;
    status(id)
}

/** Queue the tracker completion for a verified merge: a comment with the merge and attestation,
 * then the transition to done (Jira) or completion (Asana)
 * Input
    - config: &DeliveryConfig - configuration
    - tracker: &Value - {source, external_id}
    - task: &str - task id
    - merge_sha: &str - verified merge commit
    - state: &Value - delivery state
 * Output
    - Result<Vec<String>, String> outbox files
*/
fn tracker_completion(
    config: &DeliveryConfig,
    tracker: &Value,
    task: &str,
    merge_sha: &str,
    state: &Value,
) -> Result<Vec<String>, String> {
    let external = tracker["external_id"].as_str().unwrap_or(task);
    let text = format!(
        "Merged by Crane as {merge_sha} ({}). Contract {} held; attestation {}.",
        state["pull_request"]["url"].as_str().unwrap_or_default(),
        state["contract_version"].as_str().unwrap_or_default(),
        state["attestation_digest"].as_str().unwrap_or_default()
    );
    let delivery = state["delivery_id"].as_str().unwrap_or_default();
    let requests = match tracker["source"].as_str() {
        Some("jira") => vec![
            json!({"method": "POST", "path": format!("/rest/api/3/issue/{external}/comment"), "body": {"body": {"type": "doc", "version": 1, "content": [{"type": "paragraph", "content": [{"type": "text", "text": text}]}]}}}),
            json!({"method": "POST", "path": format!("/rest/api/3/issue/{external}/transitions"), "body": {"transition": {"id": config.trackers["jira"]["done_transition"].as_str().unwrap_or("done")}}}),
        ],
        Some("asana") => vec![
            json!({"method": "POST", "path": format!("/tasks/{external}/stories"), "body": {"data": {"text": text}}}),
            json!({"method": "PUT", "path": format!("/tasks/{external}"), "body": {"data": {"completed": true}}}),
        ],
        _ => Vec::new(),
    };
    let channel = tracker["source"].as_str().unwrap_or("tracker");
    requests
        .into_iter()
        .map(|request| outbox(channel, delivery, request))
        .collect()
}

/** Render a delivery status for a terminal
 * Input
    - value: &Value - delivery status
 * Output
    - String
*/
pub(crate) fn render(value: &Value) -> String {
    let text = |value: &Value| {
        value.as_str().map_or_else(
            || {
                if value.is_null() {
                    "-".to_string()
                } else {
                    value.to_string()
                }
            },
            String::from,
        )
    };
    let eligibility = &value["eligibility"];
    let mut out = format!(
        "Delivery {} (round {})\n  pull request: #{} {} ({})\n  branch: {} -> {} at {}\n  change: {} files, criticality {}, session autonomy {} / safety {}\n  contract tests: {} passed, {} failed\n",
        text(&value["delivery_id"]),
        value["rounds"],
        text(&value["pull_request"]["number"]),
        text(&value["pull_request"]["url"]),
        text(&value["pull_request"]["action"]),
        text(&value["branch"]),
        text(&value["base"]),
        text(&value["head"]),
        value["files"].as_array().map_or(0, Vec::len),
        text(&value["criticality"]),
        text(&value["autonomy"]),
        text(&value["safety"]),
        value["contract_tests"]["passed"],
        value["contract_tests"]["failed"],
    );
    for check in value["checks"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  check {}: {}\n",
            text(&check["name"]),
            text(&check["status"])
        ));
    }
    if !value["merged"].is_null() {
        out.push_str(&format!(
            "  MERGED as {} by {}; trusted checkpoint {}\n",
            text(&value["merged"]["sha"]),
            text(&value["merged"]["by"]),
            text(&value["merged"]["trusted_checkpoint"])
        ));
    } else if !eligibility.is_null() {
        out.push_str(&format!(
            "  merge rule {}: {} of {} approvals ({})\n  {}\n",
            text(&eligibility["rule"]["name"]),
            eligibility["approvals"]["counted"],
            eligibility["approvals"]["required"],
            eligibility["approvals"]["by"]
                .as_array()
                .into_iter()
                .flatten()
                .map(text)
                .collect::<Vec<_>>()
                .join(", "),
            if eligibility["eligible"] == true {
                "ELIGIBLE to merge".to_string()
            } else {
                format!(
                    "waiting for: {}",
                    eligibility["missing"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(text)
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            }
        ));
    }
    for exception in value["exceptions"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  exception {} for {} by {} until {}\n",
            text(&exception["id"]),
            text(&exception["check"]),
            text(&exception["by"]),
            exception["expires_at"]
        ));
    }
    out.push_str(&format!(
        "  journal: {} events, chain {}\n",
        value["events"],
        text(&value["chain"]["status"])
    ));
    out
}
