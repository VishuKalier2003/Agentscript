// Task completion: the tracker issue of a task is completed only after the delivery pipeline has
// confirmed that its governed change merged. The merge creates one completion event per
// (repository, task, merge commit), queued as Jira requests in the delivery outbox; a dispatcher
// sends them through the configured Jira transport with idempotent steps (status check, a comment
// carrying the event id that is never posted twice, the transition, a final status check), keeps
// every step's result on disk so a restart resumes where it stopped, and retries with backoff when
// Jira is unavailable. Nothing here reruns an engineering session or marks a task complete before
// Jira confirms it (or was already completed externally).

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::delivery::policy::DeliveryConfig;
use crate::proposals::store::agent_environment;
use crate::repository::root;
use crate::util::{io_error, now_unix, sha256};

/** Version of the completion event layout */
const COMPLETION_FORMAT: u64 = 1;

/** Seconds before the first retry; each failed attempt doubles it */
const BACKOFF: u64 = 60;

/** Longest wait between two attempts, in seconds */
const MAX_BACKOFF: u64 = 3600;

/** Age after which a dispatcher lock is considered abandoned (its process died), in seconds */
const STALE_LOCK: u64 = 300;

/** Refuse a completion action on behalf of an agent
 * Input
    - action: &str - send or reconcile
 * Output
    - Result<(), String>
*/
fn require_human(action: &str) -> Result<(), String> {
    match agent_environment() {
        Some(marker) => Err(format!(
            "crane task completions {action} refuses to run in an agent environment ({marker} is set); tracker completion is the organization's"
        )),
        None => Ok(()),
    }
}

/** Return the completion events directory, .crane/runtime/completions
 * Input
    - None
 * Output
    - Result<PathBuf, String>
*/
fn directory() -> Result<PathBuf, String> {
    Ok(root()?.join("runtime").join("completions"))
}

/** Compute the id of the completion event of a merged task: the same repository, task, and merge
 * always give the same id, which is what makes completion idempotent
 * Input
    - repository: &str - repository identity
    - task: &str - task id
    - merge_sha: &str - merge commit
 * Output
    - String "completion-" and 20 hex digits
*/
pub(crate) fn event_id(repository: &str, task: &str, merge_sha: &str) -> String {
    let digest =
        sha256(format!("crane-completion\n{repository}\n{task}\n{merge_sha}\n").as_bytes());
    format!(
        "completion-{}",
        digest
            .trim_start_matches("sha256:")
            .chars()
            .take(20)
            .collect::<String>()
    )
}

/** Load a completion event
 * Input
    - id: &str - event id
 * Output
    - Result<Option<Value>, String>
*/
pub(crate) fn load(id: &str) -> Result<Option<Value>, String> {
    if !id.starts_with("completion-")
        || !id[11..]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(format!("invalid completion event id '{id}'"));
    }
    match fs::read_to_string(directory()?.join(format!("{id}.json"))) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| format!("completion event {id} is invalid: {error}")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}

/** Write a completion event through a temporary file
 * Input
    - event: &Value - event
 * Output
    - Result<(), String>
*/
fn save(event: &Value) -> Result<(), String> {
    let folder = directory()?;
    fs::create_dir_all(&folder).map_err(io_error)?;
    let path = folder.join(format!(
        "{}.json",
        event["event_id"].as_str().unwrap_or_default()
    ));
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_string_pretty(event).map_err(io_error)? + "\n",
    )
    .map_err(io_error)?;
    fs::rename(&temporary, &path).map_err(io_error)
}

/** List every completion event, oldest first
 * Input
    - None
 * Output
    - Result<Vec<Value>, String>
*/
pub(crate) fn all() -> Result<Vec<Value>, String> {
    let mut events = match fs::read_dir(directory()?) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".json").map(String::from)
            })
            .filter_map(|id| load(&id).ok().flatten())
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(io_error(error)),
    };
    events.sort_by_key(|event| (event["created_at"].as_u64(), event["event_id"].to_string()));
    Ok(events)
}

/** Return the latest completion event of a task
 * Input
    - task: &str - task id
 * Output
    - Result<Option<Value>, String>
*/
pub(crate) fn for_task(task: &str) -> Result<Option<Value>, String> {
    Ok(all()?
        .into_iter()
        .rev()
        .find(|event| event["task_id"] == task))
}

/** Append a history entry to an event
 * Input
    - event: &mut Value - event
    - action: &str - what happened
    - detail: Value - details
 * Output
    - None
*/
fn log(event: &mut Value, action: &str, detail: Value) {
    let entry =
        json!({"action": action, "at": now_unix(), "status": event["status"], "detail": detail});
    if let Some(history) = event["history"].as_array_mut() {
        history.push(entry);
    }
}

/** Find the tracker issue of a task: the orchestrated task's source, else the task source that
 * offers it (task intake)
 * Input
    - task: &str - task id
 * Output
    - Result<Option<Value>, String> {source, external_id}
*/
fn tracker_of(task: &str) -> Result<Option<Value>, String> {
    if let Some(record) = crate::orchestration::TaskRecord::load(task)? {
        return Ok(Some(
            json!({"source": record.value["source"], "external_id": record.value["external_id"]}),
        ));
    }
    Ok(crate::intake::source_of(task))
}

/** Queue the completion of a merged task (called by the delivery pipeline after it verified the
 * merge): one completion event per repository, task, and merge commit, with its Jira requests in
 * the delivery outbox; queuing the same merge again returns the existing event unchanged
 * Input
    - delivery: &str - delivery id (the session id)
    - task: &str - task id
    - merge_sha: &str - verified merge commit
    - state: &Value - the delivery state (pull request, attestation, binding)
    - config: &DeliveryConfig - delivery configuration (tracker settings)
 * Output
    - Result<Value, String> the event, with duplicate telling whether it already existed
*/
pub(crate) fn enqueue(
    delivery: &str,
    task: &str,
    merge_sha: &str,
    state: &Value,
    config: &DeliveryConfig,
) -> Result<Value, String> {
    let (_, identity) = crate::session::repository_identity()?;
    let id = event_id(&identity, task, merge_sha);
    if let Some(mut existing) = load(&id)? {
        existing["duplicate"] = json!(true);
        return Ok(existing);
    }
    let tracker = tracker_of(task)?;
    let (owner, name) = crate::repo::owner_and_name();
    let source = tracker
        .as_ref()
        .and_then(|tracker| tracker["source"].as_str().map(String::from));
    let external = tracker
        .as_ref()
        .and_then(|tracker| tracker["external_id"].as_str().map(String::from))
        .unwrap_or_else(|| task.to_string());
    let text = format!(
        "Merged by Crane as {merge_sha} ({}). Contract {} held; attestation {}. [crane-completion:{id}]",
        state["pull_request"]["url"].as_str().unwrap_or_default(),
        state["binding"]["contract_digest"].as_str().or(state["contract_version"].as_str()).unwrap_or_default(),
        state["attestation_digest"].as_str().unwrap_or_default()
    );
    let requests = match source.as_deref() {
        Some("jira") => vec![
            (
                "comment",
                json!({"method": "POST", "path": format!("/rest/api/3/issue/{external}/comment"), "body": {"body": {"type": "doc", "version": 1, "content": [{"type": "paragraph", "content": [{"type": "text", "text": text}]}]}}}),
            ),
            (
                "transition",
                json!({"method": "POST", "path": format!("/rest/api/3/issue/{external}/transitions"), "body": {"transition": {"id": config.trackers["jira"]["done_transition"].as_str().unwrap_or("done")}}}),
            ),
        ],
        Some("asana") => vec![
            (
                "comment",
                json!({"method": "POST", "path": format!("/tasks/{external}/stories"), "body": {"data": {"text": text}}}),
            ),
            (
                "transition",
                json!({"method": "PUT", "path": format!("/tasks/{external}"), "body": {"data": {"completed": true}}}),
            ),
        ],
        _ => Vec::new(),
    };
    let mut outbox = Vec::new();
    for (step, request) in &requests {
        let mut entry = request.clone();
        entry["completion_event"] = json!(id);
        entry["step"] = json!(step);
        outbox.push(crate::delivery::outbox(
            source.as_deref().unwrap_or("tracker"),
            delivery,
            entry,
        )?);
    }
    let status = match source.as_deref() {
        Some("jira") => "pending",
        Some(_) => "queued",
        None => "completed",
    };
    let mut event = json!({
        "completion_format": COMPLETION_FORMAT,
        "event_id": id,
        "task_id": task,
        "tracker": source,
        "external_id": external,
        "repository": {"identity": identity, "owner": owner, "name": name},
        "merge_sha": merge_sha,
        "delivery": delivery,
        "pull_request": state["pull_request"]["url"],
        "attestation": state["attestation_digest"],
        "contract_digest": state["binding"]["contract_digest"],
        "status": status,
        "attempts": 0,
        "next_attempt_at": now_unix(),
        "last_error": null,
        "needs_attention": false,
        "steps": {"checked": null, "comment": null, "transition": null, "verified": null},
        "outbox": outbox,
        "created_at": now_unix(),
        "completed_at": null,
        "history": [],
    });
    log(
        &mut event,
        "queued",
        json!({"reason": match status {
            "pending" => "the governed change merged; the Jira issue is completed by the dispatcher",
            "queued" => "the governed change merged; the requests wait for the tracker's forwarder",
            _ => "the governed change merged; the task has no tracker issue",
        }}),
    );
    if status == "completed" {
        event["completed_at"] = json!(now_unix());
    }
    save(&event)?;
    if status == "completed" {
        crate::orchestration::completion_confirmed(task, &id)?;
    }
    event["duplicate"] = json!(false);
    Ok(event)
}

/** A failed Jira call
 * Variants
    - Unavailable(String) - transport failure, timeout, 429, or 5xx: retry later
    - Rejected(String) - 4xx: retry later too, but a human should look
*/
enum Failure {
    Unavailable(String),
    Rejected(String),
}

/** Call Jira through the configured transport: the program in trackers.jira.transport receives
 * {method, url, path, body} on stdin and answers {status, body} on stdout (Crane itself makes no
 * network calls)
 * Input
    - config: &DeliveryConfig - configuration
    - method: &str - HTTP method
    - path: &str - API path
    - body: Value - request body (Null for none)
 * Output
    - Result<Value, Failure> the response body
*/
fn call(config: &DeliveryConfig, method: &str, path: &str, body: Value) -> Result<Value, Failure> {
    let jira = &config.trackers["jira"];
    let transport = jira["transport"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .filter(|parts| !parts.is_empty())
        .ok_or_else(|| {
            Failure::Unavailable(
                "no Jira transport is configured (trackers.jira.transport in .crane/delivery.json)"
                    .into(),
            )
        })?;
    let base = jira["base_url"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('/');
    let request =
        json!({"method": method, "url": format!("{base}{path}"), "path": path, "body": body});
    let mut child = Command::new(&transport[0])
        .args(&transport[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            Failure::Unavailable(format!("the Jira transport could not start: {error}"))
        })?;
    if let Some(mut input) = child.stdin.take() {
        let _ = input.write_all(request.to_string().as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|error| Failure::Unavailable(format!("the Jira transport failed: {error}")))?;
    if !output.status.success() {
        return Err(Failure::Unavailable(format!(
            "the Jira transport failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let response: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        Failure::Unavailable(format!("the Jira transport answered no JSON: {error}"))
    })?;
    let status = response["status"].as_u64().unwrap_or(0);
    match status {
        200..=299 => Ok(response["body"].clone()),
        429 | 500..=599 | 0 => Err(Failure::Unavailable(format!(
            "Jira answered {status} to {method} {path}"
        ))),
        _ => Err(Failure::Rejected(format!(
            "Jira answered {status} to {method} {path}: {}",
            response["body"]
        ))),
    }
}

/** Check whether a Jira issue is in a done status
 * Input
    - config: &DeliveryConfig - configuration
    - key: &str - issue key
 * Output
    - Result<(bool, String), Failure> done and the status name
*/
fn issue_done(config: &DeliveryConfig, key: &str) -> Result<(bool, String), Failure> {
    let issue = call(
        config,
        "GET",
        &format!("/rest/api/3/issue/{key}?fields=status"),
        Value::Null,
    )?;
    let status = &issue["fields"]["status"];
    Ok((
        status["statusCategory"]["key"] == "done",
        status["name"].as_str().unwrap_or("unknown").to_string(),
    ))
}

/** Read the request a completion step queued in the outbox
 * Input
    - event: &Value - event
    - step: &str - comment or transition
 * Output
    - Result<Value, String>
*/
fn queued(event: &Value, step: &str) -> Result<Value, String> {
    let folder = root()?.join("runtime").join("delivery").join("outbox");
    for name in event["outbox"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let entry: Value =
            serde_json::from_str(&fs::read_to_string(folder.join(name)).map_err(io_error)?)
                .map_err(|error| format!("outbox {name}: {error}"))?;
        if entry["request"]["step"] == step {
            return Ok(entry["request"].clone());
        }
    }
    Err(format!(
        "the outbox has no {step} request for {}",
        event["event_id"].as_str().unwrap_or_default()
    ))
}

/** Run the steps of one completion event, each recorded as soon as it is done so a restart
 * resumes after it: if the issue is already done before Crane transitioned it, it was completed
 * externally; the comment is posted only when no comment carries the event id; the transition is
 * sent; the issue must then be done
 * Input
    - config: &DeliveryConfig - configuration
    - event: &mut Value - event (saved after every step)
 * Output
    - Result<Result<&'static str, Failure>, String> the final status, or the failure of this
      attempt; the outer error is for local problems (files)
*/
fn attempt(
    config: &DeliveryConfig,
    event: &mut Value,
) -> Result<Result<&'static str, Failure>, String> {
    let key = event["external_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let marker = format!(
        "[crane-completion:{}]",
        event["event_id"].as_str().unwrap_or_default()
    );
    macro_rules! step {
        ($call:expr) => {
            match $call {
                Ok(value) => value,
                Err(failure) => return Ok(Err(failure)),
            }
        };
    }
    let (done, status) = step!(issue_done(config, &key));
    event["steps"]["checked"] = json!({"at": now_unix(), "status": status, "done": done});
    save(event)?;
    if done && event["steps"]["transition"].is_null() {
        return Ok(Ok("completed_externally"));
    }
    if event["steps"]["comment"].is_null() {
        let comments = step!(call(
            config,
            "GET",
            &format!("/rest/api/3/issue/{key}/comment"),
            Value::Null
        ));
        let posted = comments["comments"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|comment| comment.to_string().contains(&marker));
        let result = match posted {
            Some(comment) => json!({"at": now_unix(), "id": comment["id"], "found": true}),
            None => {
                let request = queued(event, "comment")?;
                let answer = step!(call(
                    config,
                    "POST",
                    request["path"].as_str().unwrap_or_default(),
                    request["body"].clone()
                ));
                json!({"at": now_unix(), "id": answer["id"], "found": false})
            }
        };
        event["steps"]["comment"] = result;
        save(event)?;
    }
    if event["steps"]["transition"].is_null() && !done {
        let request = queued(event, "transition")?;
        step!(call(
            config,
            "POST",
            request["path"].as_str().unwrap_or_default(),
            request["body"].clone()
        ));
        event["steps"]["transition"] =
            json!({"at": now_unix(), "transition": request["body"]["transition"]["id"]});
        save(event)?;
    }
    let (done, status) = step!(issue_done(config, &key));
    event["steps"]["verified"] = json!({"at": now_unix(), "status": status, "done": done});
    save(event)?;
    if done {
        Ok(Ok("completed"))
    } else {
        Ok(Err(Failure::Rejected(format!(
            "the transition left {key} in status {status}, not done"
        ))))
    }
}

/** Exclusive access for one dispatcher, released on drop
 * Fields
    - path: PathBuf - lock file
*/
struct Lock {
    path: PathBuf,
}

impl Lock {
    /** Take the lock, replacing one left by a process that died
     * Input
        - None
     * Output
        - Result<Lock, String>
    */
    fn acquire() -> Result<Self, String> {
        let folder = directory()?;
        fs::create_dir_all(&folder).map_err(io_error)?;
        let path = folder.join("dispatch.lock");
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let _ = write!(file, "{} {}", std::process::id(), now_unix());
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let since = fs::read_to_string(&path)
                        .ok()
                        .and_then(|text| {
                            text.split_whitespace()
                                .nth(1)
                                .and_then(|at| at.parse::<u64>().ok())
                        })
                        .unwrap_or(0);
                    if now_unix().saturating_sub(since) < STALE_LOCK {
                        return Err("another completion dispatcher is running".into());
                    }
                    let _ = fs::remove_file(&path);
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        Err("the completion dispatcher lock could not be taken".into())
    }
}

impl Drop for Lock {
    /** Release the lock
     * Input
        - None (uses self)
     * Output
        - None
    */
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/** Mark an event confirmed: completed by Crane or found completed externally; the task's
 * lifecycle completes and the delivery journal records it
 * Input
    - event: &mut Value - event
    - status: &str - completed or completed_externally
 * Output
    - Result<(), String>
*/
fn confirm(event: &mut Value, status: &str) -> Result<(), String> {
    event["status"] = json!(status);
    event["completed_at"] = json!(now_unix());
    event["last_error"] = Value::Null;
    event["needs_attention"] = json!(false);
    log(event, status, json!({}));
    save(event)?;
    let task = event["task_id"].as_str().unwrap_or_default().to_string();
    let id = event["event_id"].as_str().unwrap_or_default().to_string();
    crate::orchestration::completion_confirmed(&task, &id)?;
    crate::delivery::note(
        event["delivery"].as_str().unwrap_or_default(),
        json!({"kind": "task_completed", "task": task, "tracker": event["tracker"], "external_id": event["external_id"], "completion_event": id, "status": status, "merge_sha": event["merge_sha"]}),
    )
}

/** Send the due completion events: each pending or retry-pending Jira event whose next attempt is
 * due (all of them with force) runs its remaining steps; a failure keeps the event, counts the
 * attempt, and schedules the next with exponential backoff (COMPLETION_RETRY_PENDING); a confirmed
 * event completes the task exactly once; refused on behalf of an agent
 * Input
    - force: bool - ignore the backoff
    - task: Option<&str> - only this task's events
 * Output
    - Result<Vec<Value>, String> one summary per event handled
*/
pub(crate) fn dispatch(force: bool, task: Option<&str>) -> Result<Vec<Value>, String> {
    require_human("send")?;
    let config = crate::delivery::policy::load()?;
    let _lock = Lock::acquire()?;
    let now = now_unix();
    let mut results = Vec::new();
    for mut event in all()? {
        if task.is_some_and(|task| event["task_id"] != task)
            || event["tracker"] != "jira"
            || !matches!(
                event["status"].as_str(),
                Some("pending" | "retry_pending" | "sending")
            )
            || (!force && event["next_attempt_at"].as_u64().unwrap_or(0) > now)
        {
            continue;
        }
        event["status"] = json!("sending");
        event["attempts"] = json!(event["attempts"].as_u64().unwrap_or(0) + 1);
        save(&event)?;
        let outcome = attempt(&config, &mut event)?;
        match outcome {
            Ok(status) => confirm(&mut event, status)?,
            Err(failure) => {
                let (error, attention) = match failure {
                    Failure::Unavailable(error) => (error, false),
                    Failure::Rejected(error) => (error, true),
                };
                let attempts = event["attempts"].as_u64().unwrap_or(1);
                let wait = (BACKOFF << (attempts.saturating_sub(1)).min(10)).min(MAX_BACKOFF);
                event["status"] = json!("retry_pending");
                event["last_error"] = json!(error);
                event["needs_attention"] = json!(attention);
                event["next_attempt_at"] = json!(now_unix() + wait);
                log(
                    &mut event,
                    "retry_scheduled",
                    json!({"error": error, "in_seconds": wait}),
                );
                save(&event)?;
            }
        }
        results.push(summary(&event));
    }
    Ok(results)
}

/** Recover after a restart: every merged delivery of a task gets its completion event (queued
 * again if the process stopped between the merge and the queue), an event left "sending" by a
 * process that died becomes retry-pending (its recorded steps are not repeated), and a confirmed
 * event whose task did not complete completes it; then the due events are sent unless asked not to
 * Input
    - send: bool - dispatch afterwards
 * Output
    - Result<Value, String> {queued, recovered, confirmed, sent}
*/
pub(crate) fn reconcile(send: bool) -> Result<Value, String> {
    require_human("reconcile")?;
    let config = crate::delivery::policy::load()?;
    let mut queued_again = Vec::new();
    let deliveries = root()?.join("runtime").join("delivery");
    if let Ok(entries) = fs::read_dir(&deliveries) {
        for entry in entries.filter_map(|entry| entry.ok()) {
            let id = entry.file_name().to_string_lossy().into_owned();
            if id == "outbox" || !entry.path().join("journal.jsonl").is_file() {
                continue;
            }
            let state = crate::delivery::state(&crate::delivery::journal(&id)?);
            let (Some(task), Some(sha)) = (state["task"].as_str(), state["merged"]["sha"].as_str())
            else {
                continue;
            };
            let event = enqueue(&id, task, sha, &state, &config)?;
            if event["duplicate"] == false {
                queued_again.push(event["event_id"].clone());
            }
        }
    }
    let mut recovered = Vec::new();
    let mut confirmed = Vec::new();
    {
        let _lock = Lock::acquire()?;
        for mut event in all()? {
            match event["status"].as_str() {
                Some("sending") => {
                    event["status"] = json!("retry_pending");
                    event["next_attempt_at"] = json!(now_unix());
                    log(
                        &mut event,
                        "recovered",
                        json!({"reason": "the process sending it stopped; its recorded steps are kept"}),
                    );
                    save(&event)?;
                    recovered.push(event["event_id"].clone());
                }
                Some("completed" | "completed_externally") => {
                    let task = event["task_id"].as_str().unwrap_or_default();
                    if crate::orchestration::completion_confirmed(
                        task,
                        event["event_id"].as_str().unwrap_or_default(),
                    )? {
                        confirmed.push(event["event_id"].clone());
                    }
                }
                _ => {}
            }
        }
    }
    let sent = if send {
        dispatch(false, None)?
    } else {
        Vec::new()
    };
    Ok(
        json!({"queued": queued_again, "recovered": recovered, "confirmed": confirmed, "sent": sent}),
    )
}

/** Record that a task's tracker issue was completed outside Crane after its merge (a tracker
 * webhook reported it): a pending event is confirmed as completed externally and never sent
 * Input
    - task: &str - task id
 * Output
    - Result<(), String>
*/
pub(crate) fn completed_externally(task: &str) -> Result<(), String> {
    if let Some(mut event) = for_task(task)? {
        if matches!(
            event["status"].as_str(),
            Some("pending" | "retry_pending" | "queued")
        ) {
            event["status"] = json!("completed_externally");
            event["completed_at"] = json!(now_unix());
            log(
                &mut event,
                "completed_externally",
                json!({"via": "tracker event"}),
            );
            save(&event)?;
        }
    }
    Ok(())
}

/** Summarize an event for listings
 * Input
    - event: &Value - event
 * Output
    - Value
*/
pub(crate) fn summary(event: &Value) -> Value {
    json!({
        "event_id": event["event_id"],
        "task_id": event["task_id"],
        "tracker": event["tracker"],
        "external_id": event["external_id"],
        "merge_sha": event["merge_sha"],
        "status": event["status"],
        "state": lifecycle_state(event),
        "attempts": event["attempts"],
        "next_attempt_at": event["next_attempt_at"],
        "last_error": event["last_error"],
        "needs_attention": event["needs_attention"],
    })
}

/** Name the task lifecycle state an event stands for
 * Input
    - event: &Value - event
 * Output
    - &'static str TASK_COMPLETION_PENDING, COMPLETION_RETRY_PENDING, or COMPLETED
*/
pub(crate) fn lifecycle_state(event: &Value) -> &'static str {
    match event["status"].as_str() {
        Some("completed" | "completed_externally") => "COMPLETED",
        Some("retry_pending") => "COMPLETION_RETRY_PENDING",
        _ => "TASK_COMPLETION_PENDING",
    }
}
