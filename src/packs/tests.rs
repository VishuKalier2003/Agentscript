use serde_json::json;

use super::*;

/** Identifiers split into words across camelCase, snake_case, and paths */
#[test]
fn identifiers_split_into_words() {
    assert_eq!(
        words("PaymentService.chargeCard"),
        ["payment", "service", "charge", "card"]
    );
    assert_eq!(words("process_refund"), ["process", "refund"]);
    assert_eq!(
        words("src/billing/HTTPLedger.java"),
        ["src", "billing", "httpledger", "java"]
    );
}

/** The Payments pack finds each category, needs payment context for generic words, and ignores
 * unrelated code */
#[test]
fn payment_categories() {
    let category = |name: &str, qualified: &str, file: &str| {
        payment_category(name, qualified, file)
            .map(|(category, confidence, _)| (category, confidence))
    };
    assert_eq!(
        category("charge", "PaymentService.charge", "pay/PaymentService.java"),
        Some(("payment_processing", "high"))
    );
    assert_eq!(
        category("refundOrder", "Orders.refundOrder", "orders/Orders.java"),
        Some(("refunds", "medium"))
    );
    assert_eq!(
        category(
            "postEntry",
            "LedgerWriter.postEntry",
            "ledger/LedgerWriter.java"
        ),
        Some(("transaction_logic", "medium"))
    );
    assert_eq!(
        category(
            "markCaptured",
            "PaymentStateMachine.markCaptured",
            "payments/state.py"
        ),
        Some(("payment_state_transitions", "medium"))
    );
    assert_eq!(
        category("markRead", "Inbox.markRead", "mail/inbox.py"),
        None,
        "generic words need payment context"
    );
    assert_eq!(category("render", "Page.render", "web/page.ts"), None);
}

/** Recommendations come from the inventory, covered symbols are marked, and the proposed policy
 * holds only uncovered rules */
#[test]
fn payments_recommendations() {
    let inventory = json!({"entities": [
        {"id": "java:pay.PaymentService.charge", "name": "charge", "qualified": "PaymentService.charge", "file": "pay/PaymentService.java", "module": "java:pay", "test": false, "contracts": [], "policy_target": "--function PaymentService.charge"},
        {"id": "java:pay.PaymentService.refund", "name": "refund", "qualified": "PaymentService.refund", "file": "pay/PaymentService.java", "module": "java:pay", "test": false, "contracts": ["core:preserve"], "policy_target": "--function PaymentService.refund"},
        {"id": "java:pay.PaymentServiceTest.charge", "name": "charge", "qualified": "PaymentServiceTest.charge", "file": "pay/PaymentServiceTest.java", "module": "java:pay", "test": true, "contracts": [], "policy_target": "--function PaymentServiceTest.charge"},
        {"id": "java:web.Page.render", "name": "render", "qualified": "Page.render", "file": "web/Page.java", "module": "java:web", "test": false, "contracts": [], "policy_target": "--function Page.render"},
    ]});
    let result = payments(&inventory, "baseline");
    assert_eq!(result["activates_nothing"], true);
    assert_eq!(result["recommendations"].as_array().unwrap().len(), 2);
    assert_eq!(result["summary"]["new"], 1);
    assert_eq!(result["summary"]["covered"], 1);
    assert_eq!(result["proposed_policy"], "policy payments_pack {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n");
    assert!(result["zone_recommendations"][0]["suggestion"]["zone"]
        .as_str()
        .unwrap()
        .contains("criticality critical;"));
}

/** Whole services are never recommended (that would block legitimate work on them), payment
 * state types are, and nothing in a test file counts as payment code */
#[test]
fn payments_recommendations_stay_narrow() {
    let inventory = json!({
        "files": [{"path": "pay/src/test/PaymentServiceTest.java", "test": true}],
        "entities": [
            {"id": "java:pay.PaymentService", "name": "PaymentService", "qualified": "PaymentService", "file": "pay/PaymentService.java", "module": "java:pay", "test": false, "contracts": [], "policy_target": "--class PaymentService"},
            {"id": "java:pay.PaymentStatus", "name": "PaymentStatus", "qualified": "PaymentStatus", "file": "pay/PaymentStatus.java", "module": "java:pay", "test": false, "contracts": [], "policy_target": "--class PaymentStatus"},
            {"id": "java:pay.PaymentServiceTest", "name": "PaymentServiceTest", "qualified": "PaymentServiceTest", "file": "pay/src/test/PaymentServiceTest.java", "module": "java:pay", "test": false, "contracts": [], "policy_target": "--class PaymentServiceTest"},
        ],
    });
    let result = payments(&inventory, "baseline");
    let entities = result["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["entity"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(entities, ["java:pay.PaymentStatus"]);
}

/** Frameworks are detected from test files, and each maps to the command Crane would run */
#[test]
fn test_frameworks() {
    assert_eq!(
        frameworks_in("tests/test_pay.py", "import pytest\n"),
        ["pytest"]
    );
    assert_eq!(
        frameworks_in("tests/test_pay.py", "import unittest\n"),
        ["unittest"]
    );
    assert_eq!(
        frameworks_in(
            "src/test/java/PayTest.java",
            "import org.junit.jupiter.api.Test;"
        ),
        ["junit5"]
    );
    assert_eq!(
        frameworks_in("web/pay.test.ts", "import { it } from 'vitest'"),
        ["vitest"]
    );
    assert_eq!(
        frameworks_in("pay/pay_test.go", "import \"testing\""),
        ["go-test"]
    );
    assert_eq!(
        command_for("pytest").unwrap().2,
        ["python", "-m", "pytest", "-q", "{files}"]
    );
    assert_eq!(command_for("junit5").unwrap().0, "suite");
}
