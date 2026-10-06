use serde_json::json;

use super::editor::{agentscript, draft};
use super::simulator::{history, simulate, Action, History};
use super::SCREENS;
use crate::policy::parse;

/** A draft for a payments policy with one rule of each kind
 * Input
    - None
 * Output
    - serde_json::Value
*/
fn payments_draft() -> serde_json::Value {
    json!({"name": "payments_core", "checkpoint": "baseline", "rules": [
        {"rule": "preserve", "kind": "function", "target": "PaymentService.charge", "scope": "block"},
        {"rule": "preserve", "kind": "class", "target": "Ledger", "scope": "file"},
        {"rule": "target", "kind": "function", "target": "PaymentService.refund", "scope": "block", "change_type": "logical_bn"},
    ]})
}

/** The visual editor generates canonical AgentScript the compiler accepts, and converts back */
#[test]
fn generated_agentscript_round_trips() {
    let text = agentscript(&payments_draft()).unwrap();
    assert_eq!(
        text,
        "policy payments_core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n    preserve --class Ledger scope file;\n    target --function PaymentService.refund change_type logical_bn;\n}\n"
    );
    let policy = parse(&text).unwrap();
    assert_eq!(policy.rules.len(), 3);
    let back = draft(&text).unwrap();
    assert_eq!(back["name"], "payments_core");
    assert_eq!(back["rules"][1]["scope"], "file");
    assert_eq!(back["rules"][2]["change_type"], "logical_bn");
    assert_eq!(
        agentscript(&back).unwrap(),
        text,
        "draft -> text -> draft -> text is stable"
    );
}

/** Invalid drafts report every problem row by row and generate nothing */
#[test]
fn invalid_drafts_are_explained() {
    let problems = agentscript(&json!({"name": "bad name", "rules": [
        {"rule": "protect", "kind": "function", "target": "A.b"},
        {"rule": "preserve", "kind": "method", "target": "A.b"},
        {"rule": "preserve", "kind": "function", "target": "A b"},
        {"rule": "preserve", "kind": "function", "target": "A.b", "scope": "galaxy"},
        {"rule": "preserve", "kind": "function", "target": "A.b", "change_type": "logical_bn"},
    ]}))
    .unwrap_err();
    assert_eq!(problems.len(), 6, "{problems:?}");
    assert!(problems[0].starts_with("name:"));
    assert!(problems[1].contains("rule 1: rule must be preserve or target"));
    assert!(problems[5].contains("only target rules take a change type"));
    assert!(
        agentscript(&json!({"name": "empty", "rules": []})).unwrap_err()[0]
            .contains("at least one rule")
    );
}

/** A session history from a synthetic journal
 * Input
    - id: &str - session id
    - qualified: &str - symbol the session changed
    - decision: &str - authorization decision
    - final_status: Option<&str> - finalization status
 * Output
    - History
*/
fn session(id: &str, qualified: &str, decision: &str, final_status: Option<&str>) -> History {
    let mut events = vec![
        json!({"seq": 1, "event": "pre_tool_use", "tool": "Edit", "operation": "write", "arguments_digest": "d1", "decision": decision, "resources": ["file:pay/PaymentService.java", format!("symbol:function:{qualified}")]}),
        json!({"seq": 2, "event": "post_tool_use", "tool": "Edit", "operation": "write", "arguments_digest": "d1", "effect": {"files": {"modified": ["pay/PaymentService.java"]}, "symbols": [{"symbol": format!("pay/PaymentService.java#{qualified}"), "change": "modified"}], "violations": []}}),
    ];
    if let Some(status) = final_status {
        events.push(json!({"seq": 3, "event": "session_finalized", "final_status": status}));
    }
    history(id, &events, false)
}

/** Shadow evaluation: newly blocked actions are classified by how their sessions ended, already
 * denied ones change nothing, executions are not double counted, and targets report the sessions
 * they would have failed */
#[test]
fn simulator_classifies_historical_impact() {
    let sessions = vec![
        session(
            "claude-pass",
            "PaymentService.refund",
            "allow",
            Some("PASS"),
        ),
        session(
            "claude-fail",
            "PaymentService.refund",
            "allow",
            Some("FAIL"),
        ),
        session(
            "claude-open",
            "PaymentService.refund",
            "approval_required",
            None,
        ),
        session(
            "claude-denied",
            "PaymentService.refund",
            "deny",
            Some("PASS"),
        ),
        session(
            "claude-other",
            "PaymentService.charge",
            "allow",
            Some("PASS"),
        ),
    ];
    let entities = vec![
        (
            "PaymentService.refund".to_string(),
            "function".to_string(),
            "pay/PaymentService.java".to_string(),
        ),
        (
            "PaymentService.charge".to_string(),
            "function".to_string(),
            "pay/PaymentService.java".to_string(),
        ),
    ];
    let policy = parse("policy shadow {\n    checkpoint baseline;\n    preserve --function PaymentService.refund;\n    target --function PaymentService.capture;\n}\n").unwrap();
    let result = simulate(&policy, &sessions, &entities);
    assert_eq!(result["enforced"], false);
    let preserve = &result["rules"][0];
    assert_eq!(preserve["resolution"], "resolved");
    assert_eq!(
        preserve["actions_changed"], 3,
        "pass, fail, and open; the denied one is unchanged"
    );
    assert_eq!(
        preserve["sessions_affected"],
        json!(["claude-fail", "claude-open", "claude-pass"])
    );
    let analysis = &result["false_positive_analysis"];
    assert_eq!(analysis["likely_true_positives"], 1);
    assert_eq!(analysis["likely_false_positives"], 1);
    assert_eq!(analysis["undetermined"], 1);
    assert_eq!(analysis["false_positive_rate"], 0.5);
    let shadows = result["impact"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            format!(
                "{}:{}",
                item["session"].as_str().unwrap(),
                item["shadow_decision"].as_str().unwrap()
            )
        })
        .collect::<Vec<_>>();
    assert!(shadows.contains(&"claude-denied:already_denied".to_string()));
    assert_eq!(
        result["impact"].as_array().unwrap().len(),
        4,
        "executions paired with an authorization are not counted twice"
    );
    let target = &result["rules"][1];
    assert_eq!(target["resolution"], "missing");
    assert_eq!(target["sessions_left_unmet"].as_array().unwrap().len(), 5);
}

/** File scope covers any change to the target's file, folder scope its folder */
#[test]
fn simulator_scopes() {
    let shell = History {
        id: "claude-shell".into(),
        outcome: "pass".into(),
        troubled: false,
        actions: vec![Action {
            seq: 4,
            event: "post_tool_use".into(),
            tool: "Bash".into(),
            operation: "execute".into(),
            digest: "x".into(),
            decision: None,
            files: vec!["pay/Fees.java".into()],
            symbols: vec!["Fees.compute".into()],
            violations: vec!["source_changed".into()],
        }],
    };
    let entities = vec![(
        "PaymentService.refund".to_string(),
        "function".to_string(),
        "pay/PaymentService.java".to_string(),
    )];
    let block = simulate(&parse("policy p {\n    checkpoint baseline;\n    preserve --function PaymentService.refund;\n}\n").unwrap(), std::slice::from_ref(&shell), &entities);
    assert_eq!(block["rules"][0]["actions_changed"], 0);
    let folder = simulate(&parse("policy p {\n    checkpoint baseline;\n    preserve --function PaymentService.refund scope folder;\n}\n").unwrap(), &[shell], &entities);
    assert_eq!(folder["rules"][0]["actions_changed"], 1);
    assert_eq!(folder["impact"][0]["shadow_decision"], "would_flag");
    assert_eq!(
        folder["impact"][0]["classification"], "likely_true_positive",
        "the action itself caused a violation"
    );
}

/** The control plane has its eight screens: the golden path (Flow), the six of the semantic
 * control plane, and Tasks */
#[test]
fn eight_screens() {
    let names = SCREENS.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "Flow",
            "Repository",
            "Tasks",
            "Zones",
            "Contracts",
            "Agent Sessions",
            "Policy Simulator",
            "Attestations"
        ]
    );
    let page = include_str!("app.html");
    for (name, endpoint) in SCREENS {
        assert!(page.contains(name), "{name} missing from the page");
        assert!(page.contains(endpoint), "{endpoint} not used by the page");
    }
    assert!(page.contains("Advanced: edit AgentScript"));
    assert!(
        !page.contains("innerHTML"),
        "the page never interprets data as HTML"
    );
}
