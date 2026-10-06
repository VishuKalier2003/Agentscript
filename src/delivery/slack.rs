// Slack: the messages Crane posts about a delivery (Block Kit, with buttons to view the pull
// request and the contract, approve, reject, request changes, and approve an exception where one
// is allowed) and the verification and parsing of the button clicks Slack sends back. Requests are
// authenticated with Slack's signing secret (HMAC-SHA256 over "v0:timestamp:body"), so only Slack
// can approve, and only users mapped to approvers count.

use serde_json::{json, Value};

use crate::util::hmac_sha256;

/** How old a signed Slack request may be, in seconds (Slack's own recommendation) */
pub(crate) const MAX_AGE: u64 = 5 * 60;

/** Actions a Slack button can carry */
pub(crate) const ACTIONS: &[&str] = &[
    "view_pr",
    "view_contract",
    "approve",
    "reject",
    "request_changes",
    "approve_exception",
];

/** Verify a Slack request signature, in constant time over the digest
 * Input
    - secret: &str - signing secret
    - timestamp: &str - X-Slack-Request-Timestamp
    - body: &str - raw request body
    - signature: &str - X-Slack-Signature ("v0=hex")
    - now: u64 - Unix seconds
 * Output
    - Result<(), String>
    - Error for a stale, malformed, or wrong signature
*/
pub(crate) fn verify(
    secret: &str,
    timestamp: &str,
    body: &str,
    signature: &str,
    now: u64,
) -> Result<(), String> {
    let sent = timestamp
        .trim()
        .parse::<u64>()
        .map_err(|_| "the Slack request timestamp is not a number".to_string())?;
    if now.abs_diff(sent) > MAX_AGE {
        return Err("the Slack request is older than five minutes (possible replay)".into());
    }
    let expected = format!(
        "v0={}",
        hmac_sha256(
            secret.as_bytes(),
            format!("v0:{timestamp}:{body}").as_bytes()
        )
    );
    let same = expected.len() == signature.len()
        && expected
            .bytes()
            .zip(signature.bytes())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0;
    if same {
        Ok(())
    } else {
        Err("the Slack request signature does not match".into())
    }
}

/** Decode application/x-www-form-urlencoded text
 * Input
    - text: &str - encoded text
 * Output
    - String
*/
pub(crate) fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                    .ok_or(())
                {
                    Ok(byte) => {
                        out.push(byte);
                        index += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/** One parsed button click
 * Fields
    - action: String - one of ACTIONS
    - user: String - Slack user id
    - delivery: String - delivery id (the session id)
    - head: String - the commit the message was about
    - binding: Option<String> - the delivery binding the message was about (repository, task,
      contract, commit, checks, attestation), absent in messages sent before bindings
    - check: Option<String> - the check an exception is for
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Click {
    pub(crate) action: String,
    pub(crate) user: String,
    pub(crate) delivery: String,
    pub(crate) head: String,
    pub(crate) binding: Option<String>,
    pub(crate) check: Option<String>,
}

/** Parse a Slack interaction request body ("payload=" and the URL-encoded JSON of a block_actions
 * interaction) into the click it carries
 * Input
    - body: &str - raw request body
 * Output
    - Result<Click, String>
*/
pub(crate) fn parse(body: &str) -> Result<Click, String> {
    let encoded = body
        .split('&')
        .find_map(|pair| pair.strip_prefix("payload="))
        .ok_or("the Slack request has no payload")?;
    let payload: Value = serde_json::from_str(&url_decode(encoded))
        .map_err(|error| format!("the Slack payload is not JSON: {error}"))?;
    if payload["type"] != "block_actions" {
        return Err(format!("unsupported Slack interaction {}", payload["type"]));
    }
    let action = &payload["actions"][0];
    let action_id = action["action_id"].as_str().unwrap_or_default().to_string();
    if !ACTIONS.contains(&action_id.as_str()) {
        return Err(format!("unknown Slack action '{action_id}'"));
    }
    let value: Value = serde_json::from_str(action["value"].as_str().unwrap_or("{}"))
        .map_err(|_| "the Slack button value is not Crane's".to_string())?;
    Ok(Click {
        action: action_id,
        user: payload["user"]["id"]
            .as_str()
            .ok_or("the Slack payload names no user")?
            .to_string(),
        delivery: value["delivery"]
            .as_str()
            .ok_or("the Slack button names no delivery")?
            .to_string(),
        head: value["head"]
            .as_str()
            .ok_or("the Slack button names no commit")?
            .to_string(),
        binding: value["binding"].as_str().map(String::from),
        check: value["check"].as_str().map(String::from),
    })
}

/** Build the chat.postMessage body announcing a delivery round, with its buttons
 * Input
    - channel: &str - channel or person
    - delivery: &Value - the delivery state
    - eligibility: &Value - the merge evaluation
    - pr_url: &str - pull request link
    - contract_url: &str - contract link
    - exceptable: &[String] - failing checks a human may except
 * Output
    - Value
*/
pub(crate) fn announcement(
    channel: &str,
    delivery: &Value,
    eligibility: &Value,
    pr_url: &str,
    contract_url: &str,
    exceptable: &[String],
) -> Value {
    let id = delivery["delivery_id"].as_str().unwrap_or_default();
    let head = delivery["head"].as_str().unwrap_or_default();
    let binding = delivery["binding_digest"].as_str();
    // Every button names exactly what it decides on; the binding digest covers it all
    let value = |check: Option<&str>| {
        json!({
            "delivery": id,
            "head": head,
            "binding": binding,
            "repository": delivery["binding"]["repository"]["name"],
            "task": delivery["task"],
            "contract": delivery["binding"]["contract_digest"],
            "attestation": delivery["attestation_digest"],
            "check": check,
        })
        .to_string()
    };
    let checks = delivery["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|check| {
            format!(
                "{} {}",
                if check["status"] == "passed" {
                    ":white_check_mark:"
                } else {
                    ":x:"
                },
                check["name"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("  ");
    let missing = eligibility["missing"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let mut buttons = vec![
        json!({"type": "button", "action_id": "view_pr", "text": {"type": "plain_text", "text": "View PR"}, "url": pr_url, "value": value(None)}),
        json!({"type": "button", "action_id": "view_contract", "text": {"type": "plain_text", "text": "View contract"}, "url": contract_url, "value": value(None)}),
    ];
    if eligibility["auto_merge"] != true {
        buttons.push(json!({"type": "button", "action_id": "approve", "style": "primary", "text": {"type": "plain_text", "text": "Approve"}, "value": value(None)}));
        buttons.push(json!({"type": "button", "action_id": "reject", "style": "danger", "text": {"type": "plain_text", "text": "Reject"}, "value": value(None)}));
        buttons.push(json!({"type": "button", "action_id": "request_changes", "text": {"type": "plain_text", "text": "Request changes"}, "value": value(None)}));
    }
    for check in exceptable {
        buttons.push(json!({"type": "button", "action_id": "approve_exception", "text": {"type": "plain_text", "text": format!("Approve exception: {check}")}, "value": value(Some(check)),
            "confirm": {"title": {"type": "plain_text", "text": "Approve a scoped exception?"}, "text": {"type": "mrkdwn", "text": format!("Only *{check}* on this commit, for this session, until it expires.")}, "confirm": {"type": "plain_text", "text": "Approve"}, "deny": {"type": "plain_text", "text": "Cancel"}}}));
    }
    let binding = &delivery["binding"];
    let text = format!(
        "*{}* {} — delivery of {} ({}), {} change by a {} session\nRepository: {} · task {} · commit `{}`\nContract: {} `{}` · attestation `{}` · binding `{}`\nChecks: {}\n{}",
        delivery["pull_request"]["title"].as_str().unwrap_or("Pull request"),
        pr_url,
        id,
        delivery["task"].as_str().unwrap_or("no task"),
        eligibility["criticality"].as_str().unwrap_or_default(),
        eligibility["autonomy"].as_str().unwrap_or_default(),
        match (binding["repository"]["owner"].as_str(), binding["repository"]["name"].as_str()) {
            (Some(owner), Some(name)) => format!("{owner}/{name}"),
            (None, Some(name)) => name.to_string(),
            _ => binding["repository"]["identity"].as_str().unwrap_or("this repository").to_string(),
        },
        delivery["task"].as_str().unwrap_or("none"),
        head,
        binding["contract_id"].as_str().unwrap_or("contract"),
        binding["contract_digest"].as_str().or(delivery["contract_version"].as_str()).unwrap_or_default(),
        delivery["attestation_digest"].as_str().unwrap_or_default(),
        delivery["binding_digest"].as_str().unwrap_or_default(),
        if checks.is_empty() { "none configured".to_string() } else { checks },
        if missing.is_empty() { "Eligible to merge.".to_string() } else { format!("Waiting for: {}", missing.join("; ")) },
    );
    json!({
        "channel": channel,
        "text": text,
        "blocks": [
            {"type": "section", "text": {"type": "mrkdwn", "text": text}},
            {"type": "actions", "block_id": format!("crane-delivery-{id}"), "elements": buttons},
        ],
    })
}

/** Build a plain chat.postMessage body (status updates)
 * Input
    - channel: &str - channel or person
    - text: &str - message
 * Output
    - Value
*/
pub(crate) fn message(channel: &str, text: &str) -> Value {
    json!({"channel": channel, "text": text})
}
