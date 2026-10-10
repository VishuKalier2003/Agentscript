// The MongoDB sync pipeline. Each run first reconciles with the database (reading how far each
// stream of this repository already reached MongoDB, and moving the local checkpoint back when the
// database holds less, for example after a restore), then pushes everything newer: events, ledger
// entries, governance changes, and alerts (unordered inserts, duplicates skipped), upserts the
// sessions, tasks, agents, repository, selection records, and signed policy version, and refreshes
// metric rollups for the repository and for every session and tool call that has new events.
// The outcome is recorded in the trust directory (mongodb-state.json), which drives the periodic
// sync and the dashboard's health view.

use std::collections::BTreeSet;
use std::fs;

use mongodb::bson::{self, doc, Bson, Document};
use mongodb::options::InsertManyOptions;
use mongodb::sync::Database;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{database, migrate, Settings};
use crate::governance::state::State;
use crate::governance::workspace::Workspace;
use crate::platform::files::{read_lines, write_atomic, Lock};
use crate::platform::{io_error, now_millis};
use crate::telemetry::metrics;
use crate::telemetry::model::Severity;
use crate::telemetry::Stores;

/** Largest batch inserted at once */
const BATCH: usize = 500;

/** How far each local stream has been copied
 * Fields
    - events: u64 - last event sequence copied
    - ledger: u64 - last ledger sequence copied
    - audit: u64 - last governance audit sequence copied
    - alerts: u64 - alert records copied
*/
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
struct Checkpoint {
    events: u64,
    ledger: u64,
    audit: u64,
    alerts: u64,
}

/** The result of one sync
 * Fields
    - events: usize - events copied
    - ledger: usize - ledger entries copied
    - audit: usize - governance changes copied
    - alerts: usize - alerts copied
    - rollups: usize - metric rollups refreshed
    - duplicates: usize - records already present (retries)
    - reset: bool - the database held less than the checkpoint, so missing records were re-sent
*/
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub(crate) struct Flushed {
    pub(crate) events: usize,
    pub(crate) ledger: usize,
    pub(crate) audit: usize,
    pub(crate) alerts: usize,
    pub(crate) rollups: usize,
    pub(crate) duplicates: usize,
    pub(crate) reset: bool,
}

/** The sync status kept in the trust directory
 * Fields
    - last_attempt_at: u64 - Unix milliseconds of the last attempt
    - last_success_at: Option<u64> - Unix milliseconds of the last successful sync
    - last_error: Option<String> - why the last attempt failed (redacted)
    - remote_events: u64 - event sequence MongoDB holds up to
    - remote_ledger: u64 - ledger sequence MongoDB holds up to
    - remote_audit: u64 - governance sequence MongoDB holds up to
    - remote_alerts: u64 - alerts MongoDB holds
    - database: Option<String> - database name
*/
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SyncState {
    pub(crate) last_attempt_at: u64,
    pub(crate) last_success_at: Option<u64>,
    pub(crate) last_error: Option<String>,
    pub(crate) remote_events: u64,
    pub(crate) remote_ledger: u64,
    pub(crate) remote_audit: u64,
    pub(crate) remote_alerts: u64,
    pub(crate) database: Option<String>,
}

/** Read the sync status
 * Input
    - stores: &Stores - runtime stores
 * Output
    - SyncState, default when never synced
*/
pub(crate) fn state(stores: &Stores) -> SyncState {
    fs::read_to_string(stores.directory.join("mongodb-state.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/** Write the sync status
 * Input
    - stores: &Stores - runtime stores
    - state: &SyncState - status
 * Output
    - None
*/
fn save_state(stores: &Stores, state: &SyncState) {
    let _ = write_atomic(
        &stores.directory.join("mongodb-state.json"),
        serde_json::to_string_pretty(state)
            .unwrap_or_default()
            .as_bytes(),
    );
}

/** Synchronize the local stores with MongoDB under the sync lock, recording the outcome
 * Input
    - workspace: &Workspace - repository
    - stores: &Stores - runtime stores
 * Output
    - Result<Flushed, String> what was copied (nothing when MongoDB is not configured)
    - Error if MongoDB is configured but the sync failed (the local stores are unaffected)
*/
pub(crate) fn sync(workspace: &Workspace, stores: &Stores) -> Result<Flushed, String> {
    let _lock = Lock::acquire(&stores.directory.join("mongodb-sync.lock"))?;
    let mut status = state(stores);
    status.last_attempt_at = now_millis();
    save_state(stores, &status);
    let result = database(workspace).and_then(|connected| match connected {
        None => Ok(None),
        Some((database, settings)) => {
            push(&database, &settings, workspace, stores, &mut status).map(Some)
        }
    });
    match &result {
        Ok(Some(_)) => {
            status.last_success_at = Some(now_millis());
            status.last_error = None;
        }
        Ok(None) => status.last_error = Some("MongoDB is not configured".into()),
        Err(error) => status.last_error = Some(super::redact_uri(error)),
    }
    save_state(stores, &status);
    result.map(Option::unwrap_or_default)
}

/** Read a BSON number as u64
 * Input
    - value: Option<&Bson> - number
 * Output
    - u64, 0 when absent or not a number
*/
fn number(value: Option<&Bson>) -> u64 {
    match value {
        Some(Bson::Int32(number)) => *number as u64,
        Some(Bson::Int64(number)) => *number as u64,
        Some(Bson::Double(number)) => *number as u64,
        _ => 0,
    }
}

/** Return the highest value of a field for this repository in a collection
 * Input
    - database: &Database - database
    - collection: &str - collection
    - repository_id: &str - repository
    - field: &str - numeric field
 * Output
    - Result<u64, String> highest value, 0 when empty
*/
fn highest(
    database: &Database,
    collection: &str,
    repository_id: &str,
    field: &str,
) -> Result<u64, String> {
    let found = database
        .collection::<Document>(collection)
        .find_one(doc! {"repository_id": repository_id})
        .sort(doc! {field: -1})
        .run()
        .map_err(io_error)?;
    Ok(found.map_or(0, |document| number(document.get(field))))
}

/** Convert a JSON value to a BSON document, adding the repository identity
 * Input
    - value: Value - JSON object
    - repository_id: &str - repository
 * Output
    - Result<Document, String>
*/
fn document(mut value: Value, repository_id: &str) -> Result<Document, String> {
    value["repository_id"] = Value::String(repository_id.to_string());
    bson::to_document(&value).map_err(io_error)
}

/** Insert documents unordered, counting duplicate-key rejections as already copied
 * Input
    - database: &Database - database
    - name: &str - collection
    - documents: Vec<Document> - documents
 * Output
    - Result<usize, String> duplicates skipped
*/
fn insert(database: &Database, name: &str, documents: Vec<Document>) -> Result<usize, String> {
    let mut duplicates = 0;
    for chunk in documents.chunks(BATCH) {
        let result = database
            .collection::<Document>(name)
            .insert_many(chunk.to_vec())
            .with_options(InsertManyOptions::builder().ordered(false).build())
            .run();
        if let Err(error) = result {
            match *error.kind {
                mongodb::error::ErrorKind::InsertMany(ref failure) => {
                    let write_errors = failure.write_errors.clone().unwrap_or_default();
                    if write_errors.iter().any(|error| error.code != 11000)
                        || failure.write_concern_error.is_some()
                    {
                        return Err(error.to_string());
                    }
                    duplicates += write_errors.len();
                }
                _ => return Err(error.to_string()),
            }
        }
    }
    Ok(duplicates)
}

/** Replace or insert one document
 * Input
    - database: &Database - database
    - name: &str - collection
    - filter: Document - identity of the document
    - value: Document - new content
 * Output
    - Result<(), String>
*/
fn upsert(
    database: &Database,
    name: &str,
    filter: Document,
    value: Document,
) -> Result<(), String> {
    database
        .collection::<Document>(name)
        .replace_one(filter, value)
        .upsert(true)
        .run()
        .map(|_| ())
        .map_err(io_error)
}

/** Reconcile and push: see the module comment
 * Input
    - database: &Database - database
    - settings: &Settings - connection settings
    - workspace: &Workspace - repository
    - stores: &Stores - runtime stores
    - status: &mut SyncState - sync status, updated with the remote positions
 * Output
    - Result<Flushed, String>
*/
fn push(
    database: &Database,
    settings: &Settings,
    workspace: &Workspace,
    stores: &Stores,
    status: &mut SyncState,
) -> Result<Flushed, String> {
    migrate(database)?;
    let repository = workspace.repository_id.as_str();
    let path = stores.directory.join("mongodb-checkpoint.json");
    let mut checkpoint: Checkpoint = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let mut flushed = Flushed::default();
    let remote = [
        highest(database, "events", repository, "sequence")?,
        highest(database, "autonomy_ledger", repository, "seq")?,
        highest(database, "governance_changes", repository, "seq")?,
        database
            .collection::<Document>("alerts")
            .count_documents(doc! {"repository_id": repository})
            .run()
            .map_err(io_error)?,
    ];
    for (local, remote) in [
        &mut checkpoint.events,
        &mut checkpoint.ledger,
        &mut checkpoint.audit,
        &mut checkpoint.alerts,
    ]
    .into_iter()
    .zip(remote)
    {
        if remote < *local {
            *local = remote;
            flushed.reset = true;
        }
    }
    let all_events = stores.events.read_all()?;
    let events = all_events
        .iter()
        .filter(|event| event.sequence > checkpoint.events)
        .collect::<Vec<_>>();
    let mut sessions = BTreeSet::new();
    let mut calls = BTreeSet::new();
    if let Some(last) = events.last().map(|event| event.sequence) {
        let documents = events
            .iter()
            .map(|event| {
                sessions.extend(event.scope.foxx_session_id.clone());
                calls.extend(event.scope.tool_call_id.clone());
                let mut document =
                    document(serde_json::to_value(event).map_err(io_error)?, repository)?;
                document.insert("ingested_at", bson::DateTime::now());
                if event.severity <= Severity::Medium {
                    let expiry = event.occurred_at as i64 + settings.retention_days * 86_400_000;
                    document.insert("expires_at", bson::DateTime::from_millis(expiry));
                }
                Ok(document)
            })
            .collect::<Result<Vec<_>, String>>()?;
        flushed.duplicates += insert(database, "events", documents)?;
        flushed.events = events.len();
        checkpoint.events = last;
    }
    let ledger = stores.ledger.entries()?;
    let entries = ledger
        .iter()
        .filter(|entry| entry.seq > checkpoint.ledger)
        .collect::<Vec<_>>();
    if let Some(last) = entries.last().map(|entry| entry.seq) {
        let documents = entries
            .iter()
            .map(|entry| {
                sessions.insert(entry.session.clone());
                document(serde_json::to_value(entry).map_err(io_error)?, repository)
            })
            .collect::<Result<Vec<_>, String>>()?;
        flushed.duplicates += insert(database, "autonomy_ledger", documents)?;
        flushed.ledger = entries.len();
        checkpoint.ledger = last;
    }
    let governance = State::load(workspace)?;
    let audit = governance
        .audit
        .iter()
        .filter(|entry| entry.seq > checkpoint.audit)
        .collect::<Vec<_>>();
    if let Some(last) = audit.last().map(|entry| entry.seq) {
        let documents = audit
            .iter()
            .map(|entry| document(serde_json::to_value(entry).map_err(io_error)?, repository))
            .collect::<Result<Vec<_>, String>>()?;
        flushed.duplicates += insert(database, "governance_changes", documents)?;
        flushed.audit = audit.len();
        checkpoint.audit = last;
    }
    let alerts = read_lines(&stores.directory.join("alerts.jsonl"))?;
    let new_alerts = alerts
        .iter()
        .skip(checkpoint.alerts as usize)
        .cloned()
        .collect::<Vec<_>>();
    if !new_alerts.is_empty() {
        let documents = new_alerts
            .into_iter()
            .map(|alert| document(alert, repository))
            .collect::<Result<Vec<_>, String>>()?;
        flushed.alerts = documents.len();
        flushed.duplicates += insert(database, "alerts", documents)?;
        checkpoint.alerts = alerts.len() as u64;
    }
    upsert(
        database,
        "repositories",
        doc! {"repository_id": repository},
        document(
            json!({"remote": workspace.remote, "registry_generation": governance.generation(), "updated_at": now_millis()}),
            repository,
        )?,
    )?;
    for session in stores.identity.sessions() {
        upsert(
            database,
            "sessions",
            doc! {"foxx_session_id": &session.foxx_session_id},
            document(
                serde_json::to_value(&session).map_err(io_error)?,
                repository,
            )?,
        )?;
        upsert(
            database,
            "agents",
            doc! {"agent_id": &session.agent_id},
            document(
                json!({"agent_id": session.agent_id, "provider": session.provider, "last_seen_at": session.last_seen_at}),
                repository,
            )?,
        )?;
    }
    for task in stores.identity.tasks() {
        upsert(
            database,
            "tasks",
            doc! {"foxx_task_id": &task.foxx_task_id},
            document(serde_json::to_value(&task).map_err(io_error)?, repository)?,
        )?;
    }
    for record in &governance.registry.selections {
        upsert(
            database,
            "selection_registry",
            doc! {"repository_id": repository, "id": &record.id},
            document(serde_json::to_value(record).map_err(io_error)?, repository)?,
        )?;
    }
    if let Some(manifest) = &governance.registry.manifest {
        upsert(
            database,
            "policy_versions",
            doc! {"repository_id": repository, "generation": manifest.generation as i64},
            document(
                serde_json::to_value(manifest).map_err(io_error)?,
                repository,
            )?,
        )?;
    }
    if flushed.events + flushed.ledger > 0 || flushed.reset {
        let session_records = stores.identity.sessions();
        let rollup = |scope: &str,
                      id: &str,
                      events: Vec<&crate::telemetry::events::Event>,
                      ledger: Vec<&crate::telemetry::ledger::Entry>,
                      sessions: Vec<Value>| {
            let computed = metrics::compute(&events, &ledger, &sessions);
            upsert(
                database,
                "metric_rollups",
                doc! {"repository_id": repository, "scope": scope, "id": id},
                document(
                    json!({"scope": scope, "id": id, "computed_at": now_millis(), "formula_version": metrics::FORMULA_VERSION, "metrics": computed}),
                    repository,
                )?,
            )
        };
        let session_values = session_records
            .iter()
            .map(|session| serde_json::to_value(session).unwrap_or_default())
            .collect::<Vec<_>>();
        rollup(
            "repository",
            repository,
            all_events.iter().collect(),
            ledger.iter().collect(),
            session_values,
        )?;
        flushed.rollups += 1;
        for session in &sessions {
            let records = session_records
                .iter()
                .filter(|record| &record.foxx_session_id == session)
                .map(|record| serde_json::to_value(record).unwrap_or_default())
                .collect();
            rollup(
                "session",
                session,
                all_events
                    .iter()
                    .filter(|event| event.scope.foxx_session_id.as_ref() == Some(session))
                    .collect(),
                ledger
                    .iter()
                    .filter(|entry| &entry.session == session)
                    .collect(),
                records,
            )?;
            flushed.rollups += 1;
        }
        for call in &calls {
            rollup(
                "tool_call",
                call,
                all_events
                    .iter()
                    .filter(|event| event.scope.tool_call_id.as_ref() == Some(call))
                    .collect(),
                ledger
                    .iter()
                    .filter(|entry| entry.tool_call_id.as_ref() == Some(call))
                    .collect(),
                Vec::new(),
            )?;
            flushed.rollups += 1;
        }
    }
    upsert(
        database,
        "ingestion_checkpoints",
        doc! {"repository_id": repository, "stream": "local"},
        document(
            json!({"stream": "local", "events": checkpoint.events, "ledger": checkpoint.ledger, "audit": checkpoint.audit, "alerts": checkpoint.alerts, "at": now_millis()}),
            repository,
        )?,
    )?;
    write_atomic(
        &path,
        serde_json::to_string(&checkpoint)
            .map_err(io_error)?
            .as_bytes(),
    )?;
    status.remote_events = checkpoint.events;
    status.remote_ledger = checkpoint.ledger;
    status.remote_audit = checkpoint.audit;
    status.remote_alerts = checkpoint.alerts;
    status.database = Some(settings.database.clone());
    Ok(flushed)
}
