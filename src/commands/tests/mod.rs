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
    let extracted =
        crate::resolver::extract_function(rust, "payment.rs", "Gateway::call").expect("method");
    assert!(extracted.contains("fn call"));
}

#[test]
fn reports_missing_checkpoint_commit_without_fetching() {
    let error = crate::repository::ensure_commit("0000000000000000000000000000000000000000")
        .expect_err("missing commit");
    assert!(error.contains("missing or invalid"));
}
