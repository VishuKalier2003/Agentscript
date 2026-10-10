// Reading the Foxx data model back from MongoDB for the dashboard: this repository's events in
// sequence order, ledger entries, sessions, tasks, and alerts. Documents are decoded into the same
// types the local stores use, so every view, metric, and integrity check works on either source.

use mongodb::bson::{self, doc, Document};
use mongodb::sync::Database;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::governance::workspace::Workspace;
use crate::platform::io_error;
use crate::telemetry::events::Event;
use crate::telemetry::identity::{SessionRecord, TaskRecord};
use crate::telemetry::ledger::Entry;

/** The repository's data as stored in MongoDB
 * Fields
    - events: Vec<Event> - events in sequence order
    - ledger: Vec<Entry> - ledger entries in sequence order
    - sessions: Vec<SessionRecord> - sessions, most recent first
    - tasks: Vec<TaskRecord> - tasks, most recent first
    - alerts: Vec<Value> - alerts, newest first
*/
pub(crate) struct Remote {
    pub(crate) events: Vec<Event>,
    pub(crate) ledger: Vec<Entry>,
    pub(crate) sessions: Vec<SessionRecord>,
    pub(crate) tasks: Vec<TaskRecord>,
    pub(crate) alerts: Vec<Value>,
}

/** Read and decode one collection of this repository
 * Input
    - database: &Database - database
    - collection: &str - collection
    - repository_id: &str - repository
    - sort: Document - sort order
 * Output
    - Result<Vec<T>, String>
    - Error if the query fails or a document cannot be decoded
*/
fn read<T: DeserializeOwned>(
    database: &Database,
    collection: &str,
    repository_id: &str,
    sort: Document,
) -> Result<Vec<T>, String> {
    let cursor = database
        .collection::<Document>(collection)
        .find(doc! {"repository_id": repository_id})
        .sort(sort)
        .projection(doc! {"_id": 0, "ingested_at": 0, "expires_at": 0})
        .run()
        .map_err(io_error)?;
    cursor
        .map(|document| {
            let document = document.map_err(io_error)?;
            bson::from_document(document).map_err(|error| format!("{collection}: {error}"))
        })
        .collect()
}

/** Load the repository's data from MongoDB
 * Input
    - workspace: &Workspace - repository
 * Output
    - Result<Option<Remote>, String>, None when MongoDB is not configured
    - Error if MongoDB is configured but cannot be read
*/
pub(crate) fn load(workspace: &Workspace) -> Result<Option<Remote>, String> {
    let Some((database, _)) = super::database(workspace)? else {
        return Ok(None);
    };
    let repository = workspace.repository_id.as_str();
    let alerts: Vec<Document> = read(&database, "alerts", repository, doc! {"at": -1})?;
    Ok(Some(Remote {
        events: read(&database, "events", repository, doc! {"sequence": 1})?,
        ledger: read(&database, "autonomy_ledger", repository, doc! {"seq": 1})?,
        sessions: read(&database, "sessions", repository, doc! {"last_seen_at": -1})?,
        tasks: read(&database, "tasks", repository, doc! {"started_at": -1})?,
        alerts: alerts
            .into_iter()
            .map(|alert| serde_json::to_value(alert).map_err(io_error))
            .collect::<Result<_, _>>()?,
    }))
}
