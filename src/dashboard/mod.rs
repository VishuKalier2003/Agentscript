// The semantic control plane: six screens (Repository, Zones, Contracts, Agent Sessions, Policy
// Simulator, Attestations) over the repository's own metadata and Crane's records. The repository
// is connected once; afterwards every screen is computed from the connection, the inventory,
// zones, contracts, proposals, session journals, and attestations, through one API that the web
// dashboard and the CLI share, so they always agree. Creating or changing a contract only ever
// makes a pending proposal; activation stays a human approval.

pub(crate) mod editor;
pub(crate) mod server;
pub(crate) mod simulator;

#[cfg(test)] // Compile the module only when running tests, not in production builds
mod tests;

use std::fs;
use std::io::ErrorKind;

use serde_json::{json, Value};

use crate::evidence::{attest, records, verify_chain};
use crate::inventory::{discover, Options};
use crate::proposals::store::{agent_environment, Proposal};
use crate::repository::root;
use crate::session::{session_ids, ContractSession};
use crate::util::{io_error, sha256};

/** Version of the connection record and the API */
pub(crate) const API_FORMAT: u64 = 1;

/** The six screens and the endpoints that serve them */
pub(crate) const SCREENS: &[(&str, &str)] = &[
    ("Flow", "/api/flow"),
    ("Repository", "/api/repository"),
    ("Tasks", "/api/tasks"),
    ("Zones", "/api/zones"),
    ("Contracts", "/api/contracts"),
    ("Agent Sessions", "/api/sessions"),
    ("Policy Simulator", "/api/simulate"),
    ("Attestations", "/api/attestations"),
];

/** Read the repository connection (the registry in crate::repo)
 * Input
    - None
 * Output
    - Result<Option<Value>, String>
*/
pub(crate) fn connection() -> Result<Option<Value>, String> {
    crate::repo::load()
}

/** Connect the repository through the registry (see crate::repo::connect)
 * Input
    - refresh: bool - update the metadata of an existing connection
 * Output
    - Result<Value, String>
*/
pub(crate) fn connect(refresh: bool) -> Result<Value, String> {
    crate::repo::connect(&crate::repo::ConnectOptions {
        refresh,
        ..Default::default()
    })
}

/** The Repository screen: the semantic inventory, critical regions, policy coverage, and policy
 * targets that do not resolve
 * Input
    - None
 * Output
    - Result<Value, String>
*/
pub(crate) fn repository() -> Result<Value, String> {
    let inventory = discover(&Options { full: false })?.to_json();
    let zones = crate::zones::inspect().ok();
    let mut critical_zones = Vec::new();
    if let Some(zones) = &zones {
        for result in zones.to_json(None)["zones"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if matches!(
                result["criticality"].as_str(),
                Some("critical" | "restricted")
            ) {
                critical_zones.push(json!({"zone": result["zone_id"], "criticality": result["criticality"], "files": result["files"]}));
            }
        }
    }
    let clauses = inventory["contracts"]["clauses"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let targets = inventory["entities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entity| entity["test"] != true && !entity["policy_target"].is_null())
        .map(|entity| json!({"id": entity["id"], "kind": entity["kind"], "qualified": entity["qualified"], "file": entity["file"], "policy_target": entity["policy_target"], "risk_score": entity["risk_score"], "contracts": entity["contracts"]}))
        .collect::<Vec<_>>();
    Ok(json!({
        "screen": "repository",
        "connection": connection()?,
        "inventory": {
            "repository": inventory["repository"],
            "languages": inventory["languages"],
            "services": inventory["services"].as_array().map(|services| services.iter().map(|service| json!({"id": service["id"], "files": service["files"], "symbols": service["symbols"], "languages": service["languages"]})).collect::<Vec<_>>()),
            "modules": inventory["modules"].as_array().map(|modules| modules.iter().map(|module| json!({"id": module["id"], "files": module["files"], "symbols": module["symbols"]})).collect::<Vec<_>>()),
            "ownership": inventory["ownership"],
            "targets": targets,
        },
        "critical_regions": {"candidates": inventory["critical_candidates"], "zones": critical_zones},
        "policy_coverage": inventory["coverage"],
        "contract_version": inventory["contracts"]["contract_version"],
        "unresolved_targets": clauses.iter().filter(|clause| clause["status"] != "resolved").cloned().collect::<Vec<_>>(),
        "malformed_policies": inventory["contracts"]["malformed"],
    }))
}

/** The Zones screen: exactly what crane zones --json reports
 * Input
    - None
 * Output
    - Result<Value, String>
*/
pub(crate) fn zones() -> Result<Value, String> {
    Ok(crate::zones::inspect()?.to_json(None))
}

/** Read the active policies
 * Input
    - None
 * Output
    - Result<Vec<(String, String)>, String> names and texts, sorted
*/
fn active_policies() -> Result<Vec<(String, String)>, String> {
    let mut policies = Vec::new();
    match fs::read_dir(root()?.join("policies")) {
        Ok(entries) => {
            for entry in entries.filter_map(|entry| entry.ok()) {
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "crane")
                {
                    let name = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    policies.push((name, fs::read_to_string(&path).map_err(io_error)?));
                }
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(error)),
    }
    policies.sort();
    Ok(policies)
}

/** The Contracts screen: active policies (as editor drafts and AgentScript) and every proposal
 * Input
    - None
 * Output
    - Result<Value, String>
*/
pub(crate) fn contracts() -> Result<Value, String> {
    let active = active_policies()?
        .into_iter()
        .map(|(name, text)| json!({"name": name, "agentscript": text, "digest": sha256(text.as_bytes()), "draft": editor::draft(&text).ok(), "valid": editor::draft(&text).is_ok()}))
        .collect::<Vec<_>>();
    let proposals = Proposal::all()?
        .iter()
        .map(|proposal| json!({"name": proposal.name, "status": proposal.status(), "revision": proposal.document["revision"], "digest": proposal.digest(), "origin": proposal.document["origin"]}))
        .collect::<Vec<_>>();
    Ok(json!({
        "screen": "contracts",
        "contract_version": crate::ir::compile().ok().map(|set| set.version),
        "active": active,
        "proposals": proposals,
    }))
}

/** One contract: its AgentScript and draft, its version history (proposal revisions, the
 * retired copy, and the Git history of the active file), and its approval history
 * Input
    - name: &str - policy or proposal name
 * Output
    - Result<Value, String>
*/
pub(crate) fn contract(name: &str) -> Result<Value, String> {
    crate::util::validate_identifier(name)?;
    let crane = root()?;
    let active = fs::read_to_string(crane.join("policies").join(format!("{name}.crane"))).ok();
    let proposal = Proposal::load(name).ok();
    let retired = fs::read_to_string(crane.join("retired").join(format!("{name}.crane"))).ok();
    if active.is_none() && proposal.is_none() && retired.is_none() {
        return Err(format!("no contract or proposal named '{name}'"));
    }
    let candidate = proposal.as_ref().and_then(|proposal| {
        fs::read_to_string(
            crane
                .join("proposals")
                .join(format!("{}.crane", proposal.name)),
        )
        .ok()
    });
    let text = active
        .clone()
        .or(candidate.clone())
        .or(retired.clone())
        .unwrap_or_default();
    let history = proposal
        .as_ref()
        .map(|proposal| proposal.document["history"].clone())
        .unwrap_or(json!([]));
    let versions = history
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| matches!(entry["action"].as_str(), Some("generated" | "regenerated" | "edited")))
        .map(|entry| json!({"revision": entry["revision"], "action": entry["action"], "by": entry["actor"], "at": entry["at"], "digest": entry["policy_digest"]}))
        .collect::<Vec<_>>();
    let approvals = history
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| matches!(entry["action"].as_str(), Some("approved" | "rejected" | "superseded" | "retired")))
        .map(|entry| json!({"revision": entry["revision"], "action": entry["action"], "by": entry["actor"], "at": entry["at"], "digest": entry["policy_digest"], "detail": entry["detail"]}))
        .collect::<Vec<_>>();
    let repository = crane
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let git_history = std::process::Command::new("git")
        .args(["log", "--format=%H%x09%at%x09%an%x09%s", "--", &format!(".crane/policies/{name}.crane")])
        .current_dir(&repository)
        .output()
        .ok()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| {
                    let parts = line.splitn(4, '\t').collect::<Vec<_>>();
                    (parts.len() == 4).then(|| json!({"commit": parts[0], "at": parts[1].parse::<u64>().ok(), "author": parts[2], "subject": parts[3]}))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(json!({
        "screen": "contract",
        "name": name,
        "status": if active.is_some() { "active" } else if retired.is_some() { "retired" } else { proposal.as_ref().map_or("unknown", |proposal| proposal.status()) },
        "agentscript": text,
        "draft": editor::draft(&text).ok(),
        "active_digest": active.as_ref().map(|text| sha256(text.as_bytes())),
        "proposal": proposal.as_ref().map(|proposal| json!({"status": proposal.status(), "revision": proposal.document["revision"], "digest": proposal.digest(), "origin": proposal.document["origin"], "activation": proposal.document["activation"]})),
        "versions": versions,
        "approvals": approvals,
        "git_history": git_history,
    }))
}

/** Summarize a session for the Agent Sessions screen from its evidence
 * Input
    - session: &ContractSession - session
 * Output
    - Result<Value, String>
*/
fn session_summary(session: &ContractSession) -> Result<Value, String> {
    let attestation = attest(&session.document(), &session.events())?;
    Ok(json!({
        "session": session.id(),
        "agent": attestation["agent"],
        "task": attestation["task"],
        "lifecycle": session.lifecycle().name(),
        "autonomy": attestation["final_state"]["autonomy"],
        "safety": attestation["final_state"]["safety"],
        "budget": attestation["final_state"]["budget"],
        "actions": attestation["action_summary"],
        "denied": attestation["denied_actions"].as_array().map_or(0, Vec::len),
        "violations": attestation["violations"],
        "tests": {"contract": attestation["contract_tests"], "ordinary": attestation["ordinary_tests"]},
        "final_outcome": attestation["final_decision"],
        "delivery": attestation["delivery"],
    }))
}

/** The Agent Sessions screen: every session's behavior, autonomy, budget, actions, violations,
 * tests, and outcome
 * Input
    - None
 * Output
    - Result<Value, String>
*/
pub(crate) fn sessions() -> Result<Value, String> {
    let mut list = Vec::new();
    for id in session_ids()? {
        if let Ok(Some(session)) = ContractSession::load(&id) {
            list.push(session_summary(&session)?);
        }
    }
    Ok(json!({"screen": "sessions", "sessions": list}))
}

/** One session in full: its summary, autonomy status, budget, and evidence records
 * Input
    - id: &str - session id
 * Output
    - Result<Value, String>
*/
pub(crate) fn session(id: &str) -> Result<Value, String> {
    let session =
        ContractSession::load(id)?.ok_or_else(|| format!("no contract session '{id}'"))?;
    let mut value = session_summary(&session)?;
    value["autonomy_status"] = crate::autonomy::manage::status(&session);
    value["budget_detail"] = crate::budget::manage::status(&session);
    value["evidence"] = json!(records(&session.document(), &session.events())?);
    Ok(value)
}

/** The Attestations screen: every finalized session's attestation
 * Input
    - None
 * Output
    - Result<Value, String>
*/
pub(crate) fn attestations() -> Result<Value, String> {
    let mut list = Vec::new();
    for id in session_ids()? {
        let Ok(Some(session)) = ContractSession::load(&id) else {
            continue;
        };
        let stored = session
            .directory()
            .and_then(|directory| fs::read_to_string(directory.join("final_attestation.json")).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        if let Some(attestation) = stored {
            list.push(json!({
                "session": id,
                "task": attestation["task"]["task_id"],
                "digest": attestation["attestation_digest"],
                "decision": attestation["final_decision"]["decision"],
                "merged": attestation["delivery"]["merged"]["merge_sha"],
                "events": attestation["evidence"]["events"],
            }));
        }
    }
    Ok(json!({"screen": "attestations", "attestations": list}))
}

/** One attestation with its full evidence trail: the stored final attestation (or the one the
 * evidence gives now), the evidence records, the journal chain, and the delivery journal
 * Input
    - id: &str - session id
 * Output
    - Result<Value, String>
*/
pub(crate) fn attestation(id: &str) -> Result<Value, String> {
    let session =
        ContractSession::load(id)?.ok_or_else(|| format!("no contract session '{id}'"))?;
    let events = session.events();
    let derived = attest(&session.document(), &events)?;
    let stored = session
        .directory()
        .and_then(|directory| fs::read_to_string(directory.join("final_attestation.json")).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let delivery = crate::delivery::journal(id)
        .ok()
        .filter(|events| !events.is_empty())
        .map(|events| crate::delivery::state(&events));
    Ok(json!({
        "screen": "attestation",
        "session": id,
        "attestation": stored.clone().unwrap_or(derived.clone()),
        "finalized": stored.is_some(),
        "matches_evidence": stored.as_ref().is_none_or(|stored| stored["attestation_digest"] == derived["attestation_digest"]),
        "chain": verify_chain(&events),
        "evidence": records(&session.document(), &events)?,
        "delivery": delivery,
    }))
}

/** Read the policy a simulation request names: a draft, AgentScript, or a proposal
 * Input
    - body: &Value - {"draft"} | {"agentscript"} | {"proposal"}
 * Output
    - Result<crate::model::Policy, String>
*/
fn requested_policy(body: &Value) -> Result<crate::model::Policy, String> {
    let text = if !body["draft"].is_null() {
        editor::agentscript(&body["draft"]).map_err(|problems| problems.join("; "))?
    } else if let Some(text) = body["agentscript"].as_str() {
        text.to_string()
    } else if let Some(name) = body["proposal"].as_str() {
        let proposal = Proposal::load(name)?;
        fs::read_to_string(
            root()?
                .join("proposals")
                .join(format!("{}.crane", proposal.name)),
        )
        .map_err(io_error)?
    } else {
        return Err("send a draft, agentscript, or proposal to simulate".into());
    };
    crate::policy::parse(&text)
}

/** The Policy Simulator: a policy evaluated in shadow mode against every session's history and
 * today's repository
 * Input
    - body: &Value - the policy to simulate
 * Output
    - Result<Value, String>
*/
pub(crate) fn simulate(body: &Value) -> Result<Value, String> {
    let policy = requested_policy(body)?;
    let inventory = discover(&Options { full: false })?.to_json();
    let entities = inventory["entities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entity| entity["test"] != true && !entity["policy_target"].is_null())
        .map(|entity| {
            (
                entity["qualified"].as_str().unwrap_or_default().to_string(),
                entity["policy_target"]
                    .as_str()
                    .unwrap_or_default()
                    .trim_start_matches("--")
                    .split(' ')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                entity["file"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect::<Vec<_>>();
    let mut result = simulator::simulate(&policy, &simulator::histories()?, &entities);
    result["screen"] = json!("simulator");
    Ok(result)
}

/** Answer one API request: the single entry point the web dashboard and the CLI share
 * Input
    - method: &str - GET or POST
    - path: &str - request path such as /api/zones
    - body: &Value - JSON body (POST)
 * Output
    - (u16, Value) HTTP status and JSON
*/
pub(crate) fn route(method: &str, path: &str, body: &Value) -> (u16, Value) {
    let segments = path
        .trim_end_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let result = (|| -> Result<(u16, Value), (u16, String)> {
        let failed = |error: String| (422, error);
        if segments.first() != Some(&"api") {
            return Err((404, format!("no API at {path}")));
        }
        let rest = &segments[1..];
        if rest == ["connection"] {
            return match method {
                "GET" => Ok((200, connection().map_err(failed)?.unwrap_or(Value::Null))),
                "POST" => Ok((200, connect(body["refresh"] == true).map_err(failed)?)),
                _ => Err((405, "use GET or POST".into())),
            };
        }
        match connection().map_err(failed)? {
            None => return Err((409, "connect the repository first: crane repo connect (or POST /api/connection)".into())),
            Some(record) if record["connection_status"] != "connected" => {
                return Err((409, "the repository is disconnected: crane repo connect (or POST /api/connection) reconnects it".into()))
            }
            Some(_) => {}
        }
        if method == "POST" {
            if let Some(marker) = agent_environment() {
                return Err((403, format!("the dashboard API refuses changes in an agent environment ({marker} is set)")));
            }
        }
        let value = match (method, rest) {
            ("GET", ["repository"]) => repository().map_err(failed)?,
            ("GET", ["repo"]) => crate::repo::status().map_err(failed)?,
            ("GET", ["zones"]) => zones().map_err(failed)?,
            ("GET", ["zones", "recommendations"]) => {
                crate::zones::review::summary().map_err(failed)?
            }
            ("POST", ["zones", "recommendations"]) => {
                crate::zones::review::run(body["by"].as_str().map(String::from)).map_err(failed)?
            }
            ("GET", ["zones", "recommendations", id]) => {
                crate::zones::review::load(id).map_err(|error| (404, error))?
            }
            ("POST", ["zones", "recommendations", id, action]) => {
                let text = |key: &str| body[key].as_str().map(String::from);
                match *action {
                    "review" => crate::zones::review::claim(id, text("by")).map_err(failed)?,
                    "approve" => {
                        crate::zones::review::approve(id, text("approver"), text("confirm"))
                            .map_err(failed)?
                    }
                    "reject" => crate::zones::review::reject(id, text("approver"), text("reason"))
                        .map_err(failed)?,
                    other => {
                        return Err((404, format!("unknown zone recommendation action '{other}'")))
                    }
                }
            }
            ("GET", ["zones", "audit"]) => crate::zones::review::audit_log().map_err(failed)?,
            ("GET", ["contracts"]) => contracts().map_err(failed)?,
            ("GET", ["contracts", name]) => contract(name).map_err(|error| (404, error))?,
            ("POST", ["contracts", "preview"]) => match editor::agentscript(body) {
                Ok(text) => {
                    json!({"valid": true, "agentscript": text, "digest": sha256(text.as_bytes()), "problems": []})
                }
                Err(problems) => json!({"valid": false, "agentscript": null, "problems": problems}),
            },
            ("POST", ["contracts", "parse"]) => {
                json!({"draft": editor::draft(body["agentscript"].as_str().unwrap_or_default()).map_err(failed)?})
            }
            ("POST", ["contracts"]) => {
                let draft = &body["draft"];
                let text = match body["agentscript"].as_str() {
                    Some(text) => text.to_string(),
                    None => editor::agentscript(draft)
                        .map_err(|problems| failed(problems.join("; ")))?,
                };
                let policy = crate::policy::parse(&text).map_err(failed)?;
                Proposal::ensure_free(&policy.name).map_err(failed)?;
                let origin = json!({"kind": "editor", "editor": if body["agentscript"].is_string() { "advanced" } else { "visual" }, "by": body["by"]});
                let proposal = Proposal::create(
                    &policy.name,
                    &policy.checkpoint,
                    origin,
                    (
                        json!([]),
                        text,
                        json!([]),
                        json!({"generated_by": "crane dashboard"}),
                    ),
                    "crane dashboard editor",
                )
                .map_err(failed)?;
                return Ok((
                    201,
                    json!({"proposal": proposal.name, "status": proposal.status(), "digest": proposal.digest(), "activated": false}),
                ));
            }
            ("POST", ["contracts", name, action]) => {
                let mut proposal = Proposal::load(name).map_err(|error| (404, error))?;
                let text = |key: &str| body[key].as_str().map(String::from);
                match *action {
                    "approve" => {
                        proposal
                            .approve(text("approver"), text("confirm"))
                            .map_err(failed)?;
                    }
                    "reject" => proposal
                        .reject(text("approver"), text("reason"))
                        .map_err(failed)?,
                    "edit" => {
                        let directory = root().map_err(failed)?.join("runtime").join("dashboard");
                        fs::create_dir_all(&directory)
                            .map_err(|error| failed(error.to_string()))?;
                        let file = directory.join(format!("{name}.crane"));
                        fs::write(&file, text("agentscript").unwrap_or_default())
                            .map_err(|error| failed(error.to_string()))?;
                        proposal
                            .edit(Some(file.to_string_lossy().into_owned()), text("by"))
                            .map_err(failed)?;
                    }
                    other => return Err((404, format!("unknown contract action '{other}'"))),
                }
                contract(name).map_err(failed)?
            }
            ("GET", ["flow"]) => crate::flow::status(None).map_err(failed)?,
            ("GET", ["flow", "audit"]) => crate::flow::audit(None).map_err(failed)?,
            ("GET", ["flow", task]) => crate::flow::status(Some(task)).map_err(failed)?,
            ("POST", ["flow", task, "advance"]) => crate::flow::advance(
                task,
                body["now"] == true,
                body["by"].as_str().unwrap_or("crane dashboard"),
            )
            .map_err(failed)?,
            ("GET", ["tasks"]) => crate::intake::list().map_err(failed)?,
            ("GET", ["tasks", task]) if *task != "contracts" => {
                crate::intake::show(task).map_err(|error| (404, error))?
            }
            ("POST", ["tasks", task, action]) if *task != "contracts" => {
                let text = |key: &str| body[key].as_str().map(String::from);
                let by = text("by").unwrap_or_else(|| "crane dashboard".into());
                match *action {
                    "prepare" => {
                        crate::intake::prepare(task, text("checkpoint"), &by).map_err(failed)?
                    }
                    "approve" => crate::intake::approve(task, text("approver"), text("confirm"))
                        .map_err(failed)?,
                    "launch" => crate::intake::launch(
                        task,
                        text("agent"),
                        text("autonomy")
                            .map(|value| crate::zones::model::Autonomy::parse(&value))
                            .transpose()
                            .map_err(failed)?,
                        body["isolate"] == true,
                        &by,
                    )
                    .map_err(failed)?,
                    other => return Err((404, format!("unknown task action '{other}'"))),
                }
            }
            ("GET", ["tasks", "contracts"]) => crate::task_contracts::list().map_err(failed)?,
            ("GET", ["tasks", "contracts", task]) => crate::task_contracts::load(task, None)
                .map_err(failed)?
                .ok_or_else(|| (404, format!("task {task} has no contract")))?,
            ("GET", ["tasks", "contracts", task, "history"]) => {
                crate::task_contracts::history(task).map_err(failed)?
            }
            ("POST", ["tasks", "contracts", task, action]) => {
                let text = |key: &str| body[key].as_str().map(String::from);
                match *action {
                    "compile" => crate::task_contracts::compile(
                        task,
                        body["checkpoint"].as_str().unwrap_or("baseline"),
                        &text("by").map_or_else(
                            || "crane dashboard".to_string(),
                            |by| format!("{by} (crane dashboard)"),
                        ),
                    )
                    .map_err(failed)?,
                    "approve" => {
                        crate::task_contracts::approve(task, text("approver"), text("confirm"))
                            .map_err(failed)?
                    }
                    "reject" => {
                        crate::task_contracts::reject(task, text("approver"), text("reason"))
                            .map_err(failed)?
                    }
                    other => return Err((404, format!("unknown task contract action '{other}'"))),
                }
            }
            ("GET", ["policies", "activation"]) => {
                crate::task_contracts::activation::record().map_err(failed)?
            }
            ("GET", ["sessions"]) => sessions().map_err(failed)?,
            ("GET", ["sessions", id]) => session(id).map_err(|error| (404, error))?,
            ("POST", ["sessions", "run"]) => {
                let task = body["task"]
                    .as_str()
                    .ok_or((400, "body.task is required".to_string()))?;
                let driver = match body["actions"].as_array() {
                    Some(actions) => crate::session_orchestrator::Driver::Script(actions.clone()),
                    None => crate::session_orchestrator::Driver::Detach,
                };
                crate::session_orchestrator::run(
                    task,
                    body["agent"].as_str().map(String::from),
                    body["autonomy"]
                        .as_str()
                        .map(crate::zones::model::Autonomy::parse)
                        .transpose()
                        .map_err(failed)?,
                    driver,
                    body["approve"] == true,
                    body["by"].as_str().unwrap_or("crane dashboard"),
                )
                .map_err(failed)?
            }
            ("GET", ["sessions", id, "lifecycle"]) => crate::session_orchestrator::load(id)
                .map_err(failed)?
                .ok_or_else(|| {
                    (
                        404,
                        format!("session {id} is not governed by the orchestrator"),
                    )
                })?,
            ("POST", ["sessions", id, "finish"]) => crate::session_orchestrator::terminate(
                id,
                body["by"].as_str().unwrap_or("crane dashboard"),
            )
            .map_err(failed)?,
            ("POST", ["simulate"]) => simulate(body).map_err(failed)?,
            ("GET", ["attestations"]) => attestations().map_err(failed)?,
            ("GET", ["attestations", id]) => attestation(id).map_err(|error| (404, error))?,
            ("GET", ["packs"]) => json!({"packs": crate::packs::list()}),
            ("GET", ["packs", pack]) => {
                crate::packs::run(pack, body["checkpoint"].as_str().unwrap_or("baseline"))
                    .map_err(|error| (404, error))?
            }
            ("POST", ["packs", "payments", "propose"]) => {
                let proposal = crate::packs::propose(
                    body["name"].as_str().unwrap_or("payments_pack"),
                    body["checkpoint"].as_str().unwrap_or("baseline"),
                )
                .map_err(failed)?;
                return Ok((
                    201,
                    json!({"proposal": proposal.name, "status": proposal.status(), "digest": proposal.digest(), "activated": false}),
                ));
            }
            ("GET", ["screens"]) => {
                json!({"api_format": API_FORMAT, "screens": SCREENS.iter().map(|(name, endpoint)| json!({"screen": name, "endpoint": endpoint})).collect::<Vec<_>>()})
            }
            _ => return Err((404, format!("no API for {method} {path}"))),
        };
        Ok((200, value))
    })();
    match result {
        Ok(answer) => answer,
        Err((status, message)) => (status, json!({"error": message})),
    }
}
