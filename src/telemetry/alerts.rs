// Alerts: deterministic, rule-driven notifications raised by the hook runtime (low autonomy
// credits, bypass attempts and confirmed bypasses, violations, quarantine, coverage gaps, failed
// verification, suspicious prompts). Each alert is deduplicated with a cooldown, recorded in the
// runtime folder, and delivered to Slack and WhatsApp when configured, by a detached curl process
// that reads its URL, headers, and body from stdin, so credentials never appear in a command line.
// Alert text carries identifiers, the trigger, values, and the action taken, never secrets, raw
// prompts, or tool payloads. Delivery is best effort and never changes a decision.

use serde_json::{json, Value};

use super::events::Event;
use super::identity::SessionRecord;
use super::Stores;
use crate::governance::workspace::Workspace;
use crate::integrations;
use crate::platform::files::{append_line, read_lines, Lock};
use crate::platform::http::post_detached;
use crate::platform::now_unix;

/** Default cooldown between two alerts with the same key, in seconds */
const DEFAULT_COOLDOWN: u64 = 600;

/** Describe an alert kind
 * Input
    - kind: &str - alert kind
 * Output
    - (&'static str, &'static str) title and the action Crane took
*/
fn describe(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "low_credits" => (
            "Low autonomy credits",
            "changes will need human approval once credits run out",
        ),
        "bypass_attempt" => (
            "Attempt to bypass Crane",
            "the action was denied before it ran",
        ),
        "bypass_confirmed" => (
            "Confirmed bypass of Crane",
            "the session was quarantined; the agent was told to restore the code",
        ),
        "violation" => (
            "Policy violation detected after the effect",
            "the session was degraded; the agent was told to repair",
        ),
        "quarantine" => (
            "Agent session quarantined",
            "mutating actions are denied for the rest of the session",
        ),
        "coverage_gap" => (
            "Enforcement coverage gap",
            "the action is recorded as unobserved; verify the hooks",
        ),
        "verification_failed" => (
            "Task verification failed",
            "the agent was asked to repair before finishing",
        ),
        "prompt_injection" => (
            "Suspicious prompt",
            "recorded for review; decisions are unaffected",
        ),
        _ => ("Crane alert", "recorded"),
    }
}

/** Rank a severity name for threshold comparison
 * Input
    - name: &str - critical, high, medium, low
 * Output
    - u8 rank, higher is more severe
*/
fn rank(name: &str) -> u8 {
    match name.to_ascii_lowercase().as_str() {
        "critical" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

/** Raise an alert for an event: deduplicate it, record it, and deliver it to the configured
 * channels; failures are recorded, never propagated
 * Input
    - workspace: &Workspace - repository
    - stores: &Stores - runtime stores
    - kind: &str - alert kind
    - session: &SessionRecord - session
    - event: &Event - triggering event
 * Output
    - None
*/
pub(crate) fn raise(
    workspace: &Workspace,
    stores: &Stores,
    kind: &str,
    session: &SessionRecord,
    event: &Event,
) {
    let _ = try_raise(workspace, stores, kind, session, event);
}

/** Implementation of raise
 * Input
    - workspace: &Workspace - repository
    - stores: &Stores - runtime stores
    - kind: &str - alert kind
    - session: &SessionRecord - session
    - event: &Event - triggering event
 * Output
    - Result<(), String>
*/
fn try_raise(
    workspace: &Workspace,
    stores: &Stores,
    kind: &str,
    session: &SessionRecord,
    event: &Event,
) -> Result<(), String> {
    let path = stores.directory.join("alerts.jsonl");
    let _lock = Lock::acquire(&path.with_extension("lock"))?;
    let key = format!("{kind}:{}", session.foxx_session_id);
    let slack = integrations::load(workspace, "slack");
    let cooldown = slack
        .as_ref()
        .and_then(|integration| {
            integration
                .settings
                .get("cooldown_seconds")
                .and_then(Value::as_u64)
        })
        .unwrap_or(DEFAULT_COOLDOWN);
    let now = now_unix();
    let suppressed = read_lines(&path)?
        .iter()
        .rev()
        .find(|alert| alert["key"] == key.as_str())
        .is_some_and(|alert| now.saturating_sub(alert["at"].as_u64().unwrap_or(0)) < cooldown);
    let (title, action) = describe(kind);
    let severity = format!("{:?}", event.severity).to_ascii_uppercase();
    let observed = event
        .measurements
        .iter()
        .find(|measurement| measurement.name == "credits_available")
        .and_then(|measurement| measurement.value);
    let text = format!(
        "[{severity}] {title}\nAgent: {} ({}) session {} task {}\nTrigger: {}\nObserved: {}  Threshold: {}\nAction taken: {action}{}",
        session.provider,
        session.model.clone().unwrap_or_else(|| "model unknown".into()),
        session.foxx_session_id,
        event.scope.foxx_task_id.clone().unwrap_or_else(|| "-".into()),
        event.reasons.first().cloned().unwrap_or_else(|| event.event_type.clone()),
        observed.map_or("-".into(), |value| value.to_string()),
        if kind == "low_credits" { "low_credit_threshold" } else { "policy" },
        slack
            .as_ref()
            .and_then(|integration| integration.settings.get("dashboard_url").and_then(Value::as_str))
            .map(|url| format!("\nDashboard: {url}/#/sessions/{}", session.foxx_session_id))
            .unwrap_or_default()
    );
    let mut deliveries = Vec::new();
    if !suppressed {
        let event_rank = rank(&severity);
        if let Some(slack) = &slack {
            let minimum = slack
                .settings
                .get("min_severity")
                .and_then(Value::as_str)
                .unwrap_or("high");
            if event_rank >= rank(minimum) {
                deliveries.push(
                    json!({"channel": "slack", "result": deliver_slack(workspace, slack, &text)}),
                );
            }
        }
        if let (Some(jira), Some(issue)) = (
            integrations::load(workspace, "jira"),
            event.scope.external_task_id.as_deref(),
        ) {
            if event_rank >= rank("high") {
                deliveries.push(json!({"channel": "jira", "issue": issue, "result": deliver_jira(workspace, &jira, issue, &text)}));
            }
        }
        if let Some(whatsapp) = integrations::load(workspace, "whatsapp") {
            if event_rank >= rank("high") {
                deliveries.push(json!({"channel": "whatsapp", "result": deliver_whatsapp(workspace, &whatsapp, &text)}));
            }
        }
    }
    let record = json!({
        "alert_id": format!("alr_{}_{}", now, event.sequence),
        "key": key,
        "kind": kind,
        "title": title,
        "severity": severity,
        "at": now,
        "session": session.foxx_session_id,
        "event_id": event.event_id,
        "suppressed": suppressed,
        "state": "OPEN",
        "deliveries": deliveries,
    });
    append_line(&path, &record.to_string())
}

/** Send an alert request without waiting for the answer, unless CRANE_ALERTS_DRY_RUN=1
 * Input
    - url: &str - endpoint
    - headers: &[String] - headers (may contain credentials)
    - body: &Value - JSON body
 * Output
    - String, "queued", "dry-run", or why it could not be sent
*/
fn send(url: &str, headers: &[String], body: &Value) -> String {
    if std::env::var("CRANE_ALERTS_DRY_RUN").is_ok_and(|value| value == "1") {
        return "dry-run".into();
    }
    match post_detached(url, headers, &body.to_string()) {
        Ok(()) => "queued".into(),
        Err(error) => error,
    }
}

/** Deliver an alert to Slack
 * Input
    - workspace: &Workspace - repository
    - slack: &integrations::Integration - Slack settings
    - text: &str - alert text
 * Output
    - String delivery result
*/
fn deliver_slack(workspace: &Workspace, slack: &integrations::Integration, text: &str) -> String {
    let secrets = integrations::secrets(workspace, "slack");
    match slack.settings.get("mode").and_then(Value::as_str) {
        Some("bot") => match (
            secrets.get("bot_token").and_then(Value::as_str),
            slack.settings.get("channel").and_then(Value::as_str),
        ) {
            (Some(token), Some(channel)) => send(
                "https://slack.com/api/chat.postMessage",
                &[format!("Authorization: Bearer {token}")],
                &json!({"channel": channel, "text": text}),
            ),
            _ => "missing bot token or channel".into(),
        },
        _ => match secrets.get("webhook_url").and_then(Value::as_str) {
            Some(url) => send(url, &[], &json!({"text": text})),
            None => "missing webhook URL".into(),
        },
    }
}

/** Deliver an alert as a comment on the Jira issue the session works on (CRANE_TASK_ID), when the
 * issue belongs to the configured project
 * Input
    - workspace: &Workspace - repository
    - jira: &integrations::Integration - Jira settings
    - issue: &str - issue key such as PAY-1821
    - text: &str - alert text
 * Output
    - String delivery result
*/
fn deliver_jira(
    workspace: &Workspace,
    jira: &integrations::Integration,
    issue: &str,
    text: &str,
) -> String {
    let project = jira
        .settings
        .get("project_key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let valid = issue
        .strip_prefix(project)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|number| {
            !number.is_empty() && number.chars().all(|character| character.is_ascii_digit())
        });
    if !valid {
        return format!("skipped: {issue} is not an issue of project {project}");
    }
    let secrets = integrations::secrets(workspace, "jira");
    let (Some(authorization), Some(site), Some(version)) = (
        integrations::jira_authorization(jira, &secrets),
        jira.settings.get("site_url").and_then(Value::as_str),
        jira.settings.get("api_version").and_then(Value::as_str),
    ) else {
        return "incomplete Jira configuration".into();
    };
    let body = if version == "3" {
        json!({"body": {"type": "doc", "version": 1, "content": [{"type": "paragraph", "content": [{"type": "text", "text": text}]}]}})
    } else {
        json!({ "body": text })
    };
    send(
        &format!("{site}/rest/api/{version}/issue/{issue}/comment"),
        &[authorization],
        &body,
    )
}

/** Deliver an alert to WhatsApp recipients through the Cloud API
 * Input
    - workspace: &Workspace - repository
    - whatsapp: &integrations::Integration - WhatsApp settings
    - text: &str - alert text
 * Output
    - String delivery result
*/
fn deliver_whatsapp(
    workspace: &Workspace,
    whatsapp: &integrations::Integration,
    text: &str,
) -> String {
    let secrets = integrations::secrets(workspace, "whatsapp");
    let (Some(token), Some(version), Some(phone)) = (
        secrets.get("access_token").and_then(Value::as_str),
        whatsapp.settings.get("api_version").and_then(Value::as_str),
        whatsapp
            .settings
            .get("phone_number_id")
            .and_then(Value::as_str),
    ) else {
        return "incomplete WhatsApp configuration".into();
    };
    let url = format!("https://graph.facebook.com/{version}/{phone}/messages");
    let mut results = Vec::new();
    for recipient in whatsapp
        .settings
        .get("recipients")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let body = if whatsapp
            .settings
            .get("message_type")
            .and_then(Value::as_str)
            == Some("template")
        {
            json!({
                "messaging_product": "whatsapp",
                "to": recipient,
                "type": "template",
                "template": {
                    "name": whatsapp.settings.get("template_name"),
                    "language": {"code": whatsapp.settings.get("template_language")},
                    "components": [{"type": "body", "parameters": [{"type": "text", "text": text.chars().take(1000).collect::<String>()}]}],
                },
            })
        } else {
            json!({"messaging_product": "whatsapp", "to": recipient, "type": "text", "text": {"body": text}})
        };
        results.push(send(
            &url,
            &[format!("Authorization: Bearer {token}")],
            &body,
        ));
    }
    results.join(", ")
}

/** Read the recorded alerts, newest first
 * Input
    - stores: &Stores - runtime stores
 * Output
    - Vec<Value>
*/
pub(crate) fn list(stores: &Stores) -> Vec<Value> {
    let mut alerts = read_lines(&stores.directory.join("alerts.jsonl")).unwrap_or_default();
    alerts.reverse();
    alerts
}

/** Report whether alert delivery is configured, for compliance evidence
 * Input
    - workspace: &Workspace - repository
 * Output
    - bool
*/
pub(crate) fn configured(workspace: &Workspace) -> bool {
    integrations::load(workspace, "slack").is_some()
        || integrations::load(workspace, "whatsapp").is_some()
}
