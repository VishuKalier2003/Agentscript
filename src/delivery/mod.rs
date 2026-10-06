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
use crate::repo::providers::{named, PullRequest};
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
    let mut state = json!({"rounds": 0, "approvals": [], "blocks": [], "exceptions": [], "unauthorized": [], "merge_failures": [], "merged": null, "task_completion": null, "pull_request": null});
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
                    "tree",
                    "binding",
                    "binding_digest",
                    "verification",
                    "task_contract",
                    "checkpoint",
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
            Some("merge_failed") => state["merge_failures"]
                .as_array_mut()
                .unwrap()
                .push(entry(event)),
            Some("task_completion_queued") => state["task_completion"] = entry(event),
            Some("task_completed") => state["task_confirmed"] = entry(event),
            Some("attestation_finalized") => state["attestation_digest"] = event["digest"].clone(),
            _ => {}
        }
    }
    state["events"] = json!(events.len());
    // Delivery journals chain the same way session journals do
    state["chain"] = crate::evidence::verify_chain(events);
    state
}

/** Check whether a recorded decision is about the delivery's current round: the same binding
 * (commit, checks, contract, attestation), or for decisions recorded before bindings, the same
 * commit
 * Input
    - decision: &Value - approval, rejection, or change request
    - state: &Value - delivery state
 * Output
    - bool
*/
fn current_round(decision: &Value, state: &Value) -> bool {
    match decision["binding"].as_str() {
        Some(binding) => state["binding_digest"] == binding,
        None => decision["head"] == state["head"],
    }
}

/** Gather the facts merge eligibility needs from a delivery's state: approvals and blocks count
 * only for the current round (binding-bound), and approvals only within the configured lifetime
 * Input
    - state: &Value - delivery state
    - now: u64 - Unix seconds
    - approval_ttl: Option<u64> - seconds an approval counts
 * Output
    - Result<Facts, String>
*/
fn facts(state: &Value, now: u64, approval_ttl: Option<u64>) -> Result<Facts, String> {
    let branch_head = state["branch"].as_str().and_then(|branch| {
        git_in(
            &root_directory().ok()?,
            &["rev-parse", &format!("refs/heads/{branch}")],
        )
        .ok()
    });
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    let head = text(&state["head"]);
    let fresh = |approval: &Value| {
        approval_ttl.is_none_or(|ttl| approval["at"].as_u64().unwrap_or(0) + ttl > now)
    };
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
        head: head.clone(),
        branch_head,
        approvals: state["approvals"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|approval| current_round(approval, state) && fresh(approval))
            .map(|approval| (text(&approval["by"]), head.clone()))
            .collect(),
        blocks: state["blocks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| current_round(block, state))
            .map(|block| (text(&block["kind"]), text(&block["by"]), head.clone()))
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
pub(crate) fn outbox(channel: &str, delivery: &str, request: Value) -> Result<String, String> {
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

/** Render the pull request body: the delivery binding (repository, task, contract digest,
 * checkpoint, checked commit and tree, checks, attestation), the affected zones, the contract and
 * its tests, the validation results, the attestation, the autonomy mode, the exceptions, and the
 * merge policy that applies
 * Input
    - session: &ContractSession - session
    - round: &Value - the delivery state with the submitted round
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
    let binding = &round["binding"];
    let text = |value: &Value| {
        value
            .as_str()
            .map_or_else(|| "none".to_string(), String::from)
    };
    let mut out = format!(
        "Delivered by Crane from contract session `{}`{}.\n\n## Delivery binding\n\nThis pull request is bound to exactly the repository state Crane checked:\n\n| | |\n|---|---|\n| repository | {} (`{}`) |\n| task | {} |\n| task contract | {} `{}` |\n| checkpoint | {} `{}` |\n| checked commit | `{}` (tree `{}`) |\n| checks digest | `{}` |\n| attestation | `{}` |\n| verified by | {} |\n| **binding digest** | `{}` |\n\nApprovals count only for this binding; any new commit or check result needs new approvals.\n\n## Contract\n\nContract version `{}`\n\n",
        session.id(),
        round["task"].as_str().map_or(String::new(), |task| format!(" for task {task}")),
        match (binding["repository"]["owner"].as_str(), binding["repository"]["name"].as_str()) {
            (Some(owner), Some(name)) => format!("{owner}/{name}"),
            (_, Some(name)) => name.to_string(),
            _ => "this repository".to_string(),
        },
        text(&binding["repository"]["identity"]),
        text(&round["task"]),
        text(&binding["contract_id"]),
        text(&binding["contract_digest"]),
        text(&binding["checkpoint"]["name"]),
        text(&binding["checkpoint"]["sha"]),
        text(&round["head"]),
        text(&round["tree"]),
        text(&binding["checks_digest"]),
        text(&round["attestation_digest"]),
        text(&round["verification"]["by"]),
        text(&round["binding_digest"]),
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
    let zones = round["zones"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    out.push_str(&format!(
        "\n## Affected zones\n\n{} (change criticality {})\n\n## Validation\n\nContract tests: {} passed, {} failed, {} not applicable (decided by Crane, never by test files)\n\n",
        if zones.is_empty() { "none".to_string() } else { zones.join(", ") },
        round["criticality"].as_str().unwrap_or_default(),
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
        "\n## Attestation\n\n- digest `{}`\n- final decision {}\n- {} actions authorized, {} denied, {} violations, {} repairs\n- autonomy mode **{}**, safety {}\n\n## Exceptions\n\n",
        attestation["attestation_digest"].as_str().unwrap_or_default(),
        attestation["final_decision"]["decision"].as_str().unwrap_or_default(),
        attestation["action_summary"]["authorizations"],
        attestation["denied_actions"].as_array().map_or(0, Vec::len),
        attestation["violations"].as_array().map_or(0, Vec::len),
        attestation["repairs"].as_array().map_or(0, Vec::len),
        round["autonomy"].as_str().unwrap_or_default(),
        round["safety"].as_str().unwrap_or_default(),
    ));
    let exceptions = round["exceptions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|exception| exception["head"] == round["head"])
        .collect::<Vec<_>>();
    if exceptions.is_empty() {
        out.push_str("None.\n");
    }
    for exception in exceptions {
        out.push_str(&format!(
            "- `{}`: check {} excepted by {} until {} ({})\n",
            text(&exception["id"]),
            text(&exception["check"]),
            text(&exception["by"]),
            exception["expires_at"],
            text(&exception["reason"])
        ));
    }
    out.push_str(&format!(
        "\n## Merge policy\n\nRule `{}`: {} approval(s){}{}.\n",
        eligibility["rule"]["name"].as_str().unwrap_or_default(),
        eligibility["approvals"]["required"],
        eligibility["rule"]["approvers"]
            .as_array()
            .filter(|approvers| !approvers.is_empty())
            .map_or(String::new(), |approvers| format!(
                " from {}",
                approvers
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        if eligibility["rule"]["auto_merge"] == true {
            ", merged automatically once eligible"
        } else {
            ""
        },
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
    - trailers: &str - commit trailers binding the commit to its session, task, contract, and
      attestation
 * Output
    - Result<(PathBuf, String, String, bool), String> working directory, branch, base, and
      whether the base must be checked out again afterwards
*/
fn prepare_branch(
    session: &ContractSession,
    config: &DeliveryConfig,
    files: &[String],
    trailers: &str,
) -> Result<(PathBuf, String, String, bool), String> {
    let repository = root_directory()?;
    let current = git_in(&repository, &["branch", "--show-current"])?;
    // The configured base, else the connected repository's default branch, else the checkout
    let base = config
        .base_branch
        .clone()
        .or_else(|| {
            crate::repo::connected()
                .and_then(|record| record["default_branch"].as_str().map(String::from))
        })
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
            "{}{}\n\nDelivered by Crane from contract session {} (contract {}).\n\n{trailers}",
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

/** Open or update the pull request through the repository provider abstraction (the local
 * provider keeps a record and merges with git; GitHub pushes and uses its CLI); the body is kept
 * in the delivery directory
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
    let provider = named(&config.provider)
        .ok_or_else(|| format!("unknown delivery provider '{}'", config.provider))?;
    // Local numbers count the deliveries that opened a pull request
    let local_number = fs::read_dir(root()?.join("runtime").join("delivery"))
        .map_err(io_error)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().join("pull_request.md").exists())
        .count() as u64;
    let repository = root_directory()?;
    let (number, url) = provider.open_pull_request(&PullRequest {
        repository: &repository,
        branch,
        base,
        head: state["head"].as_str().unwrap_or_default(),
        title,
        body_file: &file,
        number: existing["number"].as_u64(),
        url: existing["url"].as_str(),
        local_number,
        url_template: config.pr_url_template.as_deref(),
        cli: &config.github_cli,
    })?;
    Ok(json!({
        "action": if existing.is_null() { "opened" } else { "updated" },
        "provider": provider.name(),
        "number": number,
        "url": url,
        "title": title,
        "branch": branch,
        "base": base,
        "head": state["head"],
        "binding_digest": state["binding_digest"],
        "body_digest": sha256(text.as_bytes()),
    }))
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
    if !state(&journal(id)?)["merged"].is_null() {
        return Err(format!(
            "delivery {id} is already merged; nothing to deliver"
        ));
    }
    // Only a verified session is delivered: a governed one once the orchestrator verified it, any
    // other once its finalization reconciled to PASS (checked below)
    crate::session_orchestrator::delivery_allowed(id)?;
    let governed = crate::session_orchestrator::load(id)?;
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
    let task_contract = governance.task_contract.clone().unwrap_or(Value::Null);
    let trailers = format!(
        "Crane-Session: {id}\nCrane-Task: {}\nCrane-Contract-Digest: {}\nCrane-Attestation: {}",
        session.identity().1.unwrap_or("none"),
        task_contract["digest"].as_str().unwrap_or("none"),
        attestation["attestation_digest"]
            .as_str()
            .unwrap_or_default()
    );
    let (workdir, branch, base, restore) = prepare_branch(&session, &config, &files, &trailers)?;
    let head = git_in(&workdir, &["rev-parse", "HEAD"])?;
    let tree = git_in(&workdir, &["rev-parse", "HEAD^{tree}"])?;
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
    let contract_tests = json!({"passed": summary["passed"], "failed": summary["failed"], "not_applicable": summary["not_applicable"], "check": CONTRACT_TESTS});
    // The binding ties the pull request, the approvals, and the merge to exactly this checked
    // state: repository, task, contract, checkpoint, commit and tree, check results, attestation
    let checkpoint = match crate::task_contracts::stored(
        session.identity().1.unwrap_or_default(),
        task_contract["version"].as_u64().unwrap_or(0),
    ) {
        Ok(contract) => contract["bindings"]["checkpoint"].clone(),
        Err(_) => session
            .contracts()
            .contracts
            .first()
            .map_or(Value::Null, |contract| json!({"name": contract.checkpoint, "sha": contract.checkpoint_sha.clone().ok()})),
    };
    let (owner, name) = crate::repo::owner_and_name();
    let binding = json!({
        "repository": {"identity": session.describe()["repository_id"], "owner": owner, "name": name},
        "task": session.identity().1,
        "contract_id": task_contract["contract_id"],
        "contract_digest": task_contract["digest"],
        "contract_version": session.contracts().version,
        "checkpoint": checkpoint,
        "base": base,
        "branch": branch,
        "head": head,
        "tree": tree,
        "checks_digest": sha256(json!({"checks": checks, "contract_tests": contract_tests}).to_string().as_bytes()),
        "attestation": attestation["attestation_digest"],
        "zones": zones,
        "autonomy": final_state["autonomy"],
    });
    let binding_digest = sha256(binding.to_string().as_bytes());
    let verification = match &governed {
        Some(record) => {
            json!({"by": "orchestrator", "phase": record["phase"], "attestation": record["termination"]["attestation_digest"]})
        }
        None => json!({"by": "finalization", "decision": decision}),
    };
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
            "contract_tests": contract_tests,
            "checks": checks,
            "tree": tree,
            "binding": binding,
            "binding_digest": binding_digest,
            "verification": verification,
            "task_contract": task_contract,
            "checkpoint": checkpoint,
        }),
    )?;
    let mut current = state(&journal(id)?);
    let eligibility = evaluate(&config, &facts(&current, now_unix(), config.approval_ttl)?);
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
    session.record(json!({"event": "delivery_submitted", "branch": current["branch"], "head": current["head"], "binding_digest": current["binding_digest"], "pull_request": {"number": pull_request["number"], "url": pull_request["url"]}, "checks": current["checks"], "contract_tests": current["contract_tests"], "criticality": current["criticality"], "rule": eligibility["rule"]["name"]}))?;
    if let Some(task) = session.identity().1 {
        crate::orchestration::delivery_progress(task, None)?;
    }
    announce(&config, id, &current, &eligibility)?;
    if eligibility["auto_merge"] == true {
        return merge(id, "crane (auto-merge policy)");
    }
    status(id)
}

/** Report a delivery: its state, its merge evaluation (the merge rule for its autonomy and zone
 * criticality, plus the policies that must still hold: the binding is intact, the task contract is
 * still current, a governed session is still DELIVERY_READY, approvals have not expired), its
 * delivery state, and its chain
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
    let now = now_unix();
    current["eligibility"] = if current["head"].is_null() {
        Value::Null
    } else {
        let mut eligibility = evaluate(&config, &facts(&current, now, config.approval_ttl)?);
        let mut missing = Vec::new();
        if !current["binding"].is_null() {
            if sha256(current["binding"].to_string().as_bytes())
                != current["binding_digest"].as_str().unwrap_or_default()
            {
                missing.push("the delivery binding does not match its digest".to_string());
            }
            let tree = current["head"].as_str().and_then(|head| {
                git_in(
                    &root_directory().ok()?,
                    &["rev-parse", &format!("{head}^{{tree}}")],
                )
                .ok()
            });
            if tree.as_deref() != current["tree"].as_str() {
                missing.push("the checked commit is not in the repository any more".to_string());
            }
        }
        if let (Some(task), Some(version)) = (
            current["task"].as_str(),
            current["task_contract"]["version"].as_u64(),
        ) {
            if let Some(reason) = crate::task_contracts::obsolete(task, version) {
                missing.push(reason);
            }
        }
        if let Err(reason) = crate::session_orchestrator::delivery_allowed(id) {
            missing.push(reason);
        }
        if let Some(ttl) = config.approval_ttl {
            let approvals = current["approvals"].as_array().cloned().unwrap_or_default();
            let fresh = |by: &Value| {
                approvals.iter().any(|approval| {
                    approval["by"] == *by
                        && current_round(approval, &current)
                        && approval["at"].as_u64().unwrap_or(0) + ttl > now
                })
            };
            let mut reported = BTreeSet::new();
            for approval in &approvals {
                if current_round(approval, &current)
                    && approval["at"].as_u64().unwrap_or(0) + ttl <= now
                    && !fresh(&approval["by"])
                    && reported.insert(approval["by"].to_string())
                {
                    missing.push(format!(
                        "the approval by {} expired (approvals count for {ttl} seconds)",
                        approval["by"].as_str().unwrap_or_default()
                    ));
                }
            }
        }
        if !missing.is_empty() && current["merged"].is_null() {
            if let Some(list) = eligibility["missing"].as_array_mut() {
                list.extend(missing.into_iter().map(Value::from));
            }
            eligibility["eligible"] = json!(false);
            eligibility["auto_merge"] = json!(false);
        }
        eligibility
    };
    let failed_after = current["merge_failures"]
        .as_array()
        .and_then(|failures| failures.last())
        .is_some_and(|failure| current_round(failure, &current));
    let rejected = current["blocks"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|block| block["kind"] == "rejection" && current_round(block, &current));
    current["delivery_state"] = json!(if !current["merged"].is_null() {
        "COMPLETE"
    } else if current["head"].is_null() {
        "BLOCKED"
    } else if failed_after {
        "MERGE_FAILED"
    } else if rejected {
        "REJECTED"
    } else if current["eligibility"]["eligible"] == true {
        "APPROVED"
    } else {
        "AWAITING_APPROVAL"
    });
    Ok(current)
}

/** Report a delivery that has a submitted round, the only kind that can be approved, excepted,
 * or merged (a delivery that was blocked before its pull request has nothing to decide on)
 * Input
    - id: &str - delivery id
 * Output
    - Result<Value, String> the delivery status
*/
fn submitted(id: &str) -> Result<Value, String> {
    let current = status(id)?;
    if current["head"].is_null() {
        return Err(format!(
            "delivery {id} has no submitted pull request yet; run 'crane deliver run {id}' after the session reconciles"
        ));
    }
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
    - binding: Option<&str> - the delivery binding the decision is about (Slack buttons carry it)
    - reason: &str - why
    - via: &str - cli or slack
 * Output
    - Result<Value, String> the delivery status, with already_recorded when the same person
      already made the same decision on this round (nothing is recorded again)
*/
pub(crate) fn decide(
    id: &str,
    kind: &str,
    by: &str,
    head: Option<&str>,
    binding: Option<&str>,
    reason: &str,
    via: &str,
) -> Result<Value, String> {
    let session = session(id)?;
    refuse_agents(Some(&session), kind)?;
    let config = policy::load()?;
    let current = submitted(id)?;
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
    let bound = current["binding_digest"].as_str();
    let stale_binding = binding.is_some_and(|binding| Some(binding) != bound);
    if head.is_some_and(|head| head != round) || stale_binding {
        record(
            id,
            json!({"kind": "unauthorized_action", "by": name, "action": kind, "via": via, "reason": "stale commit or binding", "head": head, "binding": binding}),
        )?;
        return Err(format!(
            "that message is about an earlier commit or check result; the delivery is now at {round} (binding {})",
            bound.unwrap_or("none")
        ));
    }
    let list = if kind == "approval" {
        "approvals"
    } else {
        "blocks"
    };
    let repeated = current[list].as_array().into_iter().flatten().any(|entry| {
        entry["by"] == name.as_str()
            && entry["kind"] == kind
            && current_round(entry, &current)
            && (kind != "approval"
                || config
                    .approval_ttl
                    .is_none_or(|ttl| entry["at"].as_u64().unwrap_or(0) + ttl > now_unix()))
    });
    if repeated {
        let mut after = status(id)?;
        after["already_recorded"] = json!(true);
        return Ok(after);
    }
    record(
        id,
        json!({"kind": kind, "by": name, "head": round, "binding": bound, "checks_digest": current["binding"]["checks_digest"], "reason": reason, "via": via}),
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
    let current = submitted(id)?;
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
    refresh_pull_request(&config, id, &session)?;
    let after = status(id)?;
    if after["eligibility"]["auto_merge"] == true {
        return merge(id, &format!("crane (auto-merge after {name}'s exception)"));
    }
    Ok(after)
}

/** Update the pull request body of the current round (after an exception), so it always shows
 * what the merge relies on
 * Input
    - config: &DeliveryConfig - configuration
    - id: &str - delivery id
    - session: &ContractSession - session
 * Output
    - Result<(), String>
*/
fn refresh_pull_request(
    config: &DeliveryConfig,
    id: &str,
    session: &ContractSession,
) -> Result<(), String> {
    let current = status(id)?;
    let attestation = attest(&session.document(), &session.events())?;
    let text = body(session, &current, &attestation, &current["eligibility"]);
    let title = current["pull_request"]["title"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut event = open_pull_request(config, id, &current, &title, &text)?;
    event["kind"] = json!("pull_request");
    record(id, event)?;
    Ok(())
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
            click.binding.as_deref(),
            &reason,
            "slack",
        ),
        "reject" => decide(
            &click.delivery,
            "rejection",
            &by,
            Some(&click.head),
            click.binding.as_deref(),
            &reason,
            "slack",
        ),
        "request_changes" => decide(
            &click.delivery,
            "changes_requested",
            &by,
            Some(&click.head),
            click.binding.as_deref(),
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

/** Merge a delivery once the merge policy allows it: refused unless eligible (the merge rule for
 * its autonomy and zones, the binding, the task contract, the session's verification); the
 * provider merges exactly the checked commit; a failed merge is recorded and leaves the session,
 * the task, and the trusted checkpoint untouched; merging a merged delivery changes nothing
 * Input
    - id: &str - delivery id
    - by: &str - who merges
 * Output
    - Result<Value, String> the delivery status (already_merged when it was)
*/
pub(crate) fn merge(id: &str, by: &str) -> Result<Value, String> {
    let session = session(id)?;
    refuse_agents(Some(&session), "merge")?;
    let config = policy::load()?;
    let current = submitted(id)?;
    if !current["merged"].is_null() {
        let mut after = status(id)?;
        after["already_merged"] = json!(true);
        return Ok(after);
    }
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
    let provider = named(&config.provider)
        .ok_or_else(|| format!("unknown delivery provider '{}'", config.provider))?;
    let body_file = directory(id)?.join("pull_request.md");
    let merged = provider.merge_pull_request(&PullRequest {
        repository: &repository,
        branch: current["branch"].as_str().unwrap_or_default(),
        base: current["base"].as_str().unwrap_or_default(),
        head: current["head"].as_str().unwrap_or_default(),
        title: current["pull_request"]["title"]
            .as_str()
            .unwrap_or_default(),
        body_file: &body_file,
        number: current["pull_request"]["number"].as_u64(),
        url: current["pull_request"]["url"].as_str(),
        local_number: 0,
        url_template: config.pr_url_template.as_deref(),
        cli: &config.github_cli,
    });
    match merged {
        Ok(merge_sha) => complete(id, &merge_sha, by, &current),
        Err(error) => {
            failed_merge(id, &config, &session, &current, by, &error)?;
            Err(format!("the merge of delivery {id} failed: {error}; the session and its task are not completed"))
        }
    }
}

/** Record a failed merge: in the delivery journal (MERGE_FAILED), on the session, and to Slack;
 * nothing is completed and no checkpoint is trusted
 * Input
    - id: &str - delivery id
    - config: &DeliveryConfig - configuration
    - session: &ContractSession - session
    - current: &Value - delivery status
    - by: &str - who merged
    - error: &str - why it failed
 * Output
    - Result<(), String>
*/
fn failed_merge(
    id: &str,
    config: &DeliveryConfig,
    session: &ContractSession,
    current: &Value,
    by: &str,
    error: &str,
) -> Result<(), String> {
    record(
        id,
        json!({"kind": "merge_failed", "by": by, "head": current["head"], "binding": current["binding_digest"], "reason": error}),
    )?;
    session.record(json!({"event": "delivery_merge_failed", "by": by, "head": current["head"], "reason": error}))?;
    tell(
        config,
        id,
        &format!("The merge of delivery {id} failed: {error}. Nothing was completed."),
    )
}

/** Handle a merge reported by the provider (a merge made on the host, or a webhook delivered more
 * than once): the same merge again changes nothing; a different merge of a merged delivery is
 * refused; a merge of a delivery that was not eligible is recorded as unauthorized and never
 * completes the task or becomes a trusted checkpoint; otherwise the delivery is completed
 * Input
    - id: &str - delivery id
    - merge_sha: &str - the merge commit the provider reports
    - by: &str - who reports it
 * Output
    - Result<Value, String> the delivery status
*/
pub(crate) fn merged(id: &str, merge_sha: &str, by: &str) -> Result<Value, String> {
    let session = session(id)?;
    refuse_agents(Some(&session), "merged")?;
    let config = policy::load()?;
    let current = submitted(id)?;
    let reported = git_in(
        &root_directory()?,
        &["rev-parse", &format!("{merge_sha}^{{commit}}")],
    )
    .map_err(|_| format!("merge commit {merge_sha} is not in this repository; fetch it first"))?;
    if !current["merged"].is_null() {
        if current["merged"]["sha"] == reported.as_str() {
            let mut after = status(id)?;
            after["already_merged"] = json!(true);
            return Ok(after);
        }
        record(
            id,
            json!({"kind": "unauthorized_action", "by": by, "action": "merged", "reason": format!("delivery is merged as {}, not {reported}", current["merged"]["sha"].as_str().unwrap_or_default())}),
        )?;
        return Err(format!(
            "delivery {id} is already merged as {}; the report of {reported} was refused",
            current["merged"]["sha"].as_str().unwrap_or_default()
        ));
    }
    if current["eligibility"]["eligible"] != true {
        let reasons = current["eligibility"]["missing"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("; ");
        record(
            id,
            json!({"kind": "merge_failed", "by": by, "head": current["head"], "binding": current["binding_digest"], "sha": reported, "reason": format!("merged without eligibility: {reasons}")}),
        )?;
        tell(&config, id, &format!("Delivery {id} was merged as {reported} without meeting the merge policy ({reasons}); it is not completed and not trusted."))?;
        return Err(format!(
            "delivery {id} was merged without meeting the merge policy ({reasons}); it is not completed"
        ));
    }
    complete(id, &reported, by, &current)
}

/** Complete a merged delivery: verify the merge contains exactly the checked commit, record it,
 * make the merge commit the trusted checkpoint (and the connected repository's trusted state),
 * complete the task (and only then its Jira or Asana issue), note it on the governed session, and
 * re-finalize the session attestation; the delivery becomes COMPLETE
 * Input
    - id: &str - delivery id
    - merge_sha: &str - merge commit
    - by: &str - who merged
    - current: &Value - delivery status before the merge
 * Output
    - Result<Value, String> the delivery status
*/
fn complete(id: &str, merge_sha: &str, by: &str, current: &Value) -> Result<Value, String> {
    let session = session(id)?;
    let config = policy::load()?;
    let repository = root_directory()?;
    let eligibility = &current["eligibility"];
    let base = current["base"].as_str().unwrap_or_default();
    let head = current["head"].as_str().unwrap_or_default();
    // A merge counts only when the merged history contains exactly the commit that was checked
    let verified = git_in(
        &repository,
        &["merge-base", "--is-ancestor", head, merge_sha],
    )
    .is_ok();
    if !verified {
        let error = format!("merge {merge_sha} does not contain the checked commit {head}");
        failed_merge(id, &config, &session, current, by, &error)?;
        return Err(format!("{error}; the task is not completed"));
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
    let checkpoint = crate::commands::checkpoint::create_at(&name, merge_sha, base)?;
    crate::repo::trust(
        &checkpoint.name,
        merge_sha,
        &format!("delivery {id} merged"),
    )?;
    record(
        id,
        json!({"kind": "merged", "sha": merge_sha, "by": by, "head": head, "base": base, "binding": current["binding_digest"], "verified": true, "trusted_checkpoint": checkpoint.name, "rule": eligibility["rule"]["name"], "approvals": eligibility["approvals"], "excepted": eligibility["excepted"]}),
    )?;
    // The task is not completed here: its tracker issue is completed by the completion dispatcher
    // once Jira confirms it, from a completion event queued now that the merge is verified
    let mut completion = Value::Null;
    if let Some(task) = session.identity().1 {
        crate::orchestration::delivery_progress(task, Some(merge_sha))?;
        let event = crate::task_completion::enqueue(id, task, merge_sha, current, &config)?;
        completion = json!({"task": task, "tracker": event["tracker"], "external_id": event["external_id"], "completion_event": event["event_id"], "status": event["status"], "outbox": event["outbox"]});
        record(
            id,
            json!({"kind": "task_completion_queued", "task": task, "tracker": completion["tracker"], "external_id": completion["external_id"], "completion_event": completion["completion_event"], "status": completion["status"], "outbox": completion["outbox"]}),
        )?;
    }
    crate::session_orchestrator::delivered(id, merge_sha, by)?;
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

/** Append an event to a delivery's journal on behalf of another component (the task completion
 * dispatcher confirming the tracker completion)
 * Input
    - id: &str - delivery id
    - event: Value - event fields (with "kind")
 * Output
    - Result<(), String>
*/
pub(crate) fn note(id: &str, event: Value) -> Result<(), String> {
    if id.is_empty() || journal(id)?.is_empty() {
        return Ok(());
    }
    record(id, event).map(|_| ())
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
