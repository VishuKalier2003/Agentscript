// Task contracts: the executable contract of one task, compiled by the task planner from the task,
// the repository, the active policies, the relevant zones, the discovered symbols, the checkpoint,
// and the autonomy rules, then versioned per task and bound to everything it was derived from.
// A contract grants nothing until a human approves its digest; a vague task compiles to a
// clarification_required contract that grants no authority; a contract whose checkpoint,
// persistent policies, zones, autonomy configuration, budget model, organization, or repository
// changed is invalidated and has to be compiled again, while a changed task is compiled into a new
// version. Agents can read contracts, never compile, approve, or reject them.

pub(crate) mod activation;

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::autonomy::manage::load_policy;
use crate::budget::manage::load_model;
use crate::evidence::{link, verify_chain, GENESIS};
use crate::ir::checkpoint_commit;
use crate::proposals::store::{agent_environment, Proposal};
use crate::repository::{git, load_checkpoint, root};
use crate::session::repository_identity;
use crate::tasks::{load as load_task, plan, proposal_from_plan, valid_task_id, TaskInput};
use crate::util::{io_error, now_unix, sha256};
use crate::zones::model::{load as load_zones, set_version};

/** Version of the task contract layout */
pub(crate) const CONTRACT_FORMAT: u64 = 1;

/** Number of digest characters an approver must quote */
const CONFIRM_LENGTH: usize = 12;

/** Statuses whose bindings are still checked (later statuses are final) */
const LIVE: &[&str] = &["proposed", "approved"];

/** Refuse an action on behalf of an agent: an agent can never compile, approve, or reject a
 * contract, its own or another's (the pre-tool hook also denies these commands to agents and
 * treats trying as self-escalation)
 * Input
    - action: &str - compile, approve, or reject
 * Output
    - Result<(), String>
*/
fn require_human(action: &str) -> Result<(), String> {
    match agent_environment() {
        Some(marker) => Err(format!(
            "crane task contract {action} refuses to run in an agent environment ({marker} is set); an agent can never change a task contract, its own or another's"
        )),
        None => Ok(()),
    }
}

/** Return the task contracts directory, .crane/task-contracts
 * Input
    - None
 * Output
    - Result<PathBuf, String>
*/
fn directory() -> Result<PathBuf, String> {
    Ok(root()?.join("task-contracts"))
}

/** Return the file of one contract version, .crane/task-contracts/TASK/vN.json
 * Input
    - task: &str - task id
    - version: u64 - contract version
 * Output
    - Result<PathBuf, String>
*/
fn path(task: &str, version: u64) -> Result<PathBuf, String> {
    if !valid_task_id(task) {
        return Err(format!("invalid task id '{task}'"));
    }
    Ok(directory()?.join(task).join(format!("v{version}.json")))
}

/** List the contract versions of a task, oldest first
 * Input
    - task: &str - task id
 * Output
    - Result<Vec<u64>, String>
*/
fn versions(task: &str) -> Result<Vec<u64>, String> {
    if !valid_task_id(task) {
        return Err(format!("invalid task id '{task}'"));
    }
    let mut found = match fs::read_dir(directory()?.join(task)) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.strip_prefix('v')?
                    .strip_suffix(".json")?
                    .parse::<u64>()
                    .ok()
            })
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(io_error(error)),
    };
    found.sort_unstable();
    Ok(found)
}

/** Read one contract version as stored
 * Input
    - task: &str - task id
    - version: u64 - contract version
 * Output
    - Result<Value, String>
*/
fn read(task: &str, version: u64) -> Result<Value, String> {
    let file = path(task, version)?;
    let text = fs::read_to_string(&file).map_err(|error| match error.kind() {
        ErrorKind::NotFound => format!("task {task} has no contract version {version}"),
        _ => io_error(error),
    })?;
    let record: Value = serde_json::from_str(&text)
        .map_err(|error| format!("task contract {task}@v{version} is invalid: {error}"))?;
    if record["task_contract_format"].as_u64() != Some(CONTRACT_FORMAT) {
        return Err(format!(
            "task contract {task}@v{version} has an unsupported format"
        ));
    }
    Ok(record)
}

/** Read a task's latest contract exactly as stored, without bringing it up to date or writing
 * anything (load refreshes and persists invalidations; read-only observability must not)
 * Input
    - task: &str - task id
 * Output
    - Result<Option<Value>, String> None when the task has no contract
*/
pub(crate) fn peek(task: &str) -> Result<Option<Value>, String> {
    match versions(task)?.last() {
        Some(version) => read(task, *version).map(Some),
        None => Ok(None),
    }
}

/** Write a contract version through a temporary file
 * Input
    - record: &Value - contract
 * Output
    - Result<(), String>
*/
fn save(record: &Value) -> Result<(), String> {
    let file = path(
        record["task_id"].as_str().unwrap_or_default(),
        record["version"].as_u64().unwrap_or(0),
    )?;
    fs::create_dir_all(file.parent().ok_or("invalid task contract path")?).map_err(io_error)?;
    let temporary = file.with_extension(format!("tmp{}", std::process::id()));
    // The record is written with its human summary first
    fs::write(
        &temporary,
        crate::human::pretty(&with_summary(record.clone()))?,
    )
    .map_err(io_error)?;
    fs::rename(&temporary, &file).map_err(io_error)
}

/** Attach the human summary to a contract (derived from its own fields, never part of its digest)
 * Input
    - record: Value - contract
 * Output
    - Value
*/
pub(crate) fn with_summary(mut record: Value) -> Value {
    record["summary"] = summarize(&record);
    record
}

/** Summarize a contract for people: the decision it needs, what the agent must and must not
 * change, what still needs a human, the tests, and the commands that act on it
 * Input
    - record: &Value - contract
 * Output
    - Value
*/
pub(crate) fn summarize(record: &Value) -> Value {
    use crate::human;
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    let task = text(&record["task_id"]);
    let contract = &record["contract"];
    let items = |section: &str| {
        contract[section]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| format!("{} ({})", text(&item["qualified"]), text(&item["file"])))
            .collect::<Vec<_>>()
    };
    let at = |value: &Value| human::when_value(&value["at"]).unwrap_or_default();
    let first = |section: &str| {
        record[section]
            .as_array()
            .and_then(|list| list.iter().find(|item| item["blocking"] != false))
            .map_or(String::new(), |item| text(&item["message"]))
    };
    let invalidated = record["invalidation"]["message"].as_str();
    let status = text(&record["status"]);
    let decision = match (status.as_str(), invalidated) {
        (_, Some(message)) => format!("Recompile: {message}"),
        ("proposed", _) => {
            format!("Approve or reject: let an AI agent work on {task} within this contract")
        }
        ("clarification_required", _) => format!(
            "Clarify the task in the tracker, then recompile: {}",
            first("clarifications")
        ),
        ("conflicts_with_policy", _) => format!(
            "Resolve the conflict with active policy, then recompile: {}",
            first("conflicts")
        ),
        ("unrelated", _) => "None: the task does not touch this repository".into(),
        ("approved", _) => format!(
            "None: approved by {} on {}; an agent session can start",
            text(&record["approval"]["approver"]),
            at(&record["approval"])
        ),
        ("rejected", _) => format!(
            "None: rejected by {} on {}{}",
            text(&record["rejection"]["by"]),
            at(&record["rejection"]),
            record["rejection"]["reason"]
                .as_str()
                .map_or(String::new(), |reason| format!(" ({reason})"))
        ),
        ("superseded", _) => "None: a newer version replaced it".into(),
        (other, _) => format!("None: the contract is {}", other.replace('_', " ")),
    };
    let tests = contract["EXPECTED_TESTS"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| {
            if let Some(test) = item["test"].as_str() {
                format!("run {test}")
            } else if let Some(file) = item["test_file"].as_str() {
                format!("run the tests in {file}")
            } else if let Some(list) = item["add_regression_test"].as_array() {
                format!(
                    "add a regression test for {}",
                    list.iter()
                        .map(|name| human::symbol(&text(name)))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                format!(
                    "add a test for {}",
                    human::symbol(&text(&item["add_test_for"]))
                )
            }
        })
        .collect::<Vec<_>>();
    let scope = &contract["TASK_SCOPE"];
    let names = |value: &Value| human::strings(value).join(", ");
    let code = human::code(record["digest"].as_str().unwrap_or_default());
    let must_not = items("MUST_NOT_CHANGE");
    let mut summary = json!({
        "decision_needed": decision,
        "task": format!("{task}: {}", text(&record["task"]["title"])),
        "status": match status.as_str() {
            "proposed" => "waiting for review",
            "approved" if invalidated.is_some() => "approved, but out of date",
            "approved" => "approved: an agent may work within it",
            "clarification_required" => "the task is unclear; no agent may work on it",
            "conflicts_with_policy" => "the task conflicts with active policy; no agent may work on it",
            "unrelated" => "the task does not touch this repository",
            "rejected" => "rejected",
            "superseded" => "replaced by a newer version",
            other => other,
        },
        "grants_authority": status == "approved" && invalidated.is_none(),
        "must_change": items("MUST_CHANGE"),
        "must_not_change": if must_not.len() > 8 { json!(format!("{} and {} more", must_not[..8].join(", "), must_not.len() - 8)) } else { json!(must_not) },
        "may_change": contract["MAY_CHANGE"].as_array().map_or(0, Vec::len),
        "needs_human_approval": contract["REQUIRES_APPROVAL"].as_array().into_iter().flatten().map(|item| text(&item["reason"])).collect::<Vec<_>>(),
        "scope": format!("services: {}; modules: {}; files: {}", names(&scope["services"]), names(&scope["modules"]), names(&scope["files"])),
        "tests": tests,
        "version": record["version"],
        "approval_code": code,
        "compiled": format!("by {} on {}", text(&record["compiled"]["by"]), at(&record["compiled"])),
        "review": format!("crane task contract show {task}"),
    });
    match status.as_str() {
        "proposed" if invalidated.is_none() => {
            summary["approve"] = json!(format!(
                "crane task contract approve {task} --approver YOUR_NAME --confirm {code}"
            ));
            summary["reject"] = json!(format!(
                "crane task contract reject {task} --approver YOUR_NAME --reason \"WHY\""
            ));
        }
        "approved" if invalidated.is_none() => {
            summary["start"] = json!(format!("crane task launch {task} --agent claude"));
        }
        "unrelated" | "rejected" | "superseded" => {}
        _ => summary["recompile"] = json!(format!("crane task contract compile {task}")),
    }
    summary
}

/** Append a history entry to a contract
 * Input
    - record: &mut Value - contract
    - action: &str - what happened
    - by: &str - who or what did it
    - detail: Value - details
 * Output
    - None
*/
fn log(record: &mut Value, action: &str, by: &str, detail: Value) {
    let entry = json!({"action": action, "by": by, "at": now_unix(), "status": record["status"], "detail": detail});
    if let Some(history) = record["history"].as_array_mut() {
        history.push(entry);
    }
}

/** Append an event to the hash-chained audit log of task contracts
 * Input
    - event: Value - event fields
 * Output
    - Result<(), String>
*/
fn audit(mut event: Value) -> Result<(), String> {
    let file = directory()?.join("audit.jsonl");
    let events = audit_events()?;
    let previous = events
        .last()
        .and_then(|last| last["chain"].as_str().map(String::from))
        .unwrap_or_else(|| GENESIS.to_string());
    event["seq"] = json!(events.len() + 1);
    event["at"] = json!(now_unix());
    event["git_user"] = json!(git(&["config", "user.email"]).ok());
    event["chain"] = json!(link(&previous, &event));
    fs::create_dir_all(directory()?).map_err(io_error)?;
    let mut handle = OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .map_err(io_error)?;
    writeln!(handle, "{event}").map_err(io_error)
}

/** Read the task contract audit log
 * Input
    - None
 * Output
    - Result<Vec<Value>, String>
*/
pub(crate) fn audit_events() -> Result<Vec<Value>, String> {
    match fs::read_to_string(directory()?.join("audit.jsonl")) {
        Ok(text) => text
            .lines()
            .map(|line| {
                serde_json::from_str(line)
                    .map_err(|error| format!("task contract audit log: {error}"))
            })
            .collect(),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(io_error(error)),
    }
}

/** Compute the bindings of a task's contract from the repository as it is now: the repository
 * identity, the task file, the checkpoint SHA, the persistent policy version, the zone set
 * version, the autonomy configuration, the budget model, and the organization configuration
 * Input
    - task_id: &str - task id
    - task_digest: &str - digest of the task file
    - checkpoint: &str - checkpoint name
 * Output
    - Result<Value, String>
    - Error if the checkpoint or a configuration file is unusable
*/
pub(crate) fn bindings(
    task_id: &str,
    task_digest: &str,
    checkpoint: &str,
) -> Result<Value, String> {
    let (_, identity) = repository_identity()?;
    let (owner, name) = crate::repo::owner_and_name();
    let stored = load_checkpoint(checkpoint)?;
    let sha = checkpoint_commit(checkpoint, &stored.name, &stored.commit)?;
    let (zones, _) = load_zones(&root()?)?;
    let autonomy = load_policy()?;
    let budget = load_model()?;
    let organization = activation::organization()?;
    Ok(json!({
        "repository": {"identity": identity, "owner": owner, "name": name},
        "task": {"task_id": task_id, "task_digest": task_digest},
        "checkpoint": {"name": checkpoint, "sha": sha},
        "policy_version": activation::policy_version()?,
        "zone_set_version": set_version(&zones),
        "autonomy": {"policy_version": autonomy.version(), "max_autonomy": autonomy.max_autonomy.name()},
        "budget": {"model_version": budget.version(), "max": budget.max_for(autonomy.max_autonomy)},
        "organization": organization,
    }))
}

/** Compute a contract's digest: the canonical JSON (keys sorted) of everything that defines it,
 * which is what an approver confirms
 * Input
    - record: &Value - contract
 * Output
    - String
*/
fn digest(record: &Value) -> String {
    let defining = json!({
        "task_contract_format": record["task_contract_format"],
        "task_id": record["task_id"],
        "version": record["version"],
        "plan_status": record["plan_status"],
        "contract": record["contract"],
        "bindings": record["bindings"],
        "clarifications": record["clarifications"],
        "conflicts": record["conflicts"],
        "agentscript": record["agentscript"],
        "policy_name": record["policy_name"],
    });
    sha256(defining.to_string().as_bytes())
}

/** Describe the authority a contract grants in its status
 * Input
    - status: &str - contract status
    - record: &Value - contract
 * Output
    - Value {granted, reason}
*/
fn authority(status: &str, record: &Value) -> Value {
    let reason = match status {
        "approved" => "a human approved this contract: sessions of the task act within its scope, under the zones and policies it was bound to",
        "proposed" => "the contract waits for human approval: sessions of the task are held to assisted autonomy (every change needs approval)",
        "clarification_required" => "the task is too vague to bound: no authority is granted until it is clarified and compiled again",
        "conflicts_with_policy" => "the task conflicts with a permanent policy or zone: no authority is granted",
        "unrelated" => "the task is for another repository: no authority is granted here",
        "invalidated" => "what the contract was bound to changed: no authority is granted until it is compiled and approved again",
        _ => "this contract version is no longer current: it grants no authority",
    };
    let mut value = json!({"granted": status == "approved", "reason": reason});
    if status == "approved" {
        value["scope"] = record["contract"]["TASK_SCOPE"].clone();
    }
    value
}

/** Set a contract's status, with its authority summary
 * Input
    - record: &mut Value - contract
    - status: &str - new status
 * Output
    - None
*/
fn set_status(record: &mut Value, status: &str) {
    record["status"] = json!(status);
    record["authority"] = authority(status, record);
}

/** Import a task file given as a path into .crane/tasks/ID.json, so its contract can be checked
 * against it later; a task id is returned unchanged
 * Input
    - spec: &str - task id or task file
 * Output
    - Result<String, String> task id
*/
fn task_id_of(spec: &str) -> Result<String, String> {
    if !(spec.ends_with(".json") || spec.contains('/') || spec.contains('\\')) {
        return Ok(spec.to_string());
    }
    let (task, _) = load_task(spec)?;
    if !valid_task_id(&task.task_id) {
        return Err(format!(
            "task {spec} has no valid task_id, so no contract can be stored for it"
        ));
    }
    let tasks = root()?.join("tasks");
    fs::create_dir_all(&tasks).map_err(io_error)?;
    fs::write(
        tasks.join(format!("{}.json", task.task_id)),
        serde_json::to_string_pretty(&task.to_json()).map_err(io_error)? + "\n",
    )
    .map_err(io_error)?;
    Ok(task.task_id)
}

/** Bring a live contract up to date: follow the decision taken on its policy proposal (approved
 * or rejected with 'crane policy', superseded or retired by the orchestrator), then compare its
 * bindings with the repository as it is now and invalidate it when the repository, checkpoint,
 * persistent policies, zones, autonomy configuration, budget model, or organization changed (a
 * changed task only sets task_changed: it is versioned by compiling again); with persist the
 * change is written (a pending proposal is superseded and an approved task policy retired, so it
 * is no longer enforced), otherwise it is only reported
 * Input
    - record: &mut Value - contract
    - persist: bool - write changes (never on behalf of an agent)
 * Output
    - Result<(), String>
*/
fn refresh(record: &mut Value, persist: bool) -> Result<(), String> {
    let status = record["status"].as_str().unwrap_or_default().to_string();
    if !LIVE.contains(&status.as_str()) {
        return Ok(());
    }
    let mut changed = false;
    let proposal = record["proposal"]["proposal_id"]
        .as_str()
        .and_then(|name| Proposal::load(name).ok());
    if let Some(proposal) = &proposal {
        match (status.as_str(), proposal.status()) {
            ("proposed", "approved") => {
                let approver = proposal.document["activation"]["approver"].clone();
                record["approval"] = json!({
                    "approver": approver,
                    "at": proposal.document["activation"]["at"],
                    "digest": record["digest"],
                    "via": format!("crane policy approve {}", proposal.name),
                });
                set_status(record, "approved");
                log(
                    record,
                    "approved",
                    approver.as_str().unwrap_or("a human"),
                    json!({"via": "policy proposal"}),
                );
                changed = true;
            }
            ("proposed", "rejected") => {
                record["rejection"] = proposal.document["rejection"].clone();
                set_status(record, "rejected");
                log(
                    record,
                    "rejected",
                    proposal.document["rejection"]["by"]
                        .as_str()
                        .unwrap_or("a human"),
                    json!({"via": "policy proposal"}),
                );
                changed = true;
            }
            ("proposed", "superseded") | ("approved", "retired") => {
                let to = if proposal.status() == "retired" {
                    "retired"
                } else {
                    "superseded"
                };
                set_status(record, to);
                log(
                    record,
                    to,
                    "crane task orchestration",
                    json!({"via": "policy proposal"}),
                );
                changed = true;
            }
            _ => {}
        }
    }
    if LIVE.contains(&record["status"].as_str().unwrap_or_default()) {
        let task_id = record["task_id"].as_str().unwrap_or_default().to_string();
        let task_digest = load_task(&task_id)
            .map(|(_, digest)| digest)
            .unwrap_or_else(|error| format!("unavailable: {error}"));
        let checkpoint = record["bindings"]["checkpoint"]["name"]
            .as_str()
            .unwrap_or("baseline")
            .to_string();
        let now = bindings(&task_id, &task_digest, &checkpoint).unwrap_or_else(
            |error| json!({"checkpoint": {"name": checkpoint, "sha": format!("unavailable: {error}")}}),
        );
        // A changed task is a new version, not an invalidation: the approved version stays in force
        // until the next one is approved, and a proposed one can no longer be approved
        record["task_changed"] =
            json!(record["bindings"]["task"]["task_digest"] != now["task"]["task_digest"]);
        let bound = &record["bindings"];
        let checks = [
            (
                "repository",
                &bound["repository"]["identity"],
                &now["repository"]["identity"],
            ),
            (
                "checkpoint",
                &bound["checkpoint"]["sha"],
                &now["checkpoint"]["sha"],
            ),
            ("policies", &bound["policy_version"], &now["policy_version"]),
            (
                "zones",
                &bound["zone_set_version"],
                &now["zone_set_version"],
            ),
            ("autonomy", &bound["autonomy"], &now["autonomy"]),
            (
                "budget",
                &bound["budget"]["model_version"],
                &now["budget"]["model_version"],
            ),
            (
                "organization",
                &bound["organization"]["digest"],
                &now["organization"]["digest"],
            ),
        ];
        let differences = checks
            .iter()
            .filter(|(_, was, is)| was != is)
            .map(|(binding, was, is)| json!({"binding": binding, "bound": was, "current": is}))
            .collect::<Vec<_>>();
        if !differences.is_empty() {
            let names = differences
                .iter()
                .filter_map(|difference| difference["binding"].as_str())
                .collect::<Vec<_>>()
                .join(", ");
            record["invalidation"] = json!({
                "at": now_unix(),
                "previous_status": record["status"],
                "changed": differences,
                "message": format!("{names} changed since the contract was compiled; compile it again with 'crane task contract compile {task_id}'"),
            });
            set_status(record, "invalidated");
            log(record, "invalidated", "crane", json!({"changed": names}));
            changed = true;
            if persist {
                if let Some(mut proposal) = proposal {
                    proposal.supersede(&format!(
                        "contract {} invalidated",
                        record["contract_id"].as_str().unwrap_or_default()
                    ))?;
                    proposal.retire(&format!("contract invalidated: {names} changed"))?;
                }
            }
        }
    }
    if changed && persist {
        save(record)?;
        if record["status"] == "invalidated" {
            audit(json!({
                "event": "task_contract_invalidated",
                "task_id": record["task_id"],
                "version": record["version"],
                "digest": record["digest"],
                "by": "crane",
                "changed": record["invalidation"]["changed"],
            }))?;
        }
    }
    Ok(())
}

/** Load a task's contract, its latest version unless one is given, brought up to date (changes
 * are written unless the caller is an agent)
 * Input
    - task: &str - task id
    - version: Option<u64> - version, the latest when None
 * Output
    - Result<Option<Value>, String> None when the task has no contract
*/
pub(crate) fn load(task: &str, version: Option<u64>) -> Result<Option<Value>, String> {
    let Some(version) = version.or(versions(task)?.last().copied()) else {
        return Ok(None);
    };
    let mut record = read(task, version)?;
    refresh(&mut record, agent_environment().is_none())?;
    Ok(Some(with_summary(record)))
}

/** Map a planner status to a contract status
 * Input
    - plan_status: &str - planned, task_needs_clarification, task_conflicts_with_policy, or
      task_unrelated
 * Output
    - &'static str
*/
fn status_of(plan_status: &str) -> &'static str {
    match plan_status {
        "planned" => "proposed",
        "task_conflicts_with_policy" => "conflicts_with_policy",
        "task_unrelated" => "unrelated",
        _ => "clarification_required",
    }
}

/** Compile a task's contract: plan the task, bind it, and store it as a new version when it differs
 * from the current one (an identical compile changes nothing). A planned task becomes a proposed
 * contract with its AgentScript stored as a pending policy proposal (task_ID_vN); a vague task
 * becomes clarification_required, a conflicting one conflicts_with_policy, and neither grants
 * authority. A pending earlier version is superseded; an approved one stays in force until the new
 * version is approved. Refused on behalf of an agent.
 * Input
    - spec: &str - task id, or a task file (imported into .crane/tasks)
    - checkpoint: &str - checkpoint the contract binds to
    - by: &str - who or what compiles
 * Output
    - Result<Value, String> the current contract, with "unchanged" telling whether it is new
    - Error if the task or checkpoint is unusable or the files cannot be written
*/
pub(crate) fn compile(spec: &str, checkpoint: &str, by: &str) -> Result<Value, String> {
    require_human("compile")?;
    let task_id = task_id_of(spec)?;
    let (task, task_digest) = load_task(&task_id)?;
    if task.task_id != task_id {
        return Err(format!(
            "task file {task_id}.json holds task_id '{}'",
            task.task_id
        ));
    }
    compile_task(&task, &task_digest, checkpoint, by)
}

/** Compile the contract of a loaded task (see compile)
 * Input
    - task: &TaskInput - task, stored as .crane/tasks/ID.json
    - task_digest: &str - digest of that file
    - checkpoint: &str - checkpoint
    - by: &str - who or what compiles
 * Output
    - Result<Value, String>
*/
fn compile_task(
    task: &TaskInput,
    task_digest: &str,
    checkpoint: &str,
    by: &str,
) -> Result<Value, String> {
    let task_id = task.task_id.as_str();
    let planned = plan(task, task_digest, checkpoint)?;
    let plan_status = planned["status"].as_str().unwrap_or_default().to_string();
    let existing = versions(task_id)?;
    let version = existing.last().copied().unwrap_or(0) + 1;
    let policy_name = format!(
        "{}_v{version}",
        planned["policy_name"].as_str().unwrap_or("task")
    );
    let mut record = json!({
        "task_contract_format": CONTRACT_FORMAT,
        "contract_id": format!("{task_id}@v{version}"),
        "task_id": task_id,
        "version": version,
        "plan_status": plan_status,
        "contract": planned["contract"],
        "bindings": bindings(task_id, task_digest, checkpoint)?,
        "clarifications": planned["clarifications"],
        "conflicts": planned["conflicts"],
        "agentscript": Value::Null,
        "agentscript_note": planned["agentscript_note"],
        "policy_name": Value::Null,
        "task": planned["task"],
        "repository_names": planned["repository"]["names"],
        "zones": planned["zones"],
        "policies": planned["policy_packs"]["policies"],
        "mentions": planned["mentions"],
        "previous_contracts": planned["previous_contracts"],
        "proposal": Value::Null,
        "approval": Value::Null,
        "rejection": Value::Null,
        "invalidation": Value::Null,
        "compiled": {"by": by, "at": now_unix()},
        "history": [],
    });
    // The AgentScript carries the versioned policy name, so it is part of what is approved
    if let Some(script) = planned["agentscript"].as_str() {
        let planned_name = format!(
            "policy {} {{",
            planned["policy_name"].as_str().unwrap_or_default()
        );
        record["agentscript"] =
            json!(script.replacen(&planned_name, &format!("policy {policy_name} {{"), 1));
        record["policy_name"] = json!(policy_name);
    }
    record["digest"] = json!(digest(&record));

    // An identical compile of the current version changes nothing
    if let Some(current) = existing.last() {
        let mut previous = read(task_id, *current)?;
        refresh(&mut previous, true)?;
        let same = {
            let mut probe = record.clone();
            probe["version"] = previous["version"].clone();
            probe["contract_id"] = previous["contract_id"].clone();
            probe["policy_name"] = previous["policy_name"].clone();
            if let (Some(script), Some(name)) = (
                probe["agentscript"].as_str(),
                previous["policy_name"].as_str(),
            ) {
                probe["agentscript"] = json!(script.replacen(
                    &format!("policy {policy_name} {{"),
                    &format!("policy {name} {{"),
                    1
                ));
            }
            digest(&probe) == previous["digest"].as_str().unwrap_or_default()
        };
        let reusable = !matches!(
            previous["status"].as_str(),
            Some("invalidated" | "superseded" | "retired")
        );
        if same && reusable {
            previous["unchanged"] = json!(true);
            return Ok(previous);
        }
        // A pending earlier version can no longer be approved
        if !matches!(
            previous["status"].as_str(),
            Some("approved" | "invalidated" | "retired" | "superseded" | "rejected")
        ) {
            if let Some(name) = previous["proposal"]["proposal_id"].as_str() {
                if let Ok(mut proposal) = Proposal::load(name) {
                    proposal.supersede(&format!("contract {task_id}@v{version}"))?;
                }
            }
            set_status(&mut previous, "superseded");
            log(
                &mut previous,
                "superseded",
                by,
                json!({"by": format!("{task_id}@v{version}")}),
            );
            save(&previous)?;
        }
    }

    let status = status_of(&plan_status);
    set_status(&mut record, status);
    if status == "proposed" && record["agentscript"].is_string() {
        Proposal::ensure_free(&policy_name)?;
        let content = proposal_from_plan(&planned, &policy_name)?;
        let proposal = Proposal::create(
            &policy_name,
            checkpoint,
            json!({"kind": "task", "task_id": task_id, "contract_version": version, "contract_digest": record["digest"]}),
            content,
            by,
        )?;
        record["proposal"] =
            json!({"proposal_id": proposal.name, "policy_digest": proposal.digest()});
    }
    let detail = json!({"digest": record["digest"], "plan_status": plan_status});
    log(&mut record, "compiled", by, detail);
    save(&record)?;
    audit(json!({
        "event": "task_contract_compiled",
        "task_id": task_id,
        "version": version,
        "status": status,
        "digest": record["digest"],
        "by": by,
        "bindings": record["bindings"],
    }))?;
    record["unchanged"] = json!(false);
    Ok(record)
}

/** Approve a task's current contract: refused on behalf of an agent and for any contract that is
 * not proposed (a vague, conflicting, invalidated, or superseded one can never be approved); the
 * approver must be named and quote the start of the contract digest. The contract's AgentScript
 * is activated through its policy proposal, an earlier approved version is retired, and the
 * decision is audited. Approving the same digest again changes nothing.
 * Input
    - task: &str - task id
    - approver: Option<String> - who approves (required)
    - confirm: Option<String> - at least the first 12 characters of the contract digest
 * Output
    - Result<Value, String> the contract, with already_approved
*/
pub(crate) fn approve(
    task: &str,
    approver: Option<String>,
    confirm: Option<String>,
) -> Result<Value, String> {
    require_human("approve")?;
    let approver = approver
        .filter(|value| !value.trim().is_empty())
        .ok_or("crane task contract approve requires --approver NAME")?;
    let mut record = load(task, None)?.ok_or_else(|| {
        format!("task {task} has no contract; compile it with 'crane task contract compile {task}'")
    })?;
    let digest = record["digest"].as_str().unwrap_or_default().to_string();
    let quoted = confirm.unwrap_or_default();
    let quoted = quoted.trim_start_matches("sha256:");
    let confirmed =
        quoted.len() >= CONFIRM_LENGTH && digest.trim_start_matches("sha256:").starts_with(quoted);
    let contract_id = record["contract_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    match record["status"].as_str().unwrap_or_default() {
        "approved" if confirmed => {
            record["already_approved"] = json!(true);
            return Ok(record);
        }
        "proposed" if record["task_changed"] == true => {
            return Err(format!("task {task} changed after contract {contract_id} was compiled; compile it again with 'crane task contract compile {task}'"))
        }
        "proposed" => {}
        "clarification_required" => {
            let needed = record["clarifications"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["blocking"] != false)
                .filter_map(|item| item["message"].as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!("contract {contract_id} cannot be approved: the task is too vague to bound ({needed}); clarify the task and compile it again"));
        }
        "invalidated" => {
            return Err(format!(
                "contract {contract_id} was invalidated: {}",
                record["invalidation"]["message"].as_str().unwrap_or_default()
            ))
        }
        other => {
            return Err(format!(
                "contract {contract_id} is {other}; only a proposed contract can be approved"
            ))
        }
    }
    if !confirmed {
        return Err(format!("crane task contract approve requires --confirm with at least the first {CONFIRM_LENGTH} characters of the contract digest (see 'crane task contract show {task}')"));
    }
    if let Some(name) = record["proposal"]["proposal_id"].as_str() {
        let mut proposal = Proposal::load(name)?;
        if proposal.status() == "pending" {
            let policy_digest = proposal.digest().to_string();
            proposal.approve(Some(approver.clone()), Some(policy_digest))?;
        } else if proposal.status() != "approved" {
            return Err(format!(
                "the policy proposal {name} of contract {contract_id} is {}",
                proposal.status()
            ));
        }
    }
    // An earlier approved version stops being enforced once this one is approved
    let version = record["version"].as_u64().unwrap_or(0);
    for earlier in versions(task)?
        .into_iter()
        .filter(|earlier| *earlier < version)
    {
        let mut previous = read(task, earlier)?;
        if previous["status"] == "approved" {
            if let Some(name) = previous["proposal"]["proposal_id"].as_str() {
                if let Ok(mut proposal) = Proposal::load(name) {
                    proposal.retire(&format!("replaced by contract {contract_id}"))?;
                }
            }
            set_status(&mut previous, "superseded");
            log(
                &mut previous,
                "superseded",
                &approver,
                json!({"by": contract_id}),
            );
            save(&previous)?;
        }
    }
    record["approval"] = json!({
        "approver": approver,
        "git_user": git(&["config", "user.email"]).ok(),
        "at": now_unix(),
        "digest": digest,
    });
    set_status(&mut record, "approved");
    log(
        &mut record,
        "approved",
        &approver,
        json!({"digest": digest}),
    );
    save(&record)?;
    audit(json!({
        "event": "task_contract_approved",
        "task_id": task,
        "version": version,
        "digest": digest,
        "by": approver,
        "bindings": record["bindings"],
    }))?;
    record["already_approved"] = json!(false);
    Ok(record)
}

/** Reject a task's current contract (a human declining it, whatever its status before approval);
 * its pending policy proposal is rejected too, and the decision is audited; rejecting again
 * changes nothing
 * Input
    - task: &str - task id
    - approver: Option<String> - who rejects (required)
    - reason: Option<String> - why
 * Output
    - Result<Value, String> the contract, with already_rejected
*/
pub(crate) fn reject(
    task: &str,
    approver: Option<String>,
    reason: Option<String>,
) -> Result<Value, String> {
    require_human("reject")?;
    let approver = approver
        .filter(|value| !value.trim().is_empty())
        .ok_or("crane task contract reject requires --approver NAME")?;
    let mut record = load(task, None)?.ok_or_else(|| format!("task {task} has no contract"))?;
    let contract_id = record["contract_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    match record["status"].as_str().unwrap_or_default() {
        "rejected" => {
            record["already_rejected"] = json!(true);
            return Ok(record);
        }
        "proposed" | "clarification_required" | "conflicts_with_policy" | "unrelated" => {}
        other => {
            return Err(format!(
                "contract {contract_id} is {other}; only a contract awaiting a decision can be rejected"
            ))
        }
    }
    if let Some(name) = record["proposal"]["proposal_id"].as_str() {
        let mut proposal = Proposal::load(name)?;
        if proposal.status() == "pending" {
            proposal.reject(Some(approver.clone()), reason.clone())?;
        }
    }
    record["rejection"] = json!({"by": approver, "reason": reason, "at": now_unix()});
    set_status(&mut record, "rejected");
    log(
        &mut record,
        "rejected",
        &approver,
        json!({"reason": reason}),
    );
    save(&record)?;
    audit(json!({
        "event": "task_contract_rejected",
        "task_id": task,
        "version": record["version"],
        "digest": record["digest"],
        "by": approver,
        "reason": reason,
    }))?;
    record["already_rejected"] = json!(false);
    Ok(record)
}

/** List every task with a contract: its current version, status, digest, and scope size
 * Input
    - None
 * Output
    - Result<Value, String> {contracts: [...]}
*/
pub(crate) fn list() -> Result<Value, String> {
    let mut tasks = match fs::read_dir(directory()?) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| valid_task_id(name))
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(io_error(error)),
    };
    tasks.sort();
    let mut contracts = Vec::new();
    for task in tasks {
        if let Some(record) = load(&task, None)? {
            contracts.push(json!({
                "task_id": task,
                "contract_id": record["contract_id"],
                "version": record["version"],
                "status": record["status"],
                "digest": record["digest"],
                "title": record["task"]["title"],
                "must_change": record["contract"]["MUST_CHANGE"].as_array().map_or(0, Vec::len),
                "modules": record["contract"]["TASK_SCOPE"]["modules"],
                "requires_approval": record["contract"]["REQUIRES_APPROVAL"].as_array().map_or(0, Vec::len),
                "versions": versions(&task)?,
            }));
        }
    }
    Ok(json!({"contracts": contracts}))
}

/** Return a task's contract history: every version with its status and digest, and the task's
 * audit events with the chain verification of the whole log
 * Input
    - task: &str - task id
 * Output
    - Result<Value, String>
*/
pub(crate) fn history(task: &str) -> Result<Value, String> {
    let mut listed = Vec::new();
    for version in versions(task)? {
        let record = read(task, version)?;
        listed.push(json!({
            "version": version,
            "status": record["status"],
            "digest": record["digest"],
            "compiled": record["compiled"],
            "approval": record["approval"],
            "invalidation": record["invalidation"],
        }));
    }
    let events = audit_events()?;
    Ok(json!({
        "task_id": task,
        "versions": listed,
        "events": events.iter().filter(|event| event["task_id"] == task).collect::<Vec<_>>(),
        "chain": verify_chain(&events),
    }))
}

/** Return what a session of a task is bound to: the current contract, checked against the
 * repository without writing anything (a session may start on behalf of an agent)
 * Input
    - task: &str - task id
 * Output
    - Result<Option<Value>, String> None when the task has no contract
*/
pub(crate) fn for_session(task: &str) -> Result<Option<Value>, String> {
    let Some(version) = versions(task)?.last().copied() else {
        return Ok(None);
    };
    let mut record = read(task, version)?;
    refresh(&mut record, false)?;
    Ok(Some(record))
}

/** Read one contract version exactly as stored (for checking a launch binding, which covers only
 * what never changes after compiling)
 * Input
    - task: &str - task id
    - version: u64 - contract version
 * Output
    - Result<Value, String>
*/
pub(crate) fn stored(task: &str, version: u64) -> Result<Value, String> {
    read(task, version)
}

/** Explain why the contract version a session was bound to no longer gives authority: it was
 * superseded, retired, rejected, or invalidated (checked without writing, since this runs for the
 * agent's tool calls)
 * Input
    - task: &str - task id
    - version: u64 - bound contract version
 * Output
    - Option<String> the reason, None while the version is still approved
*/
pub(crate) fn obsolete(task: &str, version: u64) -> Option<String> {
    let mut record = match read(task, version) {
        Ok(record) => record,
        Err(error) => return Some(format!("the bound task contract cannot be read: {error}")),
    };
    if let Err(error) = refresh(&mut record, false) {
        return Some(format!(
            "the bound task contract cannot be checked: {error}"
        ));
    }
    let status = record["status"].as_str().unwrap_or_default();
    (status != "approved").then(|| {
        format!(
            "task contract {} is {status}{}; this session's authority ended (a human must approve the current contract and launch a new session)",
            record["contract_id"].as_str().unwrap_or_default(),
            record["invalidation"]["message"]
                .as_str()
                .map(|message| format!(" ({message})"))
                .unwrap_or_default()
        )
    })
}
