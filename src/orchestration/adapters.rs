use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::tasks::TaskInput;
use crate::util::{io_error, sha256};

/** Jira resolutions that mean the work was abandoned rather than done */
const CANCEL_RESOLUTIONS: &[&str] = &[
    "won't do",
    "won't fix",
    "duplicate",
    "cancelled",
    "canceled",
    "declined",
    "obsolete",
];

/** What happened to a task in its tracker, as far as the lifecycle cares
 * Variants
    - Created - the task was created
    - Assigned - the task's assignee changed
    - Updated - other task content changed
    - Reopened - a closed task was reopened
    - Closed - the task was finished in the tracker
    - Cancelled - the task was abandoned, deleted, or removed
    - Ignored - an event the lifecycle does not act on
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventKind {
    Created,
    Assigned,
    Updated,
    Reopened,
    Closed,
    Cancelled,
    Ignored,
}

impl EventKind {
    /** Return the kind's name
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Assigned => "assigned",
            Self::Updated => "updated",
            Self::Reopened => "reopened",
            Self::Closed => "closed",
            Self::Cancelled => "cancelled",
            Self::Ignored => "ignored",
        }
    }
}

/** One tracker event, translated by an adapter
 * Fields
    - event_id: String - delivery id, or a digest of the event, used for idempotency
    - external_id: String - the tracker's own id (Jira key, Asana gid)
    - task_id: String - Crane task id (Jira key, or ASANA-gid)
    - kind: EventKind - what happened
    - detail: String - human-readable summary of the change
    - payload: Value - the issue (Jira) or the event (Asana) as received
*/
pub(crate) struct SourceEvent {
    pub(crate) event_id: String,
    pub(crate) external_id: String,
    pub(crate) task_id: String,
    pub(crate) kind: EventKind,
    pub(crate) detail: String,
    pub(crate) payload: Value,
}

/** A tracker integration: it only translates (events, context, task fields) into Crane's common
 * forms; mapping to repositories, planning, approval, and sessions live in the lifecycle
*/
pub(crate) trait TaskSourceAdapter {
    /** Return the source name
     * Input
        - None (uses self)
     * Output
        - &'static str such as "jira"
    */
    fn name(&self) -> &'static str;

    /** Translate a webhook body into events
     * Input
        - body: &Value - webhook JSON
        - delivery: Option<&str> - delivery id from the transport, if any
     * Output
        - Result<Vec<SourceEvent>, String>
        - Error if the body is not this source's webhook
    */
    fn parse(&self, body: &Value, delivery: Option<&str>) -> Result<Vec<SourceEvent>, String>;

    /** Fetch the task's full context; local mode reads a snapshot under
     * .crane/sources/SOURCE/ (live API fetching needs credentials and is not implemented)
     * Input
        - event: &SourceEvent - event
        - snapshots: &Path - .crane/sources
     * Output
        - Result<Value, String> the task as the tracker's API returns it
        - Error if the context is unavailable
    */
    fn fetch(&self, event: &SourceEvent, snapshots: &Path) -> Result<Value, String>;

    /** Return the project the task belongs to, the key of the repository mapping
     * Input
        - context: &Value - task context
     * Output
        - Option<String>
    */
    fn project(&self, context: &Value) -> Option<String>;

    /** Return every name and id of the task's assignee
     * Input
        - context: &Value - task context
     * Output
        - Vec<String>
    */
    fn assignees(&self, context: &Value) -> Vec<String>;

    /** Confirm that a Closed event still describes the task (Asana reports a change of the
     * completed field, which may also be an un-completion)
     * Input
        - context: &Value - task context
     * Output
        - bool, true when the task is closed
    */
    fn closed(&self, _context: &Value) -> bool {
        true
    }

    /** Normalize the task into the common TaskContractInput (repositories and team are left to
     * the repository mapping)
     * Input
        - event: &SourceEvent - event
        - context: &Value - task context
        - acceptance_field: Option<&str> - tracker field holding acceptance criteria, if configured
     * Output
        - Result<TaskInput, String>
    */
    fn normalize(
        &self,
        event: &SourceEvent,
        context: &Value,
        acceptance_field: Option<&str>,
    ) -> Result<TaskInput, String>;
}

/** Select the adapter for a source name
 * Input
    - name: &str - source name
 * Output
    - Result<Box<dyn TaskSourceAdapter>, String>
    - Error for an unknown or not yet implemented source
*/
pub(crate) fn adapter(name: &str) -> Result<Box<dyn TaskSourceAdapter>, String> {
    match name {
        "jira" => Ok(Box::new(JiraAdapter)),
        "asana" => Ok(Box::new(AsanaAdapter)),
        "linear" | "github" => Err(format!(
            "task source '{name}' is not implemented yet; it needs its own TaskSourceAdapter"
        )),
        other => Err(format!("unknown task source '{other}'; use jira or asana")),
    }
}

/** Flatten Atlassian Document Format into text, keeping inline code and code blocks as
 * backticked spans (so code references survive) and list items as "- " lines
 * Input
    - node: &Value - ADF node
 * Output
    - String
*/
pub(crate) fn adf_text(node: &Value) -> String {
    let children = || {
        node["content"]
            .as_array()
            .map(|children| children.iter().map(adf_text).collect::<String>())
            .unwrap_or_default()
    };
    match node["type"].as_str().unwrap_or_default() {
        "text" => {
            let text = node["text"].as_str().unwrap_or_default();
            let code = node["marks"]
                .as_array()
                .is_some_and(|marks| marks.iter().any(|mark| mark["type"] == "code"));
            if code {
                format!("`{text}`")
            } else {
                text.to_string()
            }
        }
        "hardBreak" => "\n".into(),
        "codeBlock" => format!("`{}`\n", children().trim()),
        "paragraph" | "heading" => format!("{}\n", children()),
        "listItem" => format!("- {}\n", children().trim()),
        _ => children(),
    }
}

/** Turn a rich text value into plain text: Jira wiki markup (where {{code}} becomes `code`),
 * ADF, or Asana HTML (where <code> becomes backticks and other tags are dropped)
 * Input
    - value: &Value - text, ADF document, or null
 * Output
    - String
*/
pub(crate) fn rich_text(value: &Value) -> String {
    match value {
        Value::String(text) if text.contains('<') && text.contains('>') => {
            let text = text
                .replace("<code>", "`")
                .replace("</code>", "`")
                .replace("<li>", "- ")
                .replace("</li>", "\n")
                .replace("<br>", "\n")
                .replace("</p>", "\n");
            let mut plain = String::new();
            let mut inside = false;
            for character in text.chars() {
                match character {
                    '<' => inside = true,
                    '>' if inside => inside = false,
                    _ if !inside => plain.push(character),
                    _ => {}
                }
            }
            plain
                .replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
        }
        Value::String(text) => text.replace("{{", "`").replace("}}", "`"),
        Value::Object(_) => adf_text(value),
        _ => String::new(),
    }
}

/** Split an "Acceptance criteria" section out of a description: the heading line (markdown,
 * Jira wiki, or bold) and the list items after it
 * Input
    - text: &str - description text
 * Output
    - (String, Vec<String>) description without the section, and the criteria
*/
pub(crate) fn split_acceptance(text: &str) -> (String, Vec<String>) {
    let lines = text.lines().collect::<Vec<_>>();
    let heading = lines.iter().position(|line| {
        let cleaned = line
            .trim()
            .trim_start_matches(['#', '*'])
            .trim_start_matches(|character: char| {
                character == 'h' || character.is_ascii_digit() || character == '.'
            })
            .trim()
            .trim_end_matches(['*', ':'])
            .trim()
            .to_ascii_lowercase();
        cleaned == "acceptance criteria"
    });
    let Some(heading) = heading else {
        return (text.trim().to_string(), Vec::new());
    };
    let mut criteria = Vec::new();
    let mut end = heading + 1;
    while end < lines.len() {
        let line = lines[end].trim();
        let item = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("# "))
            .or_else(|| line.strip_prefix("• "))
            .or_else(|| {
                line.split_once(". ")
                    .filter(|(number, _)| {
                        !number.is_empty()
                            && number.chars().all(|character| character.is_ascii_digit())
                    })
                    .map(|(_, rest)| rest)
            });
        match item {
            Some(item) if !item.trim().is_empty() => criteria.push(item.trim().to_string()),
            None if line.is_empty() && criteria.is_empty() => {}
            _ => break,
        }
        end += 1;
    }
    let mut rest = lines[..heading].to_vec();
    rest.extend_from_slice(&lines[end..]);
    (rest.join("\n").trim().to_string(), criteria)
}

/** Return the text values of a list field (strings, or objects with a name or value)
 * Input
    - value: &Value - JSON array, string, or null
 * Output
    - Vec<String>
*/
fn names(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .or_else(|| item["name"].as_str())
                    .or_else(|| item["value"].as_str())
                    .map(String::from)
            })
            .collect(),
        Value::String(text) => vec![text.clone()],
        _ => Vec::new(),
    }
}

/** Return the delivery id, or a digest of the event when the transport sent none
 * Input
    - delivery: Option<&str> - delivery id
    - event: &Value - event JSON
 * Output
    - String
*/
fn event_id(delivery: Option<&str>, event: &Value) -> String {
    match delivery.filter(|delivery| !delivery.is_empty()) {
        Some(delivery) => delivery.to_string(),
        None => sha256(event.to_string().as_bytes()),
    }
}

/** Jira Cloud and Data Center webhooks (jira:issue_created, jira:issue_updated,
 * jira:issue_deleted); the webhook carries the issue, so a local snapshot is only a refresh
*/
pub(crate) struct JiraAdapter;

impl TaskSourceAdapter for JiraAdapter {
    /** Return "jira"
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    fn name(&self) -> &'static str {
        "jira"
    }

    /** Translate a Jira webhook: created is Created, deleted is Cancelled, and an update is
     * Cancelled when resolved with an abandoning resolution, Closed when its status category is
     * done, Reopened for issue_reopened, Assigned when the assignee changed, and Updated otherwise
     * Input
        - body: &Value - webhook JSON
        - delivery: Option<&str> - X-Atlassian-Webhook-Identifier or similar
     * Output
        - Result<Vec<SourceEvent>, String>
    */
    fn parse(&self, body: &Value, delivery: Option<&str>) -> Result<Vec<SourceEvent>, String> {
        let webhook = body["webhookEvent"]
            .as_str()
            .ok_or("not a Jira webhook: 'webhookEvent' is missing")?;
        let issue = &body["issue"];
        let key = issue["key"]
            .as_str()
            .ok_or("Jira webhook has no issue key")?
            .to_string();
        let fields = &issue["fields"];
        let changed = |field: &str| {
            body["changelog"]["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["field"] == field))
        };
        let resolution = fields["resolution"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let done = fields["status"]["statusCategory"]["key"] == "done";
        let (kind, detail) = match webhook {
            "jira:issue_created" => (EventKind::Created, "issue created".to_string()),
            "jira:issue_deleted" => (EventKind::Cancelled, "issue deleted".to_string()),
            "jira:issue_updated" => {
                let status = fields["status"]["name"].as_str().unwrap_or_default();
                if CANCEL_RESOLUTIONS.contains(&resolution.as_str()) {
                    (EventKind::Cancelled, format!("resolved as {resolution}"))
                } else if body["issue_event_type_name"] == "issue_reopened" {
                    (EventKind::Reopened, format!("reopened to {status}"))
                } else if done && changed("status") {
                    (EventKind::Closed, format!("status {status}"))
                } else if changed("assignee") || body["issue_event_type_name"] == "issue_assigned" {
                    (EventKind::Assigned, "assignee changed".to_string())
                } else {
                    (EventKind::Updated, "issue updated".to_string())
                }
            }
            other => (EventKind::Ignored, format!("{other} is not handled")),
        };
        Ok(vec![SourceEvent {
            event_id: event_id(delivery, body),
            task_id: key.clone(),
            external_id: key,
            kind,
            detail,
            payload: issue.clone(),
        }])
    }

    /** Use .crane/sources/jira/issues/KEY.json when present, otherwise the issue in the webhook
     * Input
        - event: &SourceEvent - event
        - snapshots: &Path - .crane/sources
     * Output
        - Result<Value, String>
    */
    fn fetch(&self, event: &SourceEvent, snapshots: &Path) -> Result<Value, String> {
        let path = snapshots
            .join("jira")
            .join("issues")
            .join(format!("{}.json", event.external_id));
        match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content)
                .map_err(|error| format!("{}: {error}", path.display())),
            Err(_) if event.payload["fields"].is_object() => Ok(event.payload.clone()),
            Err(_) => Err(format!("no context for Jira issue {}", event.external_id)),
        }
    }

    /** Return the Jira project key
     * Input
        - context: &Value - issue
     * Output
        - Option<String>
    */
    fn project(&self, context: &Value) -> Option<String> {
        context["fields"]["project"]["key"]
            .as_str()
            .map(String::from)
    }

    /** Return the assignee's display name, account id, email, and user name
     * Input
        - context: &Value - issue
     * Output
        - Vec<String>
    */
    fn assignees(&self, context: &Value) -> Vec<String> {
        let assignee = &context["fields"]["assignee"];
        ["displayName", "accountId", "emailAddress", "name"]
            .iter()
            .filter_map(|key| assignee[*key].as_str().map(String::from))
            .collect()
    }

    /** Normalize a Jira issue: summary, description (wiki or ADF), acceptance criteria from the
     * configured field or the description's section, reporter, priority, and labels plus
     * components
     * Input
        - event: &SourceEvent - event
        - context: &Value - issue
        - acceptance_field: Option<&str> - custom field id such as customfield_10050
     * Output
        - Result<TaskInput, String>
    */
    fn normalize(
        &self,
        event: &SourceEvent,
        context: &Value,
        acceptance_field: Option<&str>,
    ) -> Result<TaskInput, String> {
        let fields = &context["fields"];
        let (description, mut criteria) = split_acceptance(&rich_text(&fields["description"]));
        if let Some(field) = acceptance_field {
            let configured = &fields[field];
            let text = match configured {
                Value::Array(_) => names(configured).join("\n- "),
                other => rich_text(other),
            };
            let items = text
                .lines()
                .map(|line| {
                    line.trim()
                        .trim_start_matches(['-', '*', '#', '•'])
                        .trim()
                        .to_string()
                })
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            if !items.is_empty() {
                criteria = items;
            }
        }
        let mut labels = names(&fields["labels"]);
        labels.extend(
            names(&fields["components"])
                .into_iter()
                .map(|component| format!("component:{component}")),
        );
        TaskInput::from_json(&json!({
            "task_format": 1,
            "task_id": event.task_id,
            "title": fields["summary"],
            "description": description,
            "acceptance_criteria": criteria,
            "requester": fields["reporter"]["emailAddress"].as_str().or_else(|| fields["reporter"]["displayName"].as_str()),
            "priority": fields["priority"]["name"],
            "labels": labels,
        }))
    }
}

/** Asana webhooks: a delivery holds compact events naming a task by gid, so the task itself is
 * fetched (local mode: .crane/sources/asana/tasks/GID.json, as the Asana API returns it)
*/
pub(crate) struct AsanaAdapter;

impl AsanaAdapter {
    /** Return the task object of an API response ({"data": {...}} or the bare object)
     * Input
        - context: &Value - API response
     * Output
        - &Value
    */
    fn task(context: &Value) -> &Value {
        if context["data"].is_object() {
            &context["data"]
        } else {
            context
        }
    }

    /** Return the display value of a custom field by name
     * Input
        - task: &Value - task object
        - name: &str - custom field name
     * Output
        - Option<String>
    */
    fn custom(task: &Value, name: &str) -> Option<String> {
        task["custom_fields"].as_array()?.iter().find_map(|field| {
            field["name"]
                .as_str()
                .filter(|field_name| field_name.eq_ignore_ascii_case(name))
                .and_then(|_| {
                    field["display_value"]
                        .as_str()
                        .or_else(|| field["text_value"].as_str())
                        .or_else(|| field["enum_value"]["name"].as_str())
                })
                .map(String::from)
        })
    }
}

impl TaskSourceAdapter for AsanaAdapter {
    /** Return "asana"
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    fn name(&self) -> &'static str {
        "asana"
    }

    /** Translate an Asana delivery: for each task event, added is Created, deleted or removed is
     * Cancelled, undeleted is Reopened, a change of assignee is Assigned, a change of completed
     * is Closed (confirmed against the task), and other changes are Updated; events about other
     * resources are skipped
     * Input
        - body: &Value - webhook JSON with "events"
        - delivery: Option<&str> - delivery id, combined with each event's position
     * Output
        - Result<Vec<SourceEvent>, String>
    */
    fn parse(&self, body: &Value, delivery: Option<&str>) -> Result<Vec<SourceEvent>, String> {
        let events = body["events"]
            .as_array()
            .ok_or("not an Asana webhook: 'events' is missing")?;
        let mut parsed = Vec::new();
        for (index, event) in events.iter().enumerate() {
            if event["resource"]["resource_type"] != "task" {
                continue;
            }
            let gid = event["resource"]["gid"]
                .as_str()
                .ok_or("Asana event has no task gid")?
                .to_string();
            let field = event["change"]["field"].as_str().unwrap_or_default();
            let kind = match (event["action"].as_str().unwrap_or_default(), field) {
                ("added", _) => EventKind::Created,
                ("deleted" | "removed", _) => EventKind::Cancelled,
                ("undeleted", _) => EventKind::Reopened,
                ("changed", "assignee") => EventKind::Assigned,
                ("changed", "completed") => EventKind::Closed,
                ("changed", _) => EventKind::Updated,
                _ => EventKind::Ignored,
            };
            let id = delivery
                .map(|delivery| format!("{delivery}#{index}"))
                .unwrap_or_else(|| event_id(None, event));
            parsed.push(SourceEvent {
                event_id: id,
                task_id: format!("ASANA-{gid}"),
                external_id: gid,
                kind,
                detail: format!("{} {}", event["action"].as_str().unwrap_or_default(), field)
                    .trim()
                    .to_string(),
                payload: event.clone(),
            });
        }
        Ok(parsed)
    }

    /** Read .crane/sources/asana/tasks/GID.json
     * Input
        - event: &SourceEvent - event
        - snapshots: &Path - .crane/sources
     * Output
        - Result<Value, String>
        - Error when there is no snapshot (live fetching needs API credentials and is not part of
          this MVP)
    */
    fn fetch(&self, event: &SourceEvent, snapshots: &Path) -> Result<Value, String> {
        let path = snapshots
            .join("asana")
            .join("tasks")
            .join(format!("{}.json", event.external_id));
        let content = fs::read_to_string(&path).map_err(|error| {
            format!(
                "no local snapshot of Asana task {} at {} ({}); live Asana fetching needs API credentials and is not implemented in local mode",
                event.external_id,
                path.display(),
                io_error(error)
            )
        })?;
        serde_json::from_str(&content).map_err(|error| format!("{}: {error}", path.display()))
    }

    /** Return the first project gid of the task
     * Input
        - context: &Value - API response
     * Output
        - Option<String>
    */
    fn project(&self, context: &Value) -> Option<String> {
        let task = Self::task(context);
        task["projects"]
            .as_array()
            .and_then(|projects| projects.first())
            .and_then(|project| project["gid"].as_str())
            .or_else(|| task["memberships"][0]["project"]["gid"].as_str())
            .map(String::from)
    }

    /** Return the assignee's name and gid
     * Input
        - context: &Value - API response
     * Output
        - Vec<String>
    */
    fn assignees(&self, context: &Value) -> Vec<String> {
        let assignee = &Self::task(context)["assignee"];
        ["name", "gid", "email"]
            .iter()
            .filter_map(|key| assignee[*key].as_str().map(String::from))
            .collect()
    }

    /** Report whether the task is completed
     * Input
        - context: &Value - API response
     * Output
        - bool
    */
    fn closed(&self, context: &Value) -> bool {
        Self::task(context)["completed"] == true
    }

    /** Normalize an Asana task: name, notes (rich text when present), acceptance criteria from a
     * custom field or the notes' section, creator, priority custom field, and tags
     * Input
        - event: &SourceEvent - event
        - context: &Value - API response
        - acceptance_field: Option<&str> - custom field name holding acceptance criteria
     * Output
        - Result<TaskInput, String>
    */
    fn normalize(
        &self,
        event: &SourceEvent,
        context: &Value,
        acceptance_field: Option<&str>,
    ) -> Result<TaskInput, String> {
        let task = Self::task(context);
        let notes = if task["html_notes"].is_string() {
            rich_text(&task["html_notes"])
        } else {
            rich_text(&task["notes"])
        };
        let (description, mut criteria) = split_acceptance(&notes);
        if let Some(text) = Self::custom(task, acceptance_field.unwrap_or("Acceptance Criteria")) {
            let items = text
                .lines()
                .map(|line| {
                    line.trim()
                        .trim_start_matches(['-', '*', '•'])
                        .trim()
                        .to_string()
                })
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            if !items.is_empty() {
                criteria = items;
            }
        }
        TaskInput::from_json(&json!({
            "task_format": 1,
            "task_id": event.task_id,
            "title": task["name"],
            "description": description,
            "acceptance_criteria": criteria,
            "requester": task["created_by"]["name"].as_str().or_else(|| task["created_by"]["gid"].as_str()),
            "priority": Self::custom(task, "Priority"),
            "labels": names(&task["tags"]),
        }))
    }
}
