use serde_json::json;

use super::adapters::{adapter, adf_text, rich_text, split_acceptance, EventKind};
use super::TaskState;

/** Test the state machine: the main path, recoveries, cancellation from any unfinished state, and
 * refused shortcuts
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn state_machine_allows_only_its_transitions() {
    use TaskState::*;
    let path = [
        Received,
        Analyzing,
        ContractProposed,
        Approved,
        Executing,
        Validating,
        PrReady,
        Review,
        Merged,
        Completed,
    ];
    for pair in path.windows(2) {
        assert!(pair[0].allows(pair[1]), "{:?} -> {:?}", pair[0], pair[1]);
    }
    for (from, to) in [
        (Analyzing, Blocked),
        (Blocked, Analyzing),
        (Executing, Degraded),
        (Degraded, Executing),
        (Validating, Executing),
        (Failed, Analyzing),
        (Cancelled, Received),
        (Executing, Cancelled),
        (Review, Cancelled),
    ] {
        assert!(from.allows(to), "{from:?} -> {to:?}");
    }
    for (from, to) in [
        (ContractProposed, Executing),
        (Received, Approved),
        (Executing, Merged),
        (PrReady, Completed),
        (Merged, Cancelled),
        (Completed, Cancelled),
        (Completed, Executing),
        (Blocked, Approved),
    ] {
        assert!(!from.allows(to), "{from:?} -> {to:?}");
    }
    assert_eq!(TaskState::parse("contract-proposed"), Ok(ContractProposed));
    assert_eq!(
        TaskState::parse("PR_READY").map(TaskState::name),
        Ok("PR_READY")
    );
    assert!(TaskState::parse("DONE").is_err());
    assert!(Completed.terminal() && Cancelled.terminal() && !Blocked.terminal());
}

/** Test rich text conversion: ADF code marks and lists, Jira wiki monospace, and Asana HTML
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn rich_text_keeps_code_references() {
    let adf = json!({"type": "doc", "content": [
        {"type": "paragraph", "content": [{"type": "text", "text": "Fix "}, {"type": "text", "text": "Pay.refund", "marks": [{"type": "code"}]}]},
        {"type": "bulletList", "content": [{"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "one"}]}]}]},
        {"type": "codeBlock", "content": [{"type": "text", "text": "ledger_post"}]},
    ]});
    assert_eq!(adf_text(&adf), "Fix `Pay.refund`\n- one\n`ledger_post`\n");
    assert_eq!(
        rich_text(&json!("Fix {{Pay.refund}} now")),
        "Fix `Pay.refund` now"
    );
    assert_eq!(
        rich_text(&json!(
            "<body>Fix <code>Pay.refund</code> &amp; <b>log</b><ul><li>a</li></ul></body>"
        )),
        "Fix `Pay.refund` & log- a\n"
    );
    assert_eq!(rich_text(&json!(null)), "");
}

/** Test extraction of acceptance criteria sections in markdown, Jira wiki, and bold forms
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn acceptance_sections_are_split_out() {
    for text in [
        "Do it.\n\n## Acceptance criteria\n- first\n- second\n\nNotes after.",
        "Do it.\nh3. Acceptance Criteria\n* first\n* second\nNotes after.",
        "Do it.\n**Acceptance criteria:**\n1. first\n2. second\n\nNotes after.",
    ] {
        let (description, criteria) = split_acceptance(text);
        assert_eq!(criteria, ["first", "second"], "{text}");
        assert!(
            description.starts_with("Do it.") && description.ends_with("Notes after."),
            "{description}"
        );
    }
    assert_eq!(split_acceptance("No section here").1, Vec::<String>::new());
}

/** Test Jira event classification from webhook bodies
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn jira_events_are_classified() {
    let jira = adapter("jira").unwrap();
    let issue = |status: &str, category: &str, resolution: Option<&str>| {
        json!({"key": "PAY-1", "fields": {"status": {"name": status, "statusCategory": {"key": category}},
            "resolution": resolution.map(|name| json!({"name": name}))}})
    };
    let kind = |body: serde_json::Value| jira.parse(&body, Some("d1")).unwrap().remove(0).kind;
    assert_eq!(
        kind(json!({"webhookEvent": "jira:issue_created", "issue": issue("To Do", "new", None)})),
        EventKind::Created
    );
    assert_eq!(
        kind(json!({"webhookEvent": "jira:issue_deleted", "issue": issue("To Do", "new", None)})),
        EventKind::Cancelled
    );
    assert_eq!(
        kind(
            json!({"webhookEvent": "jira:issue_updated", "changelog": {"items": [{"field": "assignee"}]}, "issue": issue("To Do", "new", None)})
        ),
        EventKind::Assigned
    );
    assert_eq!(
        kind(
            json!({"webhookEvent": "jira:issue_updated", "changelog": {"items": [{"field": "status"}]}, "issue": issue("Done", "done", Some("Done"))})
        ),
        EventKind::Closed
    );
    assert_eq!(
        kind(
            json!({"webhookEvent": "jira:issue_updated", "changelog": {"items": [{"field": "status"}]}, "issue": issue("Closed", "done", Some("Won't Do"))})
        ),
        EventKind::Cancelled
    );
    assert_eq!(
        kind(
            json!({"webhookEvent": "jira:issue_updated", "issue_event_type_name": "issue_reopened", "issue": issue("To Do", "new", None)})
        ),
        EventKind::Reopened
    );
    assert_eq!(
        kind(json!({"webhookEvent": "comment_created", "issue": issue("To Do", "new", None)})),
        EventKind::Ignored
    );
    let parsed = jira
        .parse(
            &json!({"webhookEvent": "jira:issue_created", "issue": issue("To Do", "new", None)}),
            None,
        )
        .unwrap();
    assert!(
        parsed[0].event_id.starts_with("sha256:"),
        "without a delivery id the event is identified by its digest"
    );
    assert!(jira.parse(&json!({"events": []}), None).is_err());
}

/** Test Asana event classification and that non-task events are skipped
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn asana_events_are_classified() {
    let asana = adapter("asana").unwrap();
    let event = |action: &str, field: Option<&str>| json!({"action": action, "resource": {"gid": "42", "resource_type": "task"}, "change": field.map(|field| json!({"field": field}))});
    let body = json!({"events": [
        event("added", None),
        event("changed", Some("assignee")),
        event("changed", Some("completed")),
        event("changed", Some("notes")),
        event("deleted", None),
        event("undeleted", None),
        {"action": "added", "resource": {"gid": "7", "resource_type": "story"}},
    ]});
    let parsed = asana.parse(&body, Some("delivery")).unwrap();
    assert_eq!(
        parsed.iter().map(|event| event.kind).collect::<Vec<_>>(),
        [
            EventKind::Created,
            EventKind::Assigned,
            EventKind::Closed,
            EventKind::Updated,
            EventKind::Cancelled,
            EventKind::Reopened
        ]
    );
    assert_eq!(parsed[0].task_id, "ASANA-42");
    assert_eq!(parsed[1].event_id, "delivery#1");
    assert!(asana
        .parse(&json!({"events": []}), None)
        .unwrap()
        .is_empty());
    assert!(adapter("github")
        .err()
        .unwrap()
        .contains("not implemented yet"));
}
