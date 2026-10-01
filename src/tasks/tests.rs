use serde_json::json;

use super::{code_tokens, mentions, TaskInput};

/** Test that only code-like words become references: backticked spans, qualified names, paths,
 * snake_case, and camelCase; plain words, numbers, and abbreviations never do
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn only_code_like_words_are_references() {
    assert_eq!(
        code_tokens("Make `refund` reject amounts, see PaymentService.refund() and src/pay/x.py."),
        ["refund", "PaymentService.refund", "src/pay/x.py"]
    );
    assert_eq!(
        code_tokens("call compute_tax and chargeCustomer, e.g. Ledger::post"),
        ["compute_tax", "chargeCustomer", "Ledger::post"]
    );
    assert!(
        code_tokens("Make payments faster and authenticate users. Version 1.5 i.e. better")
            .is_empty()
    );
    assert!(code_tokens("Use `a b` and `` here").is_empty());
}

/** Test that negation applies per clause: "without changing X", "X must not change", and "do not
 * touch X" protect X, while "X must not accept negative amounts" still asks to change X
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn negation_applies_to_its_own_clause() {
    let task = TaskInput::from_json(&json!({
        "task_id": "PAY-1",
        "title": "Reject negative refunds",
        "description": "Make `PaymentService.refund` reject negative amounts without changing `PaymentService.charge`. `Ledger.post` must not change, but update `Ledger.audit`; do not touch `Fees.rate`.",
        "acceptance_criteria": ["`PaymentService.refund` must not accept negative amounts"],
        "references": {"symbols": ["Explicit.one"], "must_not_change": ["Explicit.two"]},
    }))
    .unwrap();
    let found = mentions(&task)
        .into_iter()
        .map(|mention| (mention.token, mention.negated, mention.explicit))
        .collect::<Vec<_>>();
    let expect = |token: &str, negated: bool, explicit: bool| {
        assert!(
            found.contains(&(token.to_string(), negated, explicit)),
            "{token}: {found:?}"
        );
    };
    expect("Explicit.one", false, true);
    expect("Explicit.two", true, true);
    expect("PaymentService.refund", false, false);
    expect("PaymentService.charge", true, false);
    expect("Ledger.post", true, false);
    expect("Ledger.audit", false, false);
    expect("Fees.rate", true, false);
    assert!(!found
        .iter()
        .any(|(token, negated, _)| token == "PaymentService.refund" && *negated));
}

/** Test task normalization: optional fields, single strings for lists, and type errors
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn task_input_is_normalized() {
    let task = TaskInput::from_json(&json!({
        "task_id": "BILL-7",
        "title": "  Round totals ",
        "acceptance_criteria": "rounds to cents",
        "labels": ["billing"],
    }))
    .unwrap();
    assert_eq!(task.title, "Round totals");
    assert_eq!(task.acceptance_criteria, ["rounds to cents"]);
    assert!(task.description.is_empty() && task.repositories.is_empty());
    assert_eq!(task.to_json()["references"]["symbols"], json!([]));
    for (bad, message) in [
        (json!({"title": 3}), "'title' must be text"),
        (json!({"labels": [1]}), "'labels' must hold text"),
        (json!({"references": []}), "'references' must be an object"),
        (json!({"task_format": 2}), "unsupported task_format"),
        (json!([]), "must be a JSON object"),
    ] {
        let error = TaskInput::from_json(&bad).err().unwrap();
        assert!(error.contains(message), "{bad}: {error}");
    }
}

/** Test that the documented example task is a valid, fully specified task
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn example_task_is_valid() {
    let value: serde_json::Value =
        serde_json::from_str(include_str!("../../examples/tasks/PAY-1821.json")).unwrap();
    let task = TaskInput::from_json(&value).unwrap();
    assert_eq!(task.task_id, "PAY-1821");
    assert_eq!(task.acceptance_criteria.len(), 2);
    let tokens = mentions(&task)
        .into_iter()
        .map(|mention| (mention.token, mention.negated))
        .collect::<Vec<_>>();
    assert!(tokens.contains(&("PaymentService.refund".to_string(), false)));
    assert!(tokens.contains(&("PaymentService.charge".to_string(), true)));
}
