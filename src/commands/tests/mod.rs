use crate::model::Rule;
use crate::policy::parse;

#[test]
fn parses_preserve_function_policy() {
    let policy = parse(
        "policy payments {\n\
         checkpoint baseline\n\
         preserve --function GatewayService.call\n\
         }\n",
    )
    .expect("valid policy");

    assert_eq!(policy.name, "payments");
    assert_eq!(policy.checkpoint, "baseline");
    assert!(matches!(
        &policy.rules[0],
        Rule::PreserveFunction { target } if target == "GatewayService.call"
    ));
}

#[test]
fn rejects_missing_checkpoint() {
    let error = parse("policy payments {\npreserve --function call\n}\n")
        .expect_err("checkpoint is required");
    assert_eq!(error, "MVP requires an explicit checkpoint");
}

#[test]
fn rust_source_is_not_an_agentscript_policy() {
    let rust = "fn policy() { let text = \"preserve --function Gateway.call\"; }\n";
    assert!(crate::policy::parse(rust).is_err());
}

#[test]
fn resolves_rust_type_method_with_double_colons() {
    let rust = r#"
struct Gateway;
impl Gateway {
    fn call(&self) { println!("payment"); }
}
"#;
    let extracted = crate::resolver::extract_functions(rust, "payment.rs", "Gateway::call")
        .expect("method")
        .pop()
        .expect("one method");
    assert!(extracted.contains("fn\0call"));
}

#[test]
fn reports_missing_checkpoint_commit_without_fetching() {
    let error = crate::repository::ensure_commit("0000000000000000000000000000000000000000")
        .expect_err("missing commit");
    assert!(error.contains("missing or invalid"));
}

#[test]
fn resolves_supported_source_languages() {
    let cases = [
        (
            "class Payment { void charge() { return; } }",
            "Payment.java",
            "Payment.charge",
        ),
        (
            "class Payment { charge() { return 1; } }",
            "payment.js",
            "Payment.charge",
        ),
        (
            "class PaymentService:\n    def charge(self):\n        return 1\n",
            "payment.py",
            "PaymentService.charge",
        ),
        (
            "struct Payment;\nimpl Payment { fn charge(&self) {} }",
            "payment.rs",
            "Payment::charge",
        ),
    ];
    for (source, path, target) in cases {
        let result = crate::resolver::extract_functions(source, path, target)
            .expect("supported source should parse");
        assert_eq!(result.len(), 1, "{path}");
    }
}

#[test]
fn reports_parser_failure() {
    let error = crate::resolver::extract_functions(
        "class Payment { void charge( { return; } }",
        "Payment.java",
        "Payment.charge",
    )
    .expect_err("malformed source must fail");
    assert!(error.contains("parser error"));
}
