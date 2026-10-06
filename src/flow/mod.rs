// The Society golden path: one lifecycle from repository connection to verified merge and task
// completion, composed from the existing subsystems. Every subsystem stays the source of truth for
// its own domain (the connection, discovery, zone review, policy activation, hooks, task intake,
// task contracts, the session orchestrator, delivery, and task completion); this module only reads
// them, derives one top-level stage with the identifiers that link each layer to the next, checks
// that those identifiers agree, records every stage change in a hash-chained audit log, and
// advances the steps that need no human decision. COMPLETED is reported only when every terminal
// condition holds in the subsystems themselves.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::adapter::AgentKind;
use crate::evidence::{link, verify_chain, GENESIS};
use crate::proposals::store::{agent_environment, Proposal};
use crate::repository::{git, load_checkpoint, root};
use crate::session::ContractSession;
use crate::session_orchestrator::Phase;
use crate::util::{io_error, now_unix};

/** The top-level stages: the fourteen steps of the golden path, and the failure branches
 * Variants
    - ConnectRepository .. Completed - the happy path, in order
    - NeedsClarification .. CompletionRetryPending - the failure branches
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    ConnectRepository,
    Discovering,
    ReviewRequired,
    PolicyApproval,
    AgentReady,
    TaskReady,
    Running,
    Verifying,
    DeliveryReady,
    Review,
    Merging,
    CompletingTask,
    Completed,
    NeedsClarification,
    Blocked,
    Denied,
    Quarantined,
    VerificationFailed,
    DeliveryFailed,
    MergeFailed,
    CompletionRetryPending,
}

impl Stage {
    /** Every stage: the happy path in order, then the failure branches */
    pub(crate) const ALL: [Stage; 21] = [
        Self::ConnectRepository,
        Self::Discovering,
        Self::ReviewRequired,
        Self::PolicyApproval,
        Self::AgentReady,
        Self::TaskReady,
        Self::Running,
        Self::Verifying,
        Self::DeliveryReady,
        Self::Review,
        Self::Merging,
        Self::CompletingTask,
        Self::Completed,
        Self::NeedsClarification,
        Self::Blocked,
        Self::Denied,
        Self::Quarantined,
        Self::VerificationFailed,
        Self::DeliveryFailed,
        Self::MergeFailed,
        Self::CompletionRetryPending,
    ];

    /** Return the stage's name
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ConnectRepository => "CONNECT_REPOSITORY",
            Self::Discovering => "DISCOVERING",
            Self::ReviewRequired => "REVIEW_REQUIRED",
            Self::PolicyApproval => "POLICY_APPROVAL",
            Self::AgentReady => "AGENT_READY",
            Self::TaskReady => "TASK_READY",
            Self::Running => "RUNNING",
            Self::Verifying => "VERIFYING",
            Self::DeliveryReady => "DELIVERY_READY",
            Self::Review => "REVIEW",
            Self::Merging => "MERGING",
            Self::CompletingTask => "COMPLETING_TASK",
            Self::Completed => "COMPLETED",
            Self::NeedsClarification => "NEEDS_CLARIFICATION",
            Self::Blocked => "BLOCKED",
            Self::Denied => "DENIED",
            Self::Quarantined => "QUARANTINED",
            Self::VerificationFailed => "VERIFICATION_FAILED",
            Self::DeliveryFailed => "DELIVERY_FAILED",
            Self::MergeFailed => "MERGE_FAILED",
            Self::CompletionRetryPending => "COMPLETION_RETRY_PENDING",
        }
    }

    /** Return the golden-path step a stage belongs to (1 to 14), the failure branches at the
     * step they stopped
     * Input
        - None (uses self)
     * Output
        - u64
    */
    pub(crate) fn step(self) -> u64 {
        match self {
            Self::ConnectRepository => 1,
            Self::Discovering => 2,
            Self::ReviewRequired => 3,
            Self::PolicyApproval => 4,
            Self::AgentReady => 5,
            Self::TaskReady | Self::NeedsClarification => 6,
            Self::Running | Self::Quarantined => 9,
            Self::Verifying | Self::VerificationFailed => 10,
            Self::DeliveryReady | Self::DeliveryFailed => 11,
            Self::Review | Self::Denied => 12,
            Self::Merging | Self::MergeFailed => 13,
            Self::CompletingTask | Self::CompletionRetryPending | Self::Completed => 14,
            Self::Blocked => 0,
        }
    }

    /** Check whether the stage is a failure branch
     * Input
        - None (uses self)
     * Output
        - bool
    */
    pub(crate) fn failure(self) -> bool {
        matches!(
            self,
            Self::NeedsClarification
                | Self::Blocked
                | Self::Denied
                | Self::Quarantined
                | Self::VerificationFailed
                | Self::DeliveryFailed
                | Self::MergeFailed
                | Self::CompletionRetryPending
        )
    }
}

/** Refuse a flow action on behalf of an agent
 * Input
    - action: &str - advance
 * Output
    - Result<(), String>
*/
fn require_human(action: &str) -> Result<(), String> {
    match agent_environment() {
        Some(marker) => Err(format!(
            "crane flow {action} refuses to run in an agent environment ({marker} is set); a human or the organization's process advances the flow"
        )),
        None => Ok(()),
    }
}

/** Return the flow directory, .crane/runtime/flow
 * Input
    - None
 * Output
    - Result<PathBuf, String>
*/
fn directory() -> Result<PathBuf, String> {
    Ok(root()?.join("runtime").join("flow"))
}

/** Build the view of a stage: its name, step, who acts next, and what to do
 * Input
    - stage: Stage - stage
    - reason: String - why the flow is here
    - next: &str - the next action
    - waiting_for: &str - human, agent, or society
 * Output
    - Value
*/
fn at(stage: Stage, reason: String, next: &str, waiting_for: &str) -> Value {
    json!({"stage": stage.name(), "step": stage.step(), "failure": stage.failure(), "reason": reason, "next": next, "waiting_for": waiting_for})
}

/** Read the repository setup layers (repository, discovery, policies, zones, agents) and the stage
 * the setup is at (TASK_READY once setup is complete)
 * Input
    - None
 * Output
    - Result<(Value, Value), String> layers and stage view
*/
fn setup() -> Result<(Value, Value), String> {
    let repository = crate::repo::status()?;
    let mut layers = json!({
        "repository": {
            "connected": repository["connection_status"] == "connected",
            "full_name": repository["full_name"],
            "provider": repository["provider"],
            "repository_id": repository["repository_id"],
            "default_branch": repository["default_branch"],
            "head": repository["head"],
            "trusted_checkpoint": repository["trusted_checkpoint"],
        },
    });
    if repository["connection_status"] != "connected" {
        return Ok((
            layers,
            at(
                Stage::ConnectRepository,
                "the repository is not connected".into(),
                "crane repo connect",
                "human",
            ),
        ));
    }
    layers["discovery"] =
        json!({"state": repository["discovery"]["state"], "last": repository["discovery"]["last"]});
    let activation = crate::task_contracts::activation::current()?;
    let pending_policies = Proposal::all()?
        .into_iter()
        .filter(|proposal| {
            proposal.status() == "pending" && proposal.document["origin"]["kind"] != "task"
        })
        .map(|proposal| proposal.name)
        .collect::<Vec<_>>();
    let persistent = activation["policies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|policy| policy["layer"] != "task")
        .filter_map(|policy| policy["policy"].as_str().map(String::from))
        .collect::<Vec<_>>();
    layers["policies"] = json!({"policy_version": activation["policy_version"], "active": persistent, "pending": pending_policies});
    let review = crate::zones::review::summary()?;
    let pending_zones = review["recommendations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| matches!(item["status"].as_str(), Some("proposed" | "in_review")))
        .filter_map(|item| item["id"].as_str().map(String::from))
        .collect::<Vec<_>>();
    let (zones, _) = crate::zones::model::load(&root()?)?;
    layers["zones"] = json!({
        "zone_set_version": crate::zones::model::set_version(&zones),
        "active": zones.iter().map(|zone| zone.zone_id.clone()).collect::<Vec<_>>(),
        "recommendation_runs": review["runs"],
        "pending": pending_zones,
    });
    let agents = [AgentKind::Claude, AgentKind::Codex]
        .into_iter()
        .map(|kind| {
            let report = crate::commands::validate_hooks(kind).unwrap_or_else(|error| json!({"valid": false, "problems": [error]}));
            json!({"profile": kind.name(), "valid": report["valid"], "problems": report["problems"]})
        })
        .collect::<Vec<_>>();
    let connected = agents
        .iter()
        .filter(|agent| agent["valid"] == true)
        .filter_map(|agent| agent["profile"].as_str())
        .collect::<Vec<_>>();
    layers["agents"] = json!({"connected": connected, "providers": agents});
    let stage = if repository["discovery"]["state"] == "never" {
        at(
            Stage::Discovering,
            "the repository was never discovered".into(),
            "crane repo connect --refresh",
            "human",
        )
    } else if review["runs"] == 0 {
        at(
            Stage::ReviewRequired,
            "no zones were recommended yet".into(),
            "crane zones recommend",
            "human",
        )
    } else if !pending_zones.is_empty() {
        at(
            Stage::ReviewRequired,
            format!(
                "zone recommendations wait for review: {}",
                pending_zones.join(", ")
            ),
            "crane zones review ID, then crane zones approve|reject ID",
            "human",
        )
    } else if !pending_policies.is_empty() {
        at(
            Stage::PolicyApproval,
            format!(
                "policy proposals wait for approval: {}",
                pending_policies.join(", ")
            ),
            "crane policy show NAME, then crane policy approve NAME",
            "human",
        )
    } else if persistent.is_empty() {
        at(
            Stage::PolicyApproval,
            "no organization or repository policy is active".into(),
            "crane policy propose (or crane packs), then crane policy approve NAME",
            "human",
        )
    } else if connected.is_empty() {
        at(
            Stage::AgentReady,
            "no agent is connected (Claude Code or Codex hooks are not installed and valid)".into(),
            "crane agent install --profile claude|codex, then crane agent hooks --profile ...",
            "human",
        )
    } else {
        at(
            Stage::TaskReady,
            format!("setup is complete; {} connected", connected.join(" and ")),
            "crane task list, then crane task prepare ID",
            "human",
        )
    };
    Ok((layers, stage))
}

/** Read the task layers: task, contract, agent, session, authority, verification, attestation,
 * pull request, approval, merge, and completion
 * Input
    - task: &str - task id
 * Output
    - Result<Value, String>
*/
fn task_layers(task: &str) -> Result<Value, String> {
    let intake = crate::intake::show(task)
        .unwrap_or_else(|error| json!({"task_id": task, "state": null, "error": error}));
    let contract = crate::task_contracts::load(task, None)?;
    let launch = &intake["launch"];
    let session_id = launch["session_id"].as_str().map(String::from).or_else(|| {
        crate::orchestration::TaskRecord::load(task)
            .ok()
            .flatten()
            .and_then(|record| {
                record.value["sessions"]
                    .as_array()
                    .and_then(|sessions| sessions.last())
                    .and_then(|id| id.as_str().map(String::from))
            })
    });
    let session = session_id
        .as_deref()
        .and_then(|id| ContractSession::load(id).ok().flatten());
    let orchestrated = session_id
        .as_deref()
        .and_then(|id| crate::session_orchestrator::load(id).ok().flatten());
    let delivery = session_id
        .as_deref()
        .filter(|id| {
            crate::delivery::journal(id)
                .map(|events| !events.is_empty())
                .unwrap_or(false)
        })
        .and_then(|id| crate::delivery::status(id).ok());
    let completion = crate::task_completion::for_task(task)?;
    let report = session.as_ref().map(crate::agent_session::status_report);
    Ok(json!({
        "task": {"task_id": task, "source": intake["source"], "external_id": intake["external_id"], "title": intake["title"], "intake_state": intake["state"], "reason": intake["reason"]},
        "contract": contract.as_ref().map(|contract| json!({"contract_id": contract["contract_id"], "version": contract["version"], "status": contract["status"], "digest": contract["digest"], "checkpoint": contract["bindings"]["checkpoint"], "policy_version": contract["bindings"]["policy_version"], "zone_set_version": contract["bindings"]["zone_set_version"]})),
        "agent": report.as_ref().map(|report| json!({"profile": report["agent"], "attachments": report["attachments"]})),
        "session": report.as_ref().map(|report| json!({"session_id": report["session_id"], "lifecycle": report["lifecycle"], "binding_digest": report["binding_digest"], "task_contract": report["task_contract"], "phase": orchestrated.as_ref().map(|record| record["phase"].clone()), "orchestrator_binding_digest": orchestrated.as_ref().map(|record| record["binding_digest"].clone())})),
        "authority": report.as_ref().map(|report| json!({"autonomy": report["autonomy"], "safety": report["safety"], "safety_reason": report["safety_reason"], "budget": report["budget"], "decisions": orchestrated.as_ref().map(|record| record["actions"].clone()), "last_event": report["last_event"]})),
        "verification": orchestrated.as_ref().map(|record| json!({"phase": record["phase"], "final_status": record["termination"]["final_status"], "checks": record["termination"]["checks"], "failures": record["termination"]["failures"]})),
        "attestation": orchestrated.as_ref().map(|record| json!({"digest": record["termination"]["attestation_digest"]})),
        "pull_request": delivery.as_ref().map(|delivery| json!({"number": delivery["pull_request"]["number"], "url": delivery["pull_request"]["url"], "provider": delivery["pull_request"]["provider"], "head": delivery["head"], "binding_digest": delivery["binding_digest"], "contract_digest": delivery["binding"]["contract_digest"], "attestation": delivery["binding"]["attestation"]})),
        "approval": delivery.as_ref().map(|delivery| json!({"delivery_state": delivery["delivery_state"], "approvals": delivery["eligibility"]["approvals"], "missing": delivery["eligibility"]["missing"], "rule": delivery["eligibility"]["rule"]["name"]})),
        "merge": delivery.as_ref().filter(|delivery| !delivery["merged"].is_null()).map(|delivery| json!({"sha": delivery["merged"]["sha"], "verified": delivery["merged"]["verified"], "trusted_checkpoint": delivery["merged"]["trusted_checkpoint"], "head": delivery["merged"]["head"]})),
        "task_completion": completion.as_ref().map(|event| json!({"event_id": event["event_id"], "task_id": event["task_id"], "status": event["status"], "tracker": event["tracker"], "external_id": event["external_id"], "merge_sha": event["merge_sha"], "contract_digest": event["contract_digest"], "attestation": event["attestation"], "attempts": event["attempts"], "last_error": event["last_error"]})),
        "_delivery": delivery,
        "_orchestrated": orchestrated,
    }))
}

/** Check that the identifiers linking the layers agree: the task's contract digest is the one the
 * session, the orchestrator, the pull request, and the completion are bound to; the session is the
 * delivery's; the merge commit is the completion's and the trusted checkpoint's; the attestation the
 * merge relied on is the completion's
 * Input
    - layers: &Value - task layers
 * Output
    - Value {consistent, checks}
*/
fn links(layers: &Value) -> Value {
    let mut checks = Vec::new();
    let mut check = |name: &str, values: Vec<&Value>| {
        let present = values
            .iter()
            .filter(|value| !value.is_null())
            .collect::<Vec<_>>();
        let consistent = present.windows(2).all(|pair| pair[0] == pair[1]);
        checks.push(json!({"link": name, "consistent": consistent, "values": present}));
    };
    let contract = &layers["contract"]["digest"];
    check(
        "contract digest",
        vec![
            contract,
            &layers["session"]["task_contract"]["digest"],
            &layers["pull_request"]["contract_digest"],
            &layers["task_completion"]["contract_digest"],
        ],
    );
    check(
        "orchestrator contract digest",
        vec![
            contract,
            &layers["_orchestrated"]["binding"]["contract_digest"],
        ],
    );
    check(
        "session",
        vec![
            &layers["session"]["session_id"],
            &layers["_delivery"]["session"],
            &layers["_orchestrated"]["session_id"],
        ],
    );
    check(
        "task",
        vec![
            &layers["task"]["task_id"],
            &layers["_delivery"]["task"],
            &layers["task_completion"]["task_id"],
            &layers["_orchestrated"]["task_id"],
        ],
    );
    check(
        "merge commit",
        vec![
            &layers["merge"]["sha"],
            &layers["task_completion"]["merge_sha"],
            &layers["_orchestrated"]["delivery"]["merge_sha"],
        ],
    );
    check(
        "attestation",
        vec![
            &layers["pull_request"]["attestation"],
            &layers["task_completion"]["attestation"],
        ],
    );
    let consistent = checks.iter().all(|check| check["consistent"] == true);
    json!({"consistent": consistent, "checks": checks})
}

/** Check the terminal conditions of COMPLETED in the subsystems themselves: the completion is
 * confirmed for this task; the delivery merged and verified that the merge contains the checked
 * commit; the merge commit is in the repository and is the trusted checkpoint's commit; a governed
 * session recorded the delivery; an orchestrated task record completed; and every link agrees
 * Input
    - layers: &Value - task layers
    - links: &Value - link checks
 * Output
    - Value {met, conditions}
*/
fn terminal(layers: &Value, links: &Value) -> Value {
    let merge = &layers["merge"];
    let sha = merge["sha"].as_str().unwrap_or_default();
    let in_repository =
        !sha.is_empty() && git(&["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_ok();
    let contains_head = merge["head"]
        .as_str()
        .is_some_and(|head| git(&["merge-base", "--is-ancestor", head, sha]).is_ok());
    let checkpoint = merge["trusted_checkpoint"]
        .as_str()
        .and_then(|name| load_checkpoint(name).ok());
    let task = layers["task"]["task_id"].as_str().unwrap_or_default();
    let record = crate::orchestration::TaskRecord::load(task).ok().flatten();
    let conditions = vec![
        json!({"condition": "the tracker completion is confirmed", "met": matches!(layers["task_completion"]["status"].as_str(), Some("completed" | "completed_externally"))}),
        json!({"condition": "the change merged and the merge was verified", "met": merge["verified"] == true}),
        json!({"condition": "the merge commit is in the repository and contains the checked commit", "met": in_repository && contains_head}),
        json!({"condition": "the merge commit is the trusted checkpoint", "met": checkpoint.as_ref().is_some_and(|checkpoint| checkpoint.commit == sha)}),
        json!({"condition": "the governed session recorded the delivery", "met": layers["_orchestrated"].is_null() || layers["_orchestrated"]["delivery"]["merge_sha"] == sha}),
        json!({"condition": "the task lifecycle completed", "met": record.as_ref().is_none_or(|record| record.value["state"] == "COMPLETED")}),
        json!({"condition": "every identifier links", "met": links["consistent"] == true}),
    ];
    json!({"met": conditions.iter().all(|condition| condition["met"] == true), "conditions": conditions})
}

/** Derive a task's stage from its layers (each subsystem decides its own part)
 * Input
    - layers: &Value - task layers
    - links: &Value - link checks
 * Output
    - Value stage view
*/
fn task_stage(layers: &Value, links: &Value) -> Value {
    let session = layers["session"]["session_id"]
        .as_str()
        .unwrap_or("SESSION")
        .to_string();
    let task = layers["task"]["task_id"]
        .as_str()
        .unwrap_or("TASK")
        .to_string();
    if links["consistent"] == false {
        let broken = links["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|check| check["consistent"] == false)
            .filter_map(|check| check["link"].as_str())
            .collect::<Vec<_>>();
        return at(
            Stage::Blocked,
            format!(
                "the lifecycle's identifiers do not agree: {}",
                broken.join(", ")
            ),
            "inspect with crane flow status --json; a human must repair the records",
            "human",
        );
    }
    if let Some(status) = layers["task_completion"]["status"].as_str() {
        return match status {
            "completed" | "completed_externally" => {
                let terminal = terminal(layers, links);
                if terminal["met"] == true {
                    at(
                        Stage::Completed,
                        format!(
                            "merged as {} and completed in the tracker",
                            layers["merge"]["sha"].as_str().unwrap_or_default()
                        ),
                        "nothing; the task is done",
                        "nobody",
                    )
                } else {
                    let missing = terminal["conditions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|condition| condition["met"] != true)
                        .filter_map(|condition| condition["condition"].as_str())
                        .collect::<Vec<_>>();
                    at(Stage::Blocked, format!("the completion is recorded, but not every terminal condition holds: {}", missing.join("; ")), "crane task completions reconcile --send", "human")
                }
            }
            "retry_pending" => at(
                Stage::CompletionRetryPending,
                format!(
                    "the tracker could not be reached: {}",
                    layers["task_completion"]["last_error"]
                        .as_str()
                        .unwrap_or("unavailable")
                ),
                &format!("crane flow advance {task} (retries after the backoff)"),
                "society",
            ),
            _ => at(
                Stage::CompletingTask,
                "the merge is verified; the tracker completion is queued".into(),
                &format!("crane flow advance {task}"),
                "society",
            ),
        };
    }
    if !layers["merge"].is_null() {
        return at(
            Stage::CompletingTask,
            "merged; the completion event is not queued yet".into(),
            &format!("crane flow advance {task}"),
            "society",
        );
    }
    if let Some(state) = layers["approval"]["delivery_state"].as_str() {
        return match state {
            "MERGE_FAILED" => at(
                Stage::MergeFailed,
                "the merge failed; nothing was completed".into(),
                &format!("resolve the merge, then crane flow advance {task}"),
                "human",
            ),
            "REJECTED" => at(
                Stage::Denied,
                "a reviewer rejected the pull request".into(),
                "change the work and deliver again",
                "human",
            ),
            "APPROVED" => at(
                Stage::Merging,
                "the merge policy is satisfied".into(),
                &format!("crane flow advance {task}"),
                "society",
            ),
            "BLOCKED" => at(
                Stage::DeliveryFailed,
                "the delivery did not submit a pull request".into(),
                &format!("crane deliver status {session}"),
                "human",
            ),
            _ => at(
                Stage::Review,
                format!(
                    "waiting for review: {}",
                    layers["approval"]["missing"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                &format!("approve in Slack or crane deliver approve {session} --approver NAME"),
                "human",
            ),
        };
    }
    if let Some(phase) = layers["session"]["phase"]
        .as_str()
        .and_then(|phase| Phase::parse(phase).ok())
    {
        let quarantined = layers["authority"]["safety"] == "quarantined";
        return match phase {
            Phase::DeliveryReady => at(Stage::DeliveryReady, "the change is verified and ready to deliver".into(), &format!("crane flow advance {task}"), "society"),
            Phase::Verified => at(Stage::Blocked, "verified, but the session changed nothing to deliver".into(), "plan the task again", "human"),
            Phase::Failed if quarantined => at(Stage::Quarantined, format!("the session was quarantined: {}", layers["authority"]["safety_reason"].as_str().unwrap_or_default()), "review the session; run the task again", "human"),
            Phase::Failed => at(Stage::VerificationFailed, layers["verification"]["failures"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("; "), "review the session; run the task again", "human"),
            Phase::Stopping | Phase::Reconciling => at(Stage::Verifying, "the session is being reconciled and tested".into(), "wait", "society"),
            Phase::Quarantined => at(Stage::Quarantined, format!("the session is quarantined: {}", layers["authority"]["safety_reason"].as_str().unwrap_or_default()), &format!("crane autonomy status {session}"), "human"),
            _ => at(Stage::Running, format!("session {session} is {}", phase.name()), &format!("the agent works (CRANE_SESSION={session}); then crane session finish {session}"), "agent"),
        };
    }
    if !layers["session"].is_null() {
        let stage = match (
            layers["session"]["lifecycle"].as_str(),
            layers["authority"]["safety"].as_str(),
        ) {
            (_, Some("quarantined")) => Stage::Quarantined,
            (Some("finalized"), _) => Stage::DeliveryReady,
            _ => Stage::Running,
        };
        return at(
            stage,
            format!(
                "session {session} is {}",
                layers["session"]["lifecycle"].as_str().unwrap_or_default()
            ),
            &format!("crane session finish {session}"),
            "agent",
        );
    }
    match layers["task"]["intake_state"].as_str() {
        Some("NEEDS_CLARIFICATION") => at(Stage::NeedsClarification, layers["task"]["reason"].as_str().unwrap_or_default().to_string(), "clarify the task in the tracker, then crane task prepare ID", "human"),
        Some("BLOCKED" | "FAILED") => match layers["contract"]["status"].as_str() {
            Some("rejected") => at(Stage::Denied, "the task contract was rejected".into(), "plan the task again", "human"),
            _ => at(Stage::Blocked, layers["task"]["reason"].as_str().unwrap_or_default().to_string(), &format!("crane task show {task}"), "human"),
        },
        Some("CONTRACT_PENDING_APPROVAL") => at(Stage::TaskReady, "the task contract waits for approval".into(), &format!("crane task show {task}, then crane task approve {task} --approver NAME --confirm DIGEST"), "human"),
        Some("READY") => at(Stage::TaskReady, "the contract is approved; start the agent".into(), &format!("crane session run {task} --agent claude|codex"), "human"),
        Some("COMPLETED") => at(Stage::Blocked, "the tracker issue is done, but Society merged nothing for it".into(), "nothing to do", "nobody"),
        _ => at(Stage::TaskReady, "the task is not planned yet".into(), &format!("crane task prepare {task}"), "human"),
    }
}

/** Record the current stage of a scope when it changed: the snapshot in .crane/runtime/flow/
 * state.json and a transition in the hash-chained .crane/runtime/flow/audit.jsonl; nothing is
 * written on behalf of an agent, and an unchanged stage writes nothing (idempotent)
 * Input
    - scope: &str - "repository" or a task id
    - view: &Value - stage view
    - identifiers: Value - the identifiers the stage is about
 * Output
    - Result<bool, String> whether a transition was recorded
*/
fn remember(scope: &str, view: &Value, identifiers: Value) -> Result<bool, String> {
    // Before Crane is initialized there is nowhere to record anything (and nothing happened yet)
    if agent_environment().is_some() || root().is_err() {
        return Ok(false);
    }
    let folder = directory()?;
    fs::create_dir_all(&folder).map_err(io_error)?;
    let path = folder.join("state.json");
    let mut state = match fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str::<Value>(&text).map_err(|error| format!("flow state: {error}"))?
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            json!({"flow_format": 1, "scopes": {}})
        }
        Err(error) => return Err(io_error(error)),
    };
    let previous = state["scopes"][scope]["stage"].clone();
    if previous == view["stage"] {
        return Ok(false);
    }
    let events = audit_events()?;
    let last = events
        .last()
        .and_then(|event| event["chain"].as_str().map(String::from))
        .unwrap_or_else(|| GENESIS.to_string());
    let from = previous
        .as_str()
        .and_then(|name| Stage::ALL.into_iter().find(|stage| stage.name() == name));
    let to = Stage::ALL
        .into_iter()
        .find(|stage| view["stage"] == stage.name());
    let regression = matches!((from, to), (Some(from), Some(to)) if !from.failure() && !to.failure() && to.step() < from.step() && from != Stage::TaskReady);
    let mut event = json!({
        "seq": events.len() + 1,
        "at": now_unix(),
        "scope": scope,
        "from": previous,
        "to": view["stage"],
        "reason": view["reason"],
        "regression": regression,
        "identifiers": identifiers,
    });
    event["chain"] = json!(link(&last, &event));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(folder.join("audit.jsonl"))
        .map_err(io_error)?;
    writeln!(file, "{event}").map_err(io_error)?;
    state["scopes"][scope] =
        json!({"stage": view["stage"], "since": now_unix(), "reason": view["reason"]});
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_string_pretty(&state).map_err(io_error)? + "\n",
    )
    .map_err(io_error)?;
    fs::rename(&temporary, &path).map_err(io_error)?;
    Ok(true)
}

/** Read the flow audit log
 * Input
    - None
 * Output
    - Result<Vec<Value>, String>
*/
fn audit_events() -> Result<Vec<Value>, String> {
    match fs::read_to_string(directory()?.join("audit.jsonl")) {
        Ok(text) => text
            .lines()
            .map(|line| serde_json::from_str(line).map_err(|error| format!("flow audit: {error}")))
            .collect(),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(io_error(error)),
    }
}

/** Return the flow audit log with its chain verification, optionally for one scope
 * Input
    - scope: Option<&str> - "repository" or a task id
 * Output
    - Result<Value, String>
*/
pub(crate) fn audit(scope: Option<&str>) -> Result<Value, String> {
    let events = audit_events()?;
    Ok(json!({
        "chain": verify_chain(&events),
        "events": events.iter().filter(|event| scope.is_none_or(|scope| event["scope"] == scope)).collect::<Vec<_>>(),
    }))
}

/** Report the golden path: the repository setup layers and stage, and for a task (the one given,
 * or every task of the repository) its layers from task to completion, the link checks, the
 * terminal conditions, and its stage; the top-level stage is the task's when one is given or in
 * progress, the setup's otherwise. Stage changes are recorded (auditable, idempotent)
 * Input
    - task: Option<&str> - task id
 * Output
    - Result<Value, String>
*/
pub(crate) fn status(task: Option<&str>) -> Result<Value, String> {
    let (layers, setup_stage) = setup()?;
    remember(
        "repository",
        &setup_stage,
        json!({"repository": layers["repository"]["repository_id"], "policy_version": layers["policies"]["policy_version"], "zone_set_version": layers["zones"]["zone_set_version"]}),
    )?;
    let mut tasks = Vec::new();
    let ids = match task {
        Some(task) => vec![task.to_string()],
        None if layers["repository"]["connected"] == true => crate::intake::list()
            .map(|list| {
                list["tasks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| item["task_id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };
    for id in ids {
        let mut view = task_layers(&id)?;
        let links = links(&view);
        let mut stage = task_stage(&view, &links);
        let conditions = (!view["task_completion"].is_null()).then(|| terminal(&view, &links));
        // Before its contract is compiled, a task waits for the repository setup to finish
        let setup_open = setup_stage["stage"] != Stage::TaskReady.name();
        if setup_open && view["contract"].is_null() && view["session"].is_null() {
            stage = setup_stage.clone();
        }
        remember(
            &id,
            &stage,
            json!({
                "task": id,
                "contract_digest": view["contract"]["digest"],
                "session": view["session"]["session_id"],
                "pull_request": view["pull_request"]["number"],
                "binding_digest": view["pull_request"]["binding_digest"],
                "merge_sha": view["merge"]["sha"],
                "completion_event": view["task_completion"]["event_id"],
            }),
        )?;
        if let Some(object) = view.as_object_mut() {
            object.remove("_delivery");
            object.remove("_orchestrated");
        }
        view["links"] = links.clone();
        view["terminal"] = json!(conditions);
        view["stage"] = stage;
        tasks.push(view);
    }
    let active = tasks.iter().find(|view| {
        task.is_some()
            || !matches!(
                view["stage"]["stage"].as_str(),
                Some("TASK_READY" | "COMPLETED") | None
            )
            || view["stage"]["stage"] == setup_stage["stage"]
    });
    let current = match (task, active) {
        (Some(_), Some(view)) => view["stage"].clone(),
        (None, Some(view)) if view["stage"]["stage"] != Stage::TaskReady.name() => {
            let mut stage = view["stage"].clone();
            stage["task"] = view["task"]["task_id"].clone();
            stage
        }
        _ => setup_stage.clone(),
    };
    Ok(json!({
        "flow_format": 1,
        "stage": current["stage"],
        "current": current,
        "setup": setup_stage,
        "layers": layers,
        "tasks": tasks,
        "statement": "One approved engineering task can be executed by an AI agent under Society governance and automatically progress from repository connection to verified merge and task completion.",
    }))
}

/** Advance a task through the steps that need no human decision, until a human, the agent, or
 * a retry backoff is next: recover completions after a restart, deliver a verified change, merge
 * once the merge policy is satisfied, and complete the tracker issue; each step is the owning
 * subsystem's own operation, so advancing again changes nothing; refused on behalf of an agent
 * Input
    - task: &str - task id
    - now: bool - ignore the completion retry backoff
    - by: &str - who advances
 * Output
    - Result<Value, String> {steps, status}
*/
pub(crate) fn advance(task: &str, now: bool, by: &str) -> Result<Value, String> {
    require_human("advance")?;
    let mut steps = Vec::new();
    // Restart recovery: completion events lost or left mid-send are recovered first
    crate::task_completion::reconcile(false)?;
    for _ in 0..6 {
        let view = status(Some(task))?;
        let stage = view["stage"].as_str().unwrap_or_default().to_string();
        let session = view["tasks"][0]["session"]["session_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let outcome = match stage.as_str() {
            "DELIVERY_READY" => crate::delivery::run(&session)
                .map(|_| "delivered: commit, tests, checks, pull request, Slack"),
            "MERGING" => {
                crate::delivery::merge(&session, by).map(|_| "merged and trusted the merge commit")
            }
            "COMPLETING_TASK" | "COMPLETION_RETRY_PENDING" => {
                crate::task_completion::reconcile(false)?;
                crate::task_completion::dispatch(now, Some(task))
                    .map(|_| "sent the tracker completion")
            }
            _ => break,
        };
        let after = status(Some(task))?["stage"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        steps.push(json!({"from": stage, "to": after, "action": outcome.as_ref().map(|text| text.to_string()).unwrap_or_else(|error| format!("failed: {error}"))}));
        if after == stage || outcome.is_err() {
            break;
        }
    }
    Ok(json!({"steps": steps, "status": status(Some(task))?}))
}

/** Render the golden path for a terminal
 * Input
    - value: &Value - status
 * Output
    - String
*/
pub(crate) fn render(value: &Value) -> String {
    let text = |value: &Value| match value {
        Value::Null => "-".to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let short = |value: &Value| {
        text(value)
            .trim_start_matches("sha256:")
            .chars()
            .take(12)
            .collect::<String>()
    };
    let layers = &value["layers"];
    let current = &value["current"];
    let mut out = format!(
        "Stage: {} (step {} of 14){}\n  {}\n  next ({}): {}\n\n",
        text(&current["stage"]),
        current["step"],
        if current["failure"] == true {
            " - FAILURE BRANCH"
        } else {
            ""
        },
        text(&current["reason"]),
        text(&current["waiting_for"]),
        text(&current["next"])
    );
    out.push_str(&format!(
        "Repository      {} ({}), trusted checkpoint {}\n",
        text(&layers["repository"]["full_name"]),
        text(&layers["repository"]["provider"]),
        text(&layers["repository"]["trusted_checkpoint"]["name"])
    ));
    out.push_str(&format!(
        "Discovery       {}\n",
        text(&layers["discovery"]["state"])
    ));
    out.push_str(&format!(
        "Policy          version {}, active [{}], pending [{}]\n",
        short(&layers["policies"]["policy_version"]),
        layers["policies"]["active"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect::<Vec<_>>()
            .join(", "),
        layers["policies"]["pending"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str(&format!(
        "Zones           version {}, active [{}], pending review [{}]\n",
        short(&layers["zones"]["zone_set_version"]),
        layers["zones"]["active"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect::<Vec<_>>()
            .join(", "),
        layers["zones"]["pending"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str(&format!(
        "Agents          connected [{}]\n",
        layers["agents"]["connected"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect::<Vec<_>>()
            .join(", ")
    ));
    for task in value["tasks"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "\nTask            {} {} [{}]\n",
            text(&task["task"]["task_id"]),
            text(&task["task"]["title"]),
            text(&task["stage"]["stage"])
        ));
        out.push_str(&format!(
            "  Contract      {} {} {}\n",
            text(&task["contract"]["contract_id"]),
            text(&task["contract"]["status"]),
            short(&task["contract"]["digest"])
        ));
        out.push_str(&format!(
            "  Agent         {}\n",
            text(&task["agent"]["profile"])
        ));
        out.push_str(&format!(
            "  Session       {} {} phase {}\n",
            text(&task["session"]["session_id"]),
            text(&task["session"]["lifecycle"]),
            text(&task["session"]["phase"])
        ));
        out.push_str(&format!(
            "  Authority     autonomy {}, safety {}, decisions {}\n",
            text(&task["authority"]["autonomy"]),
            text(&task["authority"]["safety"]),
            text(&task["authority"]["decisions"])
        ));
        out.push_str(&format!(
            "  Verification  {}\n",
            text(&task["verification"]["final_status"])
        ));
        out.push_str(&format!(
            "  Attestation   {}\n",
            short(&task["attestation"]["digest"])
        ));
        out.push_str(&format!(
            "  PR            #{} {} binding {}\n",
            text(&task["pull_request"]["number"]),
            text(&task["pull_request"]["url"]),
            short(&task["pull_request"]["binding_digest"])
        ));
        out.push_str(&format!(
            "  Approval      {} {}\n",
            text(&task["approval"]["delivery_state"]),
            text(&task["approval"]["approvals"]["by"])
        ));
        out.push_str(&format!(
            "  Merge         {} checkpoint {}\n",
            short(&task["merge"]["sha"]),
            text(&task["merge"]["trusted_checkpoint"])
        ));
        out.push_str(&format!(
            "  Completion    {} {} ({} {})\n",
            text(&task["task_completion"]["event_id"]),
            text(&task["task_completion"]["status"]),
            text(&task["task_completion"]["tracker"]),
            text(&task["task_completion"]["external_id"])
        ));
        out.push_str(&format!(
            "  Links         {}\n",
            if task["links"]["consistent"] == true {
                "consistent"
            } else {
                "INCONSISTENT"
            }
        ));
    }
    out
}
