use serde_json::{json, Value};

use super::*;
use crate::agent_session::Governance;
use crate::autonomy::AutonomyPolicy;
use crate::budget::BudgetModel;

/** Every field the task requires on an evidence record */
const RECORD_FIELDS: &[&str] = &[
    "organization",
    "team",
    "agent",
    "session",
    "task",
    "contract",
    "checkpoints",
    "autonomy",
    "safety",
    "budget",
    "tool",
    "operation",
    "resources",
    "decision",
    "violations",
    "repair",
    "tests",
    "human",
];

/** Every section the task requires in an attestation */
const ATTESTATION_SECTIONS: &[&str] = &[
    "task",
    "contract",
    "checkpoints",
    "agent",
    "policy_versions",
    "action_summary",
    "denied_actions",
    "violations",
    "contract_tests",
    "ordinary_tests",
    "final_state",
    "approvals",
    "final_decision",
];

/** A bound document like session.json, for a delegated session on task PAY-7
 * Input
    - None
 * Output
    - Value
*/
fn binding() -> Value {
    let mut governance = Governance::plain();
    governance.autonomy_policy = Some(AutonomyPolicy::default());
    governance.budget_model =
        Some(BudgetModel::from_json(&json!({"max": {"delegated": 20}})).unwrap());
    governance.organization = Some(json!({"organization": "acme", "team": "payments"}));
    governance.task_title = Some("Reject negative refunds".into());
    json!({
        "session_format": 3,
        "session_id": "claude-s1",
        "agent": "claude",
        "provider_session": "s1",
        "task_id": "PAY-7",
        "contracts": {"version": "sha256:contract", "contracts": [{"policy_id": "core", "contract_hash": "sha256:core", "version": "sha256:v", "checkpoint": "baseline", "checkpoint_sha": "abc123"}]},
        "governance": governance.to_json(),
        "binding_digest": "sha256:binding",
    })
}

/** Stamp and chain journal events the way ContractSession::record does
 * Input
    - bodies: Vec<Value> - event bodies
 * Output
    - Vec<Value>
*/
fn journal(bodies: Vec<Value>) -> Vec<Value> {
    let mut previous = GENESIS.to_string();
    bodies
        .into_iter()
        .enumerate()
        .map(|(index, mut event)| {
            event["seq"] = json!(index + 1);
            event["at"] = json!(1_000 + index as u64);
            event["session_id"] = json!("claude-s1");
            event["contract_version"] = json!("sha256:contract");
            event["checkpoints"] = json!(["baseline@abc123"]);
            event["chain"] = json!(link(&previous, &event));
            previous = event["chain"].as_str().unwrap().to_string();
            event
        })
        .collect()
}

/** A whole session: start, an autonomous write, a denied write, an approved write, a violation, a
 * verified repair, a human refill, a stop, and finalization
 * Input
    - None
 * Output
    - Vec<Value>
*/
fn lifecycle() -> Vec<Value> {
    journal(vec![
        json!({"event": "session_start", "source": "manager", "model": "opus"}),
        json!({"event": "pre_tool_use", "tool": "Write", "operation": "write", "resources": ["file:docs/a.md"], "decision": "allow", "reasons": ["no contract clause covers this change"], "arguments_digest": "sha256:a", "summary": {"files": 1, "changes": ["write"]}, "budget_reserve": {"amount": 3}}),
        json!({"event": "post_tool_use", "tool": "Write", "operation": "write", "arguments_digest": "sha256:a", "verification": "pass", "effect": {"files": {"modified": ["docs/a.md"]}, "violations": []}, "budget_consume": {"amount": 3, "compliant": true}}),
        json!({"event": "pre_tool_use", "tool": "Edit", "operation": "write", "resources": ["symbol:function:PaymentService.charge"], "decision": "deny", "reasons": ["preserved"], "arguments_digest": "sha256:b"}),
        json!({"event": "pre_tool_use", "tool": "Edit", "operation": "write", "resources": ["file:pay.py"], "decision": "approval_required", "reasons": ["critical zone"], "arguments_digest": "sha256:c"}),
        json!({"event": "post_tool_use", "tool": "Edit", "operation": "write", "arguments_digest": "sha256:c", "verification": "pass", "effect": {"files": {"modified": ["pay.py"]}, "violations": []}}),
        json!({"event": "post_tool_use", "tool": "Bash", "operation": "execute", "arguments_digest": "sha256:d", "summary": {"programs": ["python"], "arguments": 1}, "verification": "fail", "effect": {"files": {"modified": ["pay.py"]}, "violations": ["source_changed"]}, "budget_consume": {"amount": 4, "compliant": false}}),
        json!({"event": "autonomy", "trigger": "violation", "kind": "source_changed", "actor": "crane", "budget_penalty": {"violation": "source_changed", "amount": 10}}),
        json!({"event": "post_tool_use", "tool": "Bash", "operation": "execute", "arguments_digest": "sha256:e", "verification": "pass", "effect": {"files": {"modified": ["pay.py"]}, "violations": []}, "budget_consume": {"amount": 1, "compliant": true}}),
        json!({"event": "autonomy", "trigger": "evidence", "condition": "verified_repair", "actor": "crane"}),
        json!({"event": "budget", "kind": "refill", "amount": 8, "approver": "lead", "reason": "reviewed", "expires_at": 99_999}),
        json!({"event": "stop", "final_status": "PASS", "tests": [{"language": "python", "status": "passed"}]}),
        json!({"event": "session_finalized", "final_status": "PASS", "findings": [], "contract_tests": {"passed": 3, "failed": 0, "not_applicable": 1, "failed_tests": []}, "ordinary_tests": [{"language": "python", "status": "passed", "files": 1}]}),
    ])
}

/** A chained journal verifies; editing, removing, reordering, or appending unchained events breaks
 * it; a journal from before chaining is legacy
 */
#[test]
fn the_journal_chain_detects_tampering() {
    let events = lifecycle();
    assert_eq!(verify_chain(&events)["status"], "verified");
    assert_eq!(
        verify_chain(&events)["head"],
        events.last().unwrap()["chain"]
    );

    let mut edited = events.clone();
    edited[3]["decision"] = json!("allow");
    let result = verify_chain(&edited);
    assert_eq!(result["status"], "broken");
    assert!(result["problem"].as_str().unwrap().contains("event 4"));

    let mut removed = events.clone();
    removed.remove(5);
    assert_eq!(verify_chain(&removed)["status"], "broken");

    let mut reordered = events.clone();
    reordered.swap(1, 2);
    assert_eq!(verify_chain(&reordered)["status"], "broken");

    let mut forged = events.clone();
    let mut extra = json!({"event": "budget", "kind": "refill", "amount": 999, "seq": events.len() + 1, "at": 5_000});
    extra["chain"] = json!("sha256:made-up");
    forged.push(extra);
    assert_eq!(verify_chain(&forged)["status"], "broken");

    let legacy = events
        .iter()
        .map(|event| {
            let mut event = event.clone();
            event.as_object_mut().unwrap().remove("chain");
            event
        })
        .collect::<Vec<_>>();
    assert_eq!(verify_chain(&legacy)["status"], "legacy");
    assert_eq!(verify_chain(&[])["status"], "empty");
}

/** Every evidence record carries every required field, with the state replayed: budget before and
 * after, autonomy and safety, violations, repairs, approvals, and human interventions
 */
#[test]
fn evidence_records_carry_every_field() {
    let records = records(&binding(), &lifecycle()).unwrap();
    assert_eq!(records.len(), lifecycle().len());
    for record in &records {
        for field in RECORD_FIELDS {
            assert!(
                record.get(*field).is_some(),
                "record {} lacks {field}",
                record["seq"]
            );
        }
        assert_eq!(record["organization"], "acme");
        assert_eq!(record["team"], "payments");
        assert_eq!(record["task"], "PAY-7");
    }
    assert_eq!(records[1]["budget"]["before"]["available"], 20);
    assert_eq!(records[1]["budget"]["after"]["available"], 17);
    assert_eq!(records[2]["budget"]["after"]["current"], 17);
    assert_eq!(records[3]["decision"], "deny");
    assert_eq!(records[5]["human"]["action"], "approved_tool_call");
    assert_eq!(records[6]["violations"], json!(["source_changed"]));
    assert_eq!(records[7]["safety"], "degraded");
    assert_eq!(records[7]["budget"]["after"]["current"], 3);
    assert_eq!(records[8]["repair"]["kind"], "compliance_restored");
    assert_eq!(records[9]["repair"]["kind"], "verified_repair");
    assert_eq!(records[9]["safety"], "active");
    assert_eq!(records[10]["human"]["action"], "refill");
    assert_eq!(records[10]["budget"]["after"]["current"], 10);
    assert_eq!(records[12]["tests"]["contract"]["passed"], 3);
}

/** The attestation has every required section, reflects the evidence, and is deterministic: the
 * same evidence gives the same bytes; any change to the evidence changes its digest
 */
#[test]
fn attestation_is_complete_and_deterministic() {
    let first = attest(&binding(), &lifecycle()).unwrap();
    for section in ATTESTATION_SECTIONS {
        assert!(first.get(*section).is_some(), "attestation lacks {section}");
    }
    assert_eq!(
        first.to_string(),
        attest(&binding(), &lifecycle()).unwrap().to_string()
    );
    assert_eq!(first["task"]["task_id"], "PAY-7");
    assert_eq!(first["task"]["title"], "Reject negative refunds");
    assert_eq!(first["agent"]["models"], json!(["opus"]));
    assert_eq!(first["denied_actions"].as_array().unwrap().len(), 1);
    assert_eq!(first["approvals"][0]["outcome"], "approved_and_executed");
    assert_eq!(first["violations"][0]["kind"], "source_changed");
    assert_eq!(first["repairs"].as_array().unwrap().len(), 2);
    assert_eq!(
        first["action_summary"]["files_changed"],
        json!(["docs/a.md", "pay.py"])
    );
    assert_eq!(
        first["action_summary"]["by_decision"],
        json!({"allow": 1, "approval_required": 1, "deny": 1})
    );
    assert_eq!(first["action_summary"]["budget_consumed"], 8);
    assert_eq!(first["final_state"]["lifecycle"], "finalized");
    assert_eq!(first["final_state"]["safety"], "active");
    assert_eq!(first["final_decision"]["decision"], "PASS");
    assert_eq!(first["evidence"]["chain"]["status"], "verified");
    let human = first["human_interventions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            entry["intervention"]["action"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(human, ["start", "approved_tool_call", "refill"]);

    let mut changed = lifecycle();
    changed.truncate(changed.len() - 1);
    let unfinished = attest(&binding(), &changed).unwrap();
    assert_ne!(
        unfinished["attestation_digest"],
        first["attestation_digest"]
    );
    assert_eq!(unfinished["final_decision"]["decision"], "INCOMPLETE");
    assert_eq!(unfinished["final_state"]["lifecycle"], "open");
}

/** An export carries everything needed: re-deriving from it alone gives the same evidence and
 * attestation; a tampered export does not verify
 */
#[test]
fn exports_reconstruct_and_detect_tampering() {
    let exported = export(&binding(), "finalized", &lifecycle()).unwrap();
    let rebuilt = reconstruct(&exported).unwrap();
    assert_eq!(rebuilt["evidence_consistent"], true);
    assert_eq!(rebuilt["attestation_consistent"], true);
    assert_eq!(rebuilt["chain"]["status"], "verified");

    let mut tampered = exported.clone();
    tampered["journal"][3]["decision"] = json!("allow");
    let rebuilt = reconstruct(&tampered).unwrap();
    assert_eq!(rebuilt["chain"]["status"], "broken");
    assert_eq!(rebuilt["attestation_consistent"], false);
}

/** Shell commands are summarized without their arguments: secrets never survive */
#[test]
fn command_summaries_keep_no_arguments() {
    let summary = summarize_command("API_TOKEN=s3cr3t ./bin/deploy --password hunter2 | tee /tmp/log && curl -H 'Authorization: Bearer s3cr3t' https://x");
    assert_eq!(summary["programs"], json!(["deploy", "tee", "curl"]));
    assert_eq!(summary["arguments"], 8);
    let text = summary.to_string();
    for secret in ["s3cr3t", "hunter2", "Bearer", "/tmp/log", "https://x"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
}

/** OpenTelemetry output is derived from the evidence: one trace, a root span, one span per record,
 * failing spans for violations and denials
 */
#[test]
fn otlp_is_derived_from_evidence() {
    let records = records(&binding(), &lifecycle()).unwrap();
    let value = otlp(&binding(), &records);
    let spans = value["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .unwrap();
    assert_eq!(spans.len(), records.len() + 1);
    let trace = spans[0]["traceId"].as_str().unwrap();
    assert_eq!(trace.len(), 32);
    assert!(spans.iter().all(|span| span["traceId"] == trace));
    assert!(spans[1..]
        .iter()
        .all(|span| span["parentSpanId"] == spans[0]["spanId"]));
    assert_eq!(spans[4]["name"], "crane.authorization.pre_tool_use");
    assert_eq!(spans[4]["status"]["code"], 2, "a denial is an error span");
    assert_eq!(spans[2]["status"]["code"], 1);
    assert_eq!(value, otlp(&binding(), &records));
}
