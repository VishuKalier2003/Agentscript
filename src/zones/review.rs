// Zone review: the human gate between what discovery recommends and what governs agents. Every
// recommendation is a versioned proposal in .crane/zone-proposals (never in .crane/zones, so it
// governs nothing) moving PROPOSED -> IN_REVIEW -> APPROVED or REJECTED (WITHDRAWN when its
// evidence disappears). Approval is human-only, idempotent, confirmed against the reviewed
// digest, and writes an ordinary .crane/zones/ZONE.zone, which is what makes it active; every
// decision is an event in a hash-chained audit log.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

use super::model::{load as load_zones, parse, set_version};
use super::recommend::{recommend, RECOMMENDER};
use crate::evidence::{link, verify_chain, GENESIS};
use crate::inventory::{discover, Options};
use crate::proposals::store::agent_environment;
use crate::repository::{git, root};
use crate::util::{io_error, now_unix, sha256, validate_identifier};

/** Version of the recommendation records */
pub(crate) const REVIEW_FORMAT: u64 = 1;

/** Characters of the digest a reviewer must quote to approve */
const CONFIRM_LENGTH: usize = 12;

/** Return the proposals directory
 * Input
    - None
 * Output
    - Result<PathBuf, String>
*/
fn directory() -> Result<PathBuf, String> {
    Ok(root()?.join("zone-proposals"))
}

/** Write a JSON file atomically
 * Input
    - path: PathBuf - file
    - value: &Value - content
 * Output
    - Result<(), String>
*/
fn write(path: PathBuf, value: &Value) -> Result<(), String> {
    fs::create_dir_all(path.parent().ok_or("invalid path")?).map_err(io_error)?;
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_string_pretty(value).map_err(io_error)? + "\n",
    )
    .map_err(io_error)?;
    fs::rename(&temporary, &path).map_err(io_error)
}

/** Load one recommendation
 * Input
    - id: &str - recommendation id
 * Output
    - Result<Value, String>
*/
pub(crate) fn load(id: &str) -> Result<Value, String> {
    validate_identifier(id)?;
    let text = fs::read_to_string(directory()?.join(format!("{id}.json"))).map_err(|error| match error.kind() {
        ErrorKind::NotFound => format!("no zone recommendation '{id}'; run 'crane zones recommend' and list them with 'crane zones recommendations'"),
        _ => io_error(error),
    })?;
    serde_json::from_str(&text)
        .map_err(|error| format!("zone recommendation {id} is invalid: {error}"))
}

/** Load every recommendation, sorted by id
 * Input
    - None
 * Output
    - Result<Vec<Value>, String>
*/
pub(crate) fn all() -> Result<Vec<Value>, String> {
    let mut ids = match fs::read_dir(directory()?) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_suffix(".json"))
                    .map(String::from)
            })
            .filter(|id| id != "discovery")
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(io_error(error)),
    };
    ids.sort();
    ids.iter().map(|id| load(id)).collect()
}

/** Name who acts: the given name, else Git's user.email
 * Input
    - given: Option<String> - name
 * Output
    - String
*/
fn actor(given: Option<String>) -> String {
    given
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| git(&["config", "user.email"]).unwrap_or_else(|_| "unknown".into()))
}

/** Append a history entry to a record
 * Input
    - record: &mut Value - record
    - action: &str - action
    - by: &str - actor
    - detail: Value - details
 * Output
    - None
*/
fn log(record: &mut Value, action: &str, by: &str, detail: Value) {
    let entry = json!({"action": action, "by": by, "at": now_unix(), "revision": record["revision"], "digest": record["digest"], "detail": detail});
    if let Some(history) = record["history"].as_array_mut() {
        history.push(entry);
    }
}

/** Append an audit event to the hash-chained audit log
 * Input
    - event: Value - event fields
 * Output
    - Result<(), String>
*/
fn audit(mut event: Value) -> Result<(), String> {
    let path = directory()?.join("audit.jsonl");
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
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(io_error)?;
    writeln!(file, "{event}").map_err(io_error)
}

/** Read the audit log
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
                serde_json::from_str(line).map_err(|error| format!("zone audit log: {error}"))
            })
            .collect(),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(io_error(error)),
    }
}

/** The audit log with its chain verification
 * Input
    - None
 * Output
    - Result<Value, String>
*/
pub(crate) fn audit_log() -> Result<Value, String> {
    let events = audit_events()?;
    Ok(json!({"chain": verify_chain(&events), "events": events}))
}

/** Refuse a zone authority decision on behalf of an agent
 * Input
    - what: &str - decision
 * Output
    - Result<(), String>
*/
fn require_human(what: &str) -> Result<(), String> {
    match agent_environment() {
        Some(marker) => Err(format!("crane zones {what} refuses to run in an agent environment ({marker} is set); only a human decides zone authority")),
        None => Ok(()),
    }
}

/** Run discovery and record its recommendations: new ones become PROPOSED; changed ones get a new
 * revision (an approved zone keeps governing until the new revision is approved; a rejected one
 * is reopened only when it changed); ones whose evidence disappeared are WITHDRAWN while pending;
 * nothing is ever activated here
 * Input
    - by: Option<String> - who ran it
 * Output
    - Result<Value, String> the discovery result with what changed
*/
pub(crate) fn run(by: Option<String>) -> Result<Value, String> {
    let crane = root()?;
    let by = match agent_environment() {
        Some(marker) => format!("{} (agent environment: {marker})", actor(by)),
        None => actor(by),
    };
    let inventory = discover(&Options { full: false })?;
    let (zones, _) = load_zones(&crane)?;
    let records = all()?;
    let owned = records
        .iter()
        .filter(|record| record["active"] == true)
        .filter_map(|record| {
            Some((
                record["id"].as_str()?.to_string(),
                record["zone_id"].as_str()?.to_string(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let owned_names = owned.values().cloned().collect::<BTreeSet<_>>();
    let reserved = zones
        .iter()
        .map(|zone| zone.zone_id.clone())
        .filter(|name| !owned_names.contains(name))
        .collect::<BTreeSet<_>>();
    let state = crane.join("runtime").join("zones").join("resolution.json");
    let existing = super::resolve::resolve(&inventory, zones, &state)?;
    let scratch = crane
        .join("runtime")
        .join("zones")
        .join("recommendation-resolution.json");
    let (found, mut discovery) = recommend(&inventory, &existing, &reserved, &owned, &scratch)?;
    let run_id = format!(
        "discovery-{}",
        sha256(format!("{}:{:?}:{}", now_unix(), inventory.head, found.len()).as_bytes())
            .trim_start_matches("sha256:")
            .chars()
            .take(12)
            .collect::<String>()
    );
    let mut changes = Vec::new();
    let produced = found
        .iter()
        .map(|recommendation| recommendation.id.clone())
        .collect::<BTreeSet<_>>();
    for recommendation in found {
        let digest = sha256(recommendation.zone_text.as_bytes());
        let path = directory()?.join(format!("{}.json", recommendation.id));
        let previous = path
            .exists()
            .then(|| load(&recommendation.id))
            .transpose()?;
        let change = match previous {
            None => {
                if recommendation.value["covered_by_active_zones"] == true {
                    changes.push(json!({"id": recommendation.id, "change": "covered"}));
                    continue;
                }
                let mut record = json!({
                    "review_format": REVIEW_FORMAT,
                    "id": recommendation.id,
                    "zone_id": recommendation.zone_id,
                    "status": "proposed",
                    "active": false,
                    "revision": 1,
                    "digest": digest,
                    "zone_text": recommendation.zone_text,
                    "recommendation": recommendation.value,
                    "discovered_by": {"run": run_id, "recommender": RECOMMENDER, "head": inventory.head},
                    "history": [],
                    "activation": null,
                    "rejection": null,
                });
                log(&mut record, "proposed", &by, json!({"run": run_id}));
                write(path, &record)?;
                "proposed"
            }
            Some(mut record) => {
                let unchanged = record["digest"] == digest.as_str();
                let status = record["status"].as_str().unwrap_or_default().to_string();
                if unchanged && status != "withdrawn" {
                    // Refresh the evidence; the proposal itself did not change
                    record["recommendation"] = recommendation.value;
                    write(path, &record)?;
                    changes.push(
                        json!({"id": recommendation.id, "change": "unchanged", "status": status}),
                    );
                    continue;
                }
                if unchanged && recommendation.value["covered_by_active_zones"] == true {
                    continue;
                }
                record["revision"] = json!(record["revision"].as_u64().unwrap_or(1) + 1);
                record["digest"] = json!(digest);
                record["zone_id"] = json!(recommendation.zone_id);
                record["zone_text"] = json!(recommendation.zone_text);
                record["recommendation"] = recommendation.value;
                record["discovered_by"] =
                    json!({"run": run_id, "recommender": RECOMMENDER, "head": inventory.head});
                record["status"] = json!("proposed");
                record["rejection"] = Value::Null;
                let action = match status.as_str() {
                    "approved" => "revision_proposed",
                    "rejected" => "reopened",
                    _ => "regenerated",
                };
                log(&mut record, action, &by, json!({"run": run_id}));
                write(path, &record)?;
                action
            }
        };
        changes.push(json!({"id": recommendation.id, "change": change}));
    }
    for mut record in all()? {
        let id = record["id"].as_str().unwrap_or_default().to_string();
        if !produced.contains(&id)
            && matches!(record["status"].as_str(), Some("proposed" | "in_review"))
        {
            record["status"] = json!("withdrawn");
            log(
                &mut record,
                "withdrawn",
                &by,
                json!({"run": run_id, "reason": "discovery no longer finds the evidence"}),
            );
            write(directory()?.join(format!("{id}.json")), &record)?;
            changes.push(json!({"id": id, "change": "withdrawn"}));
        }
    }
    discovery["run"] = json!(run_id);
    discovery["at"] = json!(now_unix());
    discovery["by"] = json!(by);
    discovery["changes"] = json!(changes);
    let mut stored = fs::read_to_string(directory()?.join("discovery.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(json!({"runs": []}));
    if let Some(runs) = stored["runs"].as_array_mut() {
        runs.push(json!({"run": run_id, "at": discovery["at"], "head": discovery["head"], "recommendations": discovery["recommendations"].as_array().map_or(0, Vec::len)}));
    }
    stored["latest"] = discovery.clone();
    write(directory()?.join("discovery.json"), &stored)?;
    crate::repo::note_discovery(&inventory)?;
    Ok(discovery)
}

/** Mark a recommendation as under review by someone (PROPOSED -> IN_REVIEW)
 * Input
    - id: &str - recommendation id
    - by: Option<String> - reviewer
 * Output
    - Result<Value, String> the record
*/
pub(crate) fn claim(id: &str, by: Option<String>) -> Result<Value, String> {
    require_human("review")?;
    let mut record = load(id)?;
    if record["status"] != "proposed" {
        return Ok(record);
    }
    let by = actor(by);
    record["status"] = json!("in_review");
    record["reviewer"] = json!(by);
    log(&mut record, "review_started", &by, json!({}));
    write(directory()?.join(format!("{id}.json")), &record)?;
    Ok(record)
}

/** Approve a recommendation: human-only, quoting the reviewed digest; writes the zone file, which
 * activates it (sessions started afterwards are governed by it); approving the same revision
 * again changes nothing; a zone file written by hand is never overwritten
 * Input
    - id: &str - recommendation id
    - approver: Option<String> - who approves (required)
    - confirm: Option<String> - at least the first 12 characters of the digest
 * Output
    - Result<Value, String> the record, with already_approved
*/
pub(crate) fn approve(
    id: &str,
    approver: Option<String>,
    confirm: Option<String>,
) -> Result<Value, String> {
    require_human("approve")?;
    let approver = approver
        .filter(|name| !name.trim().is_empty())
        .ok_or("crane zones approve requires --approver NAME")?;
    let mut record = load(id)?;
    let digest = record["digest"].as_str().unwrap_or_default().to_string();
    if record["status"] == "approved" && record["activation"]["digest"] == digest.as_str() {
        let mut value = record.clone();
        value["already_approved"] = json!(true);
        return Ok(value);
    }
    if !matches!(record["status"].as_str(), Some("proposed" | "in_review")) {
        return Err(format!(
            "zone recommendation {id} is {}; only proposed recommendations can be approved",
            record["status"].as_str().unwrap_or("invalid")
        ));
    }
    let quoted = confirm.unwrap_or_default();
    let quoted = quoted.trim_start_matches("sha256:");
    if quoted.len() < CONFIRM_LENGTH || !digest.trim_start_matches("sha256:").starts_with(quoted) {
        return Err(format!("crane zones approve requires --confirm with at least the first {CONFIRM_LENGTH} characters of the reviewed digest (see 'crane zones review {id}')"));
    }
    let crane = root()?;
    let zone_id = record["zone_id"].as_str().unwrap_or_default().to_string();
    let text = record["zone_text"].as_str().unwrap_or_default().to_string();
    let parsed = parse(&text, &format!("zones/{zone_id}.zone"))?;
    if parsed.len() != 1 || parsed[0].zone_id != zone_id {
        return Err(format!(
            "zone recommendation {id} does not define exactly zone {zone_id}"
        ));
    }
    let file = crane.join("zones").join(format!("{zone_id}.zone"));
    let ours = record["activation"]["file"] == format!("zones/{zone_id}.zone").as_str();
    if file.exists() && !ours {
        return Err(format!("zones/{zone_id}.zone already exists and was not written by a zone review; it is never overwritten"));
    }
    let (before, _) = load_zones(&crane)?;
    if let Some(other) = before
        .iter()
        .find(|zone| zone.zone_id == zone_id && zone.source != format!("zones/{zone_id}.zone"))
    {
        return Err(format!(
            "zone {zone_id} is already defined in {}",
            other.source
        ));
    }
    let version_before = set_version(&before);
    let previous = fs::read_to_string(&file).ok();
    let temporary = file.with_extension(format!("zone.tmp{}", std::process::id()));
    fs::create_dir_all(crane.join("zones")).map_err(io_error)?;
    fs::write(&temporary, &text).map_err(io_error)?;
    fs::rename(&temporary, &file).map_err(io_error)?;
    let (_, problems) = load_zones(&crane)?;
    if let Some(problem) = problems
        .iter()
        .find(|problem| problem.contains(&format!("zones/{zone_id}.zone")))
    {
        // Restore the previous state: an invalid zone never stays active
        match previous {
            Some(previous) => fs::write(&file, previous).map_err(io_error)?,
            None => fs::remove_file(&file).map_err(io_error)?,
        }
        return Err(format!("the zone could not be activated: {problem}"));
    }
    // A revision that moved to another zone name retires the file its earlier approval wrote
    if let Some(old) = record["activation"]["file"]
        .as_str()
        .filter(|old| *old != format!("zones/{zone_id}.zone"))
    {
        let _ = fs::remove_file(crane.join(old));
    }
    let version_after = set_version(&load_zones(&crane)?.0);
    record["status"] = json!("approved");
    record["active"] = json!(true);
    record["activation"] = json!({"file": format!("zones/{zone_id}.zone"), "digest": digest, "revision": record["revision"], "approver": approver, "at": now_unix(), "zone_set_version_before": version_before, "zone_set_version_after": version_after});
    log(
        &mut record,
        "approved",
        &approver,
        json!({"file": format!("zones/{zone_id}.zone")}),
    );
    write(directory()?.join(format!("{id}.json")), &record)?;
    audit(
        json!({"event": "zone_recommendation_approved", "recommendation": id, "zone_id": zone_id, "revision": record["revision"], "digest": digest, "by": approver, "zone_set_version_before": version_before, "zone_set_version_after": version_after}),
    )?;
    record["already_approved"] = json!(false);
    Ok(record)
}

/** Reject a recommendation: human-only; nothing is activated; rejecting it again changes nothing;
 * an active zone is not rejected here (its file is the governance state)
 * Input
    - id: &str - recommendation id
    - approver: Option<String> - who rejects (required)
    - reason: Option<String> - why
 * Output
    - Result<Value, String> the record, with already_rejected
*/
pub(crate) fn reject(
    id: &str,
    approver: Option<String>,
    reason: Option<String>,
) -> Result<Value, String> {
    require_human("reject")?;
    let approver = approver
        .filter(|name| !name.trim().is_empty())
        .ok_or("crane zones reject requires --approver NAME")?;
    let mut record = load(id)?;
    if record["status"] == "rejected" {
        let mut value = record.clone();
        value["already_rejected"] = json!(true);
        return Ok(value);
    }
    if !matches!(record["status"].as_str(), Some("proposed" | "in_review")) {
        return Err(format!(
            "zone recommendation {id} is {}; only proposed recommendations can be rejected",
            record["status"].as_str().unwrap_or("invalid")
        ));
    }
    record["status"] = json!("rejected");
    record["rejection"] = json!({"by": approver, "reason": reason, "at": now_unix(), "revision": record["revision"], "digest": record["digest"]});
    log(
        &mut record,
        "rejected",
        &approver,
        json!({"reason": reason}),
    );
    write(directory()?.join(format!("{id}.json")), &record)?;
    audit(
        json!({"event": "zone_recommendation_rejected", "recommendation": id, "zone_id": record["zone_id"], "revision": record["revision"], "digest": record["digest"], "by": approver, "reason": reason}),
    )?;
    record["already_rejected"] = json!(false);
    Ok(record)
}

/** Summarize recommendations for listing
 * Input
    - None
 * Output
    - Result<Value, String>
*/
pub(crate) fn summary() -> Result<Value, String> {
    let records = all()?;
    let discovery = fs::read_to_string(directory()?.join("discovery.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    Ok(json!({
        "discovery": discovery.as_ref().map(|value| value["latest"].clone()),
        "runs": discovery.as_ref().and_then(|value| value["runs"].as_array().map(Vec::len)).unwrap_or(0),
        "recommendations": records.iter().map(|record| json!({
            "id": record["id"],
            "zone_id": record["zone_id"],
            "status": record["status"],
            "active": record["active"],
            "revision": record["revision"],
            "digest": record["digest"],
            "criticality": record["recommendation"]["criticality"],
            "autonomy": record["recommendation"]["autonomy"],
            "confidence": record["recommendation"]["confidence"],
            "sources": record["recommendation"]["sources"],
            "files": record["recommendation"]["affected"]["files"].as_array().map_or(0, Vec::len),
        })).collect::<Vec<_>>(),
    }))
}
