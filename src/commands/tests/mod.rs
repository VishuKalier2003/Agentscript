use crate::model::{ChangeType, ItemKind, Rule, Scope};
use crate::policy::parse;

/** Extract every function matching a target, used by the function tests below
 * Input
    - source: &str - file contents
    - path: &str - file path, used to pick the language
    - target: &str - qualified target
 * Output
    - Result<Vec<String>, String> canonical snippets
*/
fn extract_functions(source: &str, path: &str, target: &str) -> Result<Vec<String>, String> {
    crate::resolver::extract_items(source, path, ItemKind::Function, target)
}

/** Parse a policy containing one rule line and return that rule, used by the target tests below
 * Input
    - rule: &str - one statement including its ';'
 * Output
    - Result<Rule, String> the parsed rule or the parse error
*/
fn parse_rule(rule: &str) -> Result<Rule, String> {
    parse(&format!(
        "policy p {{\n checkpoint baseline;\n {rule}\n}}\n"
    ))
    .map(|policy| policy.rules.into_iter().next().expect("one rule"))
}

/** Test the happy paths of target parsing, by asserting the defaults (block scope, any change),
 * both options in either order, every change type, and case-insensitive change types
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn parses_target_rules() {
    assert!(matches!(
        parse_rule("target --function PaymentService.charge;").unwrap(),
        Rule::Target { kind: ItemKind::Function, target, scope: Scope::Block, change_type: None }
            if target == "PaymentService.charge"
    ));
    for rule in [
        "target --function A.b scope flow change_type logical_bn;",
        "target --function A.b change_type logical_bn scope flow;",
        "target --function A.b scope flow change_type Logical_BN;",
    ] {
        assert!(
            matches!(
                parse_rule(rule).unwrap(),
                Rule::Target {
                    scope: Scope::Flow,
                    change_type: Some(ChangeType::LogicalBn),
                    ..
                }
            ),
            "{rule}"
        );
    }
    for (value, expected) in [
        ("logical_bn", ChangeType::LogicalBn),
        ("logical_cn", ChangeType::LogicalCn),
        ("logical_sn", ChangeType::LogicalSn),
        ("semantic", ChangeType::Semantic),
        ("Semantic", ChangeType::Semantic),
    ] {
        let rule = parse_rule(&format!("target --function A.b change_type {value};")).unwrap();
        assert!(
            matches!(rule, Rule::Target { change_type: Some(parsed), .. } if parsed == expected),
            "{value}"
        );
    }
    let policy = parse(
        "policy mixed {\n checkpoint baseline;\n preserve --function A.keep;\n target --function A.fix scope file;\n}\n",
    )
    .unwrap();
    assert_eq!(policy.rules.len(), 2);
    assert_eq!(
        policy.rules[1].describe(),
        "target --function A.fix scope file"
    );
}

/** Test the sad paths of target parsing, by asserting the error for an invalid or misspelled
 * change type, missing values, duplicate or unknown options, change_type on preserve, and a
 * missing ';'
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn rejects_invalid_target_rules() {
    let cases = [
        (
            "target --function A.b change_type Loigcal_bn;",
            "invalid change_type 'Loigcal_bn'",
        ),
        (
            "target --function A.b change_type business;",
            "invalid change_type 'business'",
        ),
        (
            "target --function A.b change_type;",
            "change_type requires a value",
        ),
        ("target --function A.b scope;", "scope requires a value"),
        (
            "target --function A.b scope file scope flow;",
            "duplicate scope",
        ),
        (
            "target --function A.b change_type semantic change_type logical_bn;",
            "duplicate change_type",
        ),
        ("target --function A.b mode strict;", "unexpected 'mode'"),
        (
            "target --function A.b scope module;",
            "invalid scope 'module'",
        ),
        ("target --function A..b;", "invalid function target"),
        ("target --function A.b", "statement must end with ';'"),
        (
            "preserve --function A.b change_type semantic;",
            "unexpected 'change_type'",
        ),
    ];
    for (rule, expected) in cases {
        let error = parse_rule(rule).expect_err(rule);
        assert!(error.contains(expected), "{rule}\n=> {error}");
    }
}

/** Test that a valid policy parses, by parsing a policy with one checkpoint and one preserve rule
 * and asserting its name, checkpoint, rule target, and default block scope
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn parses_preserve_function_policy() {
    let policy = parse(
        "policy payments {\n\
         checkpoint baseline;\n\
         preserve --function GatewayService.call;\n\
         }\n",
    )
    .expect("valid policy");

    assert_eq!(policy.name, "payments");
    assert_eq!(policy.checkpoint, "baseline");
    assert!(matches!(
        &policy.rules[0],
        Rule::Preserve { kind: ItemKind::Function, target, scope: Scope::Block } if target == "GatewayService.call"
    ));
}

/** Test that every scope keyword parses, by parsing one preserve rule per scope value and
 * asserting the resulting scope
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn parses_every_scope() {
    for scope in [
        Scope::Block,
        Scope::File,
        Scope::Flow,
        Scope::Folder,
        Scope::All,
    ] {
        let policy = parse(&format!(
            "policy p {{\n checkpoint baseline;\n preserve --function A.b scope {};\n}}\n",
            scope.name()
        ))
        .expect("valid scope");
        assert!(matches!(
            &policy.rules[0],
            Rule::Preserve { scope: parsed, .. } if *parsed == scope
        ));
    }
}

/** Test the statement terminator rules, by asserting that statements without ';', a ';' after the
 * policy line or '}', a doubled ';', and bad scope values are all rejected
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn requires_semicolons_only_on_statements() {
    let cases = [
        (
            "policy p {\n checkpoint baseline\n preserve --function A.b;\n}\n",
            "line 2: statement must end with ';'",
        ),
        (
            "policy p {\n checkpoint baseline;\n preserve --function A.b\n}\n",
            "line 3: statement must end with ';'",
        ),
        (
            "policy p {\n checkpoint baseline;\n preserve --function A.b;\n};\n",
            "line 4: ';' is not allowed after '}'",
        ),
        (
            "policy p {;\n checkpoint baseline;\n preserve --function A.b;\n}\n",
            "line 1: policy must end with '{'",
        ),
        (
            "policy p {\n checkpoint baseline;\n preserve --function A.b;;\n}\n",
            "invalid function target",
        ),
        (
            "policy p {\n checkpoint baseline;\n preserve --function A.b scope module;\n}\n",
            "invalid scope 'module'",
        ),
        (
            "policy p {\n checkpoint baseline;\n preserve --function A.b scope;\n}\n",
            "scope requires a value",
        ),
    ];
    for (source, expected) in cases {
        let error = parse(source).expect_err(source);
        assert!(error.contains(expected), "{source}\n=> {error}");
    }
}

/** Test that a policy without a checkpoint is rejected, by parsing one and asserting the exact
 * error message
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn rejects_missing_checkpoint() {
    let error = parse("policy payments {\npreserve --function call;\n}\n")
        .expect_err("checkpoint is required");
    assert_eq!(error, "MVP requires an explicit checkpoint");
}

/** Test that source code containing policy-like text is not accepted as a policy, by parsing a
 * Rust snippet and asserting an error
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn rust_source_is_not_an_agentscript_policy() {
    let rust = "fn policy() { let text = \"preserve --function Gateway.call\"; }\n";
    assert!(crate::policy::parse(rust).is_err());
}

/** Test that Rust Type::method targets resolve, by extracting Gateway::call from an impl block and
 * asserting the canonical text contains its signature
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn resolves_rust_type_method_with_double_colons() {
    let rust = r#"
struct Gateway;
impl Gateway {
    fn call(&self) { println!("payment"); }
}
"#;
    let extracted = extract_functions(rust, "payment.rs", "Gateway::call")
        .expect("method")
        .pop()
        .expect("one method");
    assert!(extracted.contains("fn call(&self)"));
}

/** Extract the single canonical definition of a target, used by the comparison tests below
 * Input
    - source: &str - file contents
    - path: &str - file path, used to pick the language
    - target: &str - qualified target
 * Output
    - String canonical text (panics unless exactly one match)
*/
fn canonical(source: &str, path: &str, target: &str) -> String {
    let mut found = extract_functions(source, path, target).expect("parses");
    assert_eq!(found.len(), 1, "{path}");
    found.pop().unwrap()
}

/** Test that comments are ignored in every language using that language's own comment syntax, by
 * comparing each baseline with a copy that adds full-line, trailing, and inline comments
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn ignores_comments_in_each_language() {
    let cases = [
        (
            "class P {\n    int f() {\n        int a = 1 + 2;\n        return a;\n    }\n}\n",
            "class P {\n    /** doc */\n    int f() {\n        // line\n        int a = 1 /* inline */ + 2; // trailing\n        return a;\n    }\n}\n",
            "P.java",
            "P.f",
        ),
        (
            "class P {\n    f() {\n        const a = 1 + 2;\n        return a;\n    }\n}\n",
            "class P {\n    f() {\n        // line\n        /* block */ const a = 1 + 2; // trailing\n        return a;\n    }\n}\n",
            "p.js",
            "P.f",
        ),
        (
            "class P:\n    def f(self):\n        a = 1 + 2\n        return a\n",
            "class P:\n    def f(self):\n        # line\n        a = 1 + 2  # trailing\n\n        return a\n",
            "p.py",
            "P.f",
        ),
        (
            "struct P;\nimpl P {\n    fn f(&self) -> i32 {\n        let a = 1 + 2;\n        a\n    }\n}\n",
            "struct P;\nimpl P {\n    fn f(&self) -> i32 {\n        // line\n        let a = 1 /* inline */ + 2; // trailing\n        a\n    }\n}\n",
            "p.rs",
            "P::f",
        ),
    ];
    for (baseline, commented, path, target) in cases {
        assert_eq!(
            canonical(baseline, path, target),
            canonical(commented, path, target),
            "{path}"
        );
    }
}

/** Test that layout is significant, by asserting that moving a Python statement into a block and
 * re-indenting Java code both change the canonical text, while CRLF line endings do not
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn indentation_and_layout_are_significant() {
    let python = "class P:\n    def f(self, x):\n        if x:\n            a()\n        b()\n";
    let moved = "class P:\n    def f(self, x):\n        if x:\n            a()\n            b()\n";
    assert_ne!(
        canonical(python, "p.py", "P.f"),
        canonical(moved, "p.py", "P.f")
    );

    let java = "class P {\n    void f() {\n        g();\n    }\n}\n";
    let reindented = "class P {\n    void f() {\n    g();\n    }\n}\n";
    assert_ne!(
        canonical(java, "P.java", "P.f"),
        canonical(reindented, "P.java", "P.f")
    );

    assert_eq!(
        canonical(python, "p.py", "P.f"),
        canonical(&python.replace('\n', "\r\n"), "p.py", "P.f")
    );
}

/** Test that a Python docstring is protected code rather than a comment, by changing only the
 * docstring and asserting the canonical text differs
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn python_docstrings_are_not_comments() {
    let before = "class P:\n    def f(self):\n        \"\"\"old\"\"\"\n        return 1\n";
    let after = "class P:\n    def f(self):\n        \"\"\"new\"\"\"\n        return 1\n";
    assert_ne!(
        canonical(before, "p.py", "P.f"),
        canonical(after, "p.py", "P.f")
    );
}

/** Test that a nonexistent checkpoint commit is reported, by checking an all-zero SHA and asserting
 * the missing-or-invalid error
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn reports_missing_checkpoint_commit_without_fetching() {
    let error = crate::repository::ensure_commit("0000000000000000000000000000000000000000")
        .expect_err("missing commit");
    assert!(error.contains("missing or invalid"));
}

/** Test that every supported language resolves a method, by looping through Java, JavaScript,
 * Python, and Rust samples and asserting exactly one match each
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
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
        let result =
            extract_functions(source, path, target).expect("supported source should parse");
        assert_eq!(result.len(), 1, "{path}");
    }
}

/** Test that malformed source fails closed, by extracting from Java with a syntax error and
 * asserting a parser error
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn reports_parser_failure() {
    let error = extract_functions(
        "class Payment { void charge( { return; } }",
        "Payment.java",
        "Payment.charge",
    )
    .expect_err("malformed source must fail");
    assert!(error.contains("parser error"));
}

/** Extract the single item of a kind matching a target, used by the item-kind tests below
 * Input
    - source: &str - file contents
    - path: &str - file path, used to pick the language
    - kind: ItemKind - kind of item
    - target: &str - qualified target
 * Output
    - String canonical snippet (panics unless exactly one match)
*/
fn item(source: &str, path: &str, kind: ItemKind, target: &str) -> String {
    let mut found = crate::resolver::extract_items(source, path, kind, target).expect("parses");
    assert_eq!(found.len(), 1, "{path} {kind:?} {target}: {found:?}");
    found.pop().unwrap()
}

/** Test that every item kind resolves in every supported language, by extracting classes,
 * interfaces, variables, and data from Java, JavaScript, Python, and Rust samples and asserting
 * that a variable covers its whole declaration while data covers only the stored value
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn resolves_every_item_kind() {
    let java = "interface Payable { int pay(int amount); }\n\
                class PaymentService implements Payable {\n\
                    private static final int RATE = 5;\n\
                    public int pay(int amount) { return amount * RATE; }\n\
                }\n";
    assert!(
        item(java, "P.java", ItemKind::Class, "PaymentService").starts_with("class PaymentService")
    );
    assert!(item(java, "P.java", ItemKind::Interface, "Payable").starts_with("interface Payable"));
    assert_eq!(
        item(java, "P.java", ItemKind::Variable, "PaymentService.RATE"),
        "private static final int RATE = 5;"
    );
    assert_eq!(
        item(java, "P.java", ItemKind::Data, "PaymentService.RATE"),
        "5"
    );
    // Methods inside interfaces are now qualified by the interface name
    assert!(item(java, "P.java", ItemKind::Function, "Payable.pay").contains("int pay"));
    assert!(item(java, "P.java", ItemKind::Function, "PaymentService.pay").contains("RATE"));

    let js = "const LIMIT = 10;\nclass Cart { total() { return LIMIT; } }\n";
    assert_eq!(
        item(js, "c.js", ItemKind::Variable, "LIMIT"),
        "const LIMIT = 10;"
    );
    assert_eq!(item(js, "c.js", ItemKind::Data, "LIMIT"), "10");
    assert!(item(js, "c.js", ItemKind::Class, "Cart").starts_with("class Cart"));

    let python = "RATE = 5\nclass PaymentService:\n    fee = 2\n    def charge(self, amount):\n        return amount * RATE\n";
    assert_eq!(item(python, "p.py", ItemKind::Variable, "RATE"), "RATE = 5");
    assert_eq!(item(python, "p.py", ItemKind::Data, "RATE"), "5");
    assert_eq!(
        item(python, "p.py", ItemKind::Variable, "PaymentService.fee"),
        "fee = 2"
    );
    assert!(
        item(python, "p.py", ItemKind::Class, "PaymentService").starts_with("class PaymentService")
    );

    let rust = "const RATE: i32 = 5;\ntrait Payable { fn pay(&self) -> i32; }\n\
                struct Payment { amount: i32 }\n\
                impl Payable for Payment { fn pay(&self) -> i32 { self.amount * RATE } }\n";
    assert_eq!(
        item(rust, "p.rs", ItemKind::Variable, "RATE"),
        "const RATE: i32 = 5;"
    );
    assert_eq!(item(rust, "p.rs", ItemKind::Data, "RATE"), "5");
    assert!(item(rust, "p.rs", ItemKind::Interface, "Payable").starts_with("trait Payable"));
    assert!(item(rust, "p.rs", ItemKind::Class, "Payment").starts_with("struct Payment"));
    assert!(item(rust, "p.rs", ItemKind::Function, "Payable::pay").ends_with(';'));
    assert!(item(rust, "p.rs", ItemKind::Function, "Payment::pay").contains("RATE"));

    // Kinds do not leak into each other: a class is not a function, a variable is not a class
    let none = crate::resolver::extract_items(java, "P.java", ItemKind::Function, "PaymentService")
        .unwrap();
    assert!(none.is_empty());
    let none = crate::resolver::extract_items(js, "c.js", ItemKind::Interface, "Cart").unwrap();
    assert!(none.is_empty());
}

/** Test parsing of every item flag and the error for an unknown flag
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn parses_every_item_flag() {
    for kind in ItemKind::ALL {
        for keyword in ["preserve", "target"] {
            let rule = parse_rule(&format!("{keyword} {} A.b scope file;", kind.flag())).unwrap();
            let parsed = match rule {
                Rule::Preserve { kind, .. } | Rule::Target { kind, .. } => kind,
            };
            assert_eq!(parsed, kind, "{keyword} {}", kind.flag());
            assert_eq!(
                rule.describe(),
                format!("{keyword} {} A.b scope file", kind.flag())
            );
        }
    }
    for rule in ["preserve --method A.b;", "target A.b;", "preserve --class;"] {
        let error = parse_rule(rule).expect_err(rule);
        assert!(
            error.contains("unsupported item") || error.contains("missing class target"),
            "{rule} => {error}"
        );
    }
}

/** Test the SHA-256 digest used for contract versions against the standard test vectors
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn sha256_matches_standard_vectors() {
    use crate::util::sha256;
    assert_eq!(
        sha256(b""),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256(b"abc"),
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "sha256:248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

/** Test that preserve compiles to deny-write plus unchanged and target to permit-write plus
 * changed, and that a clause turns back into the same rule
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn rules_compile_to_runtime_and_postcondition() {
    use crate::ir::{Clause, Permission, Postcondition};
    let preserve = Clause::from_rule(&parse_rule("preserve --function A.b scope flow;").unwrap());
    assert_eq!(preserve.permission, Permission::DenyWrite);
    assert_eq!(preserve.postcondition, Postcondition::Unchanged);
    let target =
        Clause::from_rule(&parse_rule("target --class A change_type logical_sn;").unwrap());
    assert_eq!(target.permission, Permission::PermitWrite);
    assert_eq!(
        target.postcondition,
        Postcondition::Changed(Some(ChangeType::LogicalSn))
    );
    assert_eq!(
        target.rule().describe(),
        "target --class A scope block change_type logical_sn"
    );
    assert_eq!(preserve.scope, Scope::Flow);
}

/** Test that a contract set survives a session-file round trip and that edited contents or an
 * unknown IR format are rejected
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn contract_ir_round_trips_and_rejects_tampering() {
    use crate::ir::{Clause, Contract, ContractSet};
    let contract = Contract {
        policy_id: "payment".into(),
        version: crate::util::sha256(b"policy"),
        checkpoint: "baseline".into(),
        checkpoint_sha: Ok("abc123".into()),
        clauses: vec![Clause::from_rule(
            &parse_rule("preserve --function A.b;").unwrap(),
        )],
    };
    let set = ContractSet::new(vec![contract], Vec::new());
    let value = set.to_json();
    assert_eq!(ContractSet::from_json(&value).unwrap(), set);

    let mut tampered = value.clone();
    tampered["contracts"][0]["clauses"][0]["runtime"] = serde_json::json!("permit_write");
    assert!(ContractSet::from_json(&tampered).is_err());
    let mut rebased = value.clone();
    rebased["contracts"][0]["checkpoint_sha"] = serde_json::json!("def456");
    let error = ContractSet::from_json(&rebased).unwrap_err();
    assert!(error.contains("version does not match"), "{error}");
    let mut future = value;
    future["ir_format"] = serde_json::json!(2);
    assert!(ContractSet::from_json(&future).is_err());
}

/** Test that the agent adapters hold no policy semantics: their source never names clauses,
 * permissions, postconditions, scopes, change types, or verification of contracts
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn adapters_contain_no_policy_semantics() {
    let source = include_str!("../../adapter.rs");
    for word in [
        "Permission::",
        "crate::ir",
        "Postcondition",
        "Clause",
        "ContractSet",
        "Scope",
        "ChangeType",
        "ItemKind",
        "verify_contracts",
        "matches_target",
        "footprint",
        "Rule::",
        "preserve",
    ] {
        assert!(!source.contains(word), "adapter.rs mentions {word}");
    }
}

/** Test the metadata guard's path and command matching, including the hook subcommand that could
 * forge session events
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn metadata_guard_matches_paths_and_commands() {
    use crate::authority::{references_protected_path, runs_mutating_crane};
    assert!(references_protected_path(r"D:\Repo\.CRANE\x", &[]));
    assert!(!references_protected_path("src/my.crane", &[]));
    assert!(!references_protected_path(".craneignore", &[]));
    assert!(references_protected_path(
        ".claude/settings.local.json",
        &[".claude/settings.local.json"]
    ));
    assert!(!references_protected_path(
        ".claude/settings.local.json",
        &[]
    ));
    assert!(runs_mutating_crane("crane agent hook --event stop"));
    assert!(runs_mutating_crane("x && ./bin/crane.exe checkpoint"));
    assert!(!runs_mutating_crane("crane agent session show claude-1"));
    assert!(!runs_mutating_crane("crane check --agent"));
}

/** Test that Codex apply_patch text is parsed into the file changes it proposes: added, deleted,
 * and updated files (one edit per hunk, resolved against the cwd), moves, and malformed patches
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn parses_codex_apply_patch() {
    use crate::adapter::parse_patch;
    use crate::authority::{Proposed, TextEdit};
    let patch = "*** Begin Patch\n*** Add File: docs/new.md\n+hello\n+world\n*** Delete File: old.py\n*** Update File: pay.py\n@@ def charge():\n     total = 1\n-    return total\n+    return total * 2\n@@\n-x = 1\n+x = 2\n*** End of File\n*** End Patch\n";
    let changes = parse_patch(patch, None).unwrap();
    assert_eq!(changes.len(), 3);
    assert_eq!(changes[0].path, "docs/new.md");
    assert_eq!(
        changes[0].proposed,
        Proposed::Content("hello\nworld\n".into())
    );
    assert_eq!(changes[1].proposed, Proposed::Delete);
    assert_eq!(
        changes[2].proposed,
        Proposed::Edits(vec![
            TextEdit {
                old: "    total = 1\n    return total".into(),
                new: "    total = 1\n    return total * 2".into(),
                all: false,
            },
            TextEdit {
                old: "x = 1".into(),
                new: "x = 2".into(),
                all: false,
            },
        ])
    );

    let moved = parse_patch(
        "*** Begin Patch\n*** Update File: a.py\n*** Move to: b.py\n@@\n-x\n+y\n*** End Patch",
        Some("/repo"),
    )
    .unwrap();
    assert_eq!(moved[0].proposed, Proposed::Delete);
    assert_eq!(moved[1].proposed, Proposed::Unknown);
    assert!(moved[1].path.ends_with("b.py") && moved[1].path.starts_with("/repo"));

    let insertion = parse_patch(
        "*** Begin Patch\n*** Update File: a.py\n@@\n+new line\n*** End Patch",
        None,
    )
    .unwrap();
    assert_eq!(insertion[0].proposed, Proposed::Unknown);

    for malformed in [
        "",
        "*** Begin Patch\n*** End Patch",
        "*** Begin Patch\n*** Update File: a.py\n-x\n",
        "*** Begin Patch\n*** Update File: a.py\n?x\n*** End Patch",
        "diff --git a/x b/x",
    ] {
        assert!(parse_patch(malformed, None).is_none(), "{malformed:?}");
    }
}
