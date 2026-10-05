// Policy packs: domain knowledge that turns the semantic inventory into recommendations. There are
// two: Payments (payment processing, refunds, transaction logic, payment state transitions) and
// Testing (test directories, frameworks, ownership, contract-test configuration). A pack only
// recommends; the Payments pack can turn its recommendations into a pending proposal that a human
// must approve like any other, and nothing a pack does activates a policy or writes configuration.

#[cfg(test)] // Compile the module only when running tests, not in production builds
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use serde_json::{json, Value};

use crate::inventory::{discover, Options};
use crate::proposals::store::Proposal;
use crate::repository::root;

/** The packs, with their version and what they cover */
pub(crate) const PACKS: &[(&str, &str, &str)] = &[
    (
        "payments",
        "payments@1",
        "payment processing, refunds, transaction logic, and payment state transitions",
    ),
    (
        "testing",
        "testing@1",
        "test directories, test frameworks, test ownership, and contract-test configuration",
    ),
];

/** Words that mark payment code on their own, per category */
const STRONG: &[(&str, &[&str])] = &[
    (
        "payment_processing",
        &[
            "payment",
            "payments",
            "charge",
            "capture",
            "authorize",
            "authorise",
            "checkout",
            "settle",
            "settlement",
            "purchase",
        ],
    ),
    (
        "refunds",
        &["refund", "refunds", "chargeback", "reversal", "reverse"],
    ),
    (
        "transaction_logic",
        &[
            "transaction",
            "transactions",
            "ledger",
            "transfer",
            "debit",
            "payout",
        ],
    ),
];

/** Words that mark payment code only in a payment context, per category */
const CONTEXTUAL: &[(&str, &[&str])] = &[
    ("payment_processing", &["pay", "process", "fee", "amount"]),
    ("refunds", &["void", "cancel"]),
    (
        "transaction_logic",
        &["credit", "balance", "posting", "post", "commit"],
    ),
    (
        "payment_state_transitions",
        &[
            "status",
            "state",
            "transition",
            "mark",
            "advance",
            "complete",
            "fail",
            "succeed",
            "pending",
            "captured",
            "settled",
            "refunded",
        ],
    ),
];

/** Words that make a file, module, or type a payment context */
const CONTEXT: &[&str] = &[
    "payment",
    "payments",
    "pay",
    "billing",
    "checkout",
    "transaction",
    "transactions",
    "ledger",
    "refund",
    "invoice",
    "wallet",
    "charge",
];

/** Split an identifier or path into lowercase words (camelCase, snake_case, dots, slashes)
 * Input
    - text: &str - identifier or path
 * Output
    - Vec<String>
*/
pub(crate) fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for character in text.chars() {
        if !character.is_ascii_alphanumeric() {
            if !current.is_empty() {
                out.push(current.to_ascii_lowercase());
                current.clear();
            }
            previous_lower = false;
            continue;
        }
        if character.is_ascii_uppercase() && previous_lower && !current.is_empty() {
            out.push(current.to_ascii_lowercase());
            current.clear();
        }
        previous_lower = character.is_ascii_lowercase() || character.is_ascii_digit();
        current.push(character);
    }
    if !current.is_empty() {
        out.push(current.to_ascii_lowercase());
    }
    out
}

/** Classify a symbol for the Payments pack
 * Input
    - name: &str - symbol name
    - qualified: &str - qualified name
    - file: &str - file path
 * Output
    - Option<(&'static str, &'static str, String)> category, confidence, and reason
*/
pub(crate) fn payment_category(
    name: &str,
    qualified: &str,
    file: &str,
) -> Option<(&'static str, &'static str, String)> {
    let own = words(name);
    // The context is where the symbol lives (its owner and file), never its own name
    let owner = qualified.rsplit_once('.').map_or("", |(owner, _)| owner);
    let context = words(&format!("{owner} {file}"));
    let in_context = context.iter().any(|word| CONTEXT.contains(&word.as_str()));
    for (category, vocabulary) in STRONG {
        if let Some(word) = own.iter().find(|word| vocabulary.contains(&word.as_str())) {
            let confidence = if in_context { "high" } else { "medium" };
            return Some((
                category,
                confidence,
                format!(
                    "'{word}' in its name{}",
                    if in_context { " in payment code" } else { "" }
                ),
            ));
        }
    }
    if in_context {
        for (category, vocabulary) in CONTEXTUAL {
            if let Some(word) = own.iter().find(|word| vocabulary.contains(&word.as_str())) {
                return Some((
                    category,
                    "medium",
                    format!("'{word}' in its name in payment code"),
                ));
            }
        }
    }
    None
}

/** Build the Payments pack's recommendations from the inventory: one preserve rule per payment
 * function not yet covered by a contract (whole types only for payment state, since preserving a
 * whole service would block legitimate work on it), a critical zone for each payment module, and
 * the policy the uncovered recommendations would make; nothing in test files is payment code
 * Input
    - inventory: &Value - inventory JSON
    - checkpoint: &str - checkpoint for the generated policy
 * Output
    - Value
*/
pub(crate) fn payments(inventory: &Value, checkpoint: &str) -> Value {
    let mut recommendations = Vec::new();
    let mut modules: BTreeMap<String, usize> = BTreeMap::new();
    let test_files = inventory["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|file| file["test"] == true)
        .filter_map(|file| file["path"].as_str())
        .collect::<BTreeSet<_>>();
    for entity in inventory["entities"].as_array().into_iter().flatten() {
        if entity["test"] == true
            || entity["file"]
                .as_str()
                .is_some_and(|file| test_files.contains(file))
        {
            continue;
        }
        let Some(target) = entity["policy_target"].as_str() else {
            continue;
        };
        let text = |key: &str| entity[key].as_str().unwrap_or_default();
        let Some((category, confidence, reason)) =
            payment_category(text("name"), text("qualified"), text("file"))
        else {
            continue;
        };
        let callable = target.starts_with("--function ");
        let state_type = category == "payment_state_transitions"
            || words(text("name"))
                .iter()
                .any(|word| word == "status" || word == "state");
        if !callable && !state_type {
            continue;
        }
        *modules.entry(text("module").to_string()).or_insert(0) += 1;
        let covered = entity["contracts"]
            .as_array()
            .is_some_and(|contracts| !contracts.is_empty());
        recommendations.push(json!({
            "id": format!("payments:{}", text("id")),
            "category": category,
            "entity": text("id"),
            "file": text("file"),
            "confidence": confidence,
            "reason": reason,
            "status": if covered { "covered" } else { "new" },
            "covered_by": entity["contracts"],
            "suggestion": {"agentscript": format!("preserve {target};")},
        }));
    }
    let rules = recommendations
        .iter()
        .filter(|recommendation| recommendation["status"] == "new")
        .filter_map(|recommendation| recommendation["suggestion"]["agentscript"].as_str())
        .collect::<BTreeSet<_>>();
    let policy = (!rules.is_empty()).then(|| {
        format!(
            "policy payments_pack {{\n    checkpoint {checkpoint};\n{}\n}}\n",
            rules
                .iter()
                .map(|rule| format!("    {rule}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    });
    let zones = modules
        .iter()
        .filter(|(module, _)| !module.is_empty())
        .map(|(module, count)| {
            let name = module.rsplit(['.', ':', '/']).next().unwrap_or("payments");
            json!({
                "id": format!("payments:zone:{module}"),
                "category": "zone",
                "module": module,
                "symbols": count,
                "status": "new",
                "suggestion": {"zone": format!("zone {}_payments {{\n    criticality critical;\n    autonomy assisted;\n    select module {module};\n}}\n", name.to_ascii_lowercase().replace('-', "_"))},
            })
        })
        .collect::<Vec<_>>();
    let count = |category: &str| {
        recommendations
            .iter()
            .filter(|recommendation| recommendation["category"] == category)
            .count()
    };
    json!({
        "pack": "payments",
        "version": "payments@1",
        "activates_nothing": true,
        "summary": {
            "payment_processing": count("payment_processing"),
            "refunds": count("refunds"),
            "transaction_logic": count("transaction_logic"),
            "payment_state_transitions": count("payment_state_transitions"),
            "new": recommendations.iter().filter(|recommendation| recommendation["status"] == "new").count(),
            "covered": recommendations.iter().filter(|recommendation| recommendation["status"] == "covered").count(),
        },
        "recommendations": recommendations,
        "zone_recommendations": zones,
        "proposed_policy": policy,
    })
}

/** What the Testing pack learns about one test directory: test files, languages, owners, and
 * frameworks */
type TestDirectory = (
    usize,
    BTreeSet<String>,
    BTreeSet<String>,
    BTreeSet<&'static str>,
);

/** Detect the test frameworks a test file uses from its content and name
 * Input
    - path: &str - test file path
    - content: &str - file content
 * Output
    - Vec<&'static str>
*/
pub(crate) fn frameworks_in(path: &str, content: &str) -> Vec<&'static str> {
    let mut found = Vec::new();
    let has = |needle: &str| content.contains(needle);
    if path.ends_with(".py") {
        if has("import pytest")
            || has("from pytest")
            || path.ends_with("conftest.py")
            || content.contains("def test_") && !has("unittest")
        {
            found.push("pytest");
        }
        if has("import unittest") || has("from unittest") {
            found.push("unittest");
        }
    }
    if path.ends_with(".java") || path.ends_with(".kt") {
        if has("org.junit.jupiter") {
            found.push("junit5");
        } else if has("org.junit") {
            found.push("junit4");
        }
        if has("org.testng") {
            found.push("testng");
        }
    }
    if [".js", ".ts", ".jsx", ".tsx", ".mjs"]
        .iter()
        .any(|extension| path.ends_with(extension))
    {
        if has("from 'vitest'") || has("from \"vitest\"") {
            found.push("vitest");
        } else if has("describe(") || has("test(") || has("it(") {
            found.push("jest");
        }
    }
    if path.ends_with("_test.go") && has("\"testing\"") {
        found.push("go-test");
    }
    if path.ends_with(".rs") && has("#[test]") {
        found.push("cargo-test");
    }
    found
}

/** The contract-test command Crane would run for a framework, and the testing.json key it goes
 * under (commands run per file with {files}; suites run whole)
 * Input
    - framework: &str - framework
 * Output
    - Option<(&'static str, &'static str, Vec<&'static str>)> section, language, command
*/
pub(crate) fn command_for(
    framework: &str,
) -> Option<(&'static str, &'static str, Vec<&'static str>)> {
    Some(match framework {
        "pytest" => (
            "commands",
            "python",
            vec!["python", "-m", "pytest", "-q", "{files}"],
        ),
        "unittest" => (
            "commands",
            "python",
            vec!["python", "-m", "unittest", "{files}"],
        ),
        "junit5" | "junit4" | "testng" => ("suite", "java", vec!["mvn", "-q", "test"]),
        "jest" => ("commands", "javascript", vec!["npx", "jest", "{files}"]),
        "vitest" => (
            "commands",
            "javascript",
            vec!["npx", "vitest", "run", "{files}"],
        ),
        "go-test" => ("suite", "go", vec!["go", "test", "./..."]),
        "cargo-test" => ("suite", "rust", vec!["cargo", "test"]),
        _ => return None,
    })
}

/** The folder a test file belongs to: its nearest test-named folder, else its own folder
 * Input
    - path: &str - test file path
 * Output
    - String
*/
fn test_directory(path: &str) -> String {
    let parts = path.split('/').collect::<Vec<_>>();
    let folders = &parts[..parts.len().saturating_sub(1)];
    match folders
        .iter()
        .rposition(|folder| matches!(*folder, "test" | "tests" | "__tests__" | "spec" | "testing"))
    {
        Some(index) => folders[..=index].join("/"),
        None if folders.is_empty() => ".".into(),
        None => folders.join("/"),
    }
}

/** Build the Testing pack's recommendations: test directories with their owners and frameworks,
 * ownership gaps, and the contract-test configuration Crane needs to run them
 * Input
    - inventory: &Value - inventory JSON
    - repository: &std::path::Path - repository root (to read test files)
    - testing: Option<Value> - the current .crane/testing.json
 * Output
    - Value
*/
pub(crate) fn testing(
    inventory: &Value,
    repository: &std::path::Path,
    testing: Option<Value>,
) -> Value {
    let mut directories: BTreeMap<String, TestDirectory> = BTreeMap::new();
    for file in inventory["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|file| file["test"] == true)
    {
        let path = file["path"].as_str().unwrap_or_default();
        let entry = directories.entry(test_directory(path)).or_default();
        entry.0 += 1;
        if let Some(language) = file["language"].as_str() {
            entry.1.insert(language.to_string());
        }
        entry.2.extend(
            file["owners"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(String::from),
        );
        let content = fs::read_to_string(repository.join(path)).unwrap_or_default();
        entry.3.extend(frameworks_in(path, &content));
    }
    let mut recommendations = Vec::new();
    let mut frameworks = BTreeSet::new();
    for (directory, (files, languages, owners, found)) in &directories {
        frameworks.extend(found.iter().copied());
        recommendations.push(json!({
            "id": format!("testing:directory:{directory}"),
            "category": "test_directory",
            "directory": directory,
            "files": files,
            "languages": languages,
            "frameworks": found,
            "owners": owners,
            "status": "detected",
            "suggestion": {"zone": "zone tests {\n    criticality routine;\n    autonomy delegated;\n    select tests;\n}\n"},
        }));
        if owners.is_empty() {
            recommendations.push(json!({
                "id": format!("testing:ownership:{directory}"),
                "category": "test_ownership",
                "directory": directory,
                "status": "new",
                "reason": "no CODEOWNERS rule covers these tests, so nobody is asked to review agent changes to them",
                "suggestion": {"codeowners": format!("/{}/ @OWNER", directory.trim_start_matches("./"))},
            }));
        }
    }
    let configured = testing.clone().unwrap_or(json!({}));
    let mut suggested = json!({"timeout_seconds": 600, "commands": {}, "suite": {}});
    for framework in &frameworks {
        let Some((section, language, command)) = command_for(framework) else {
            continue;
        };
        let present = !configured[section][language].is_null()
            || !configured[if section == "suite" {
                "commands"
            } else {
                "suite"
            }][language]
                .is_null();
        suggested[section][language] = json!(command);
        recommendations.push(json!({
            "id": format!("testing:contract_tests:{framework}"),
            "category": "contract_test_configuration",
            "framework": framework,
            "language": language,
            "status": if present { "configured" } else { "new" },
            "reason": if present { format!("{language} tests already run from .crane/testing.json") } else { format!("Crane cannot run the {framework} tests during sessions and delivery until .crane/testing.json has a {language} command") },
            "suggestion": {"testing_json": {section: {language: command}}},
        }));
    }
    let count = |category: &str| {
        recommendations
            .iter()
            .filter(|recommendation| recommendation["category"] == category)
            .count()
    };
    json!({
        "pack": "testing",
        "version": "testing@1",
        "activates_nothing": true,
        "summary": {
            "test_directories": count("test_directory"),
            "test_files": directories.values().map(|entry| entry.0).sum::<usize>(),
            "frameworks": frameworks,
            "ownership_gaps": count("test_ownership"),
            "contract_test_configuration": if testing.is_some() { "present" } else { "missing" },
        },
        "recommendations": recommendations,
        "suggested_testing_json": suggested,
    })
}

/** Run a pack against the current repository
 * Input
    - pack: &str - payments or testing
    - checkpoint: &str - checkpoint for generated policies
 * Output
    - Result<Value, String>
*/
pub(crate) fn run(pack: &str, checkpoint: &str) -> Result<Value, String> {
    let inventory = discover(&Options { full: false })?;
    let json = inventory.to_json();
    match pack {
        "payments" => Ok(payments(&json, checkpoint)),
        "testing" => {
            let configured = fs::read_to_string(root()?.join("testing.json"))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok());
            Ok(testing(&json, &inventory.root, configured))
        }
        other => Err(format!(
            "unknown policy pack '{other}'; the packs are payments and testing"
        )),
    }
}

/** List the packs
 * Input
    - None
 * Output
    - Value
*/
pub(crate) fn list() -> Value {
    json!(PACKS
        .iter()
        .map(|(name, version, covers)| json!({"pack": name, "version": version, "covers": covers}))
        .collect::<Vec<_>>())
}

/** Turn the Payments pack's uncovered recommendations into a pending proposal: it is never
 * activated here; a human reviews and approves it like any other proposal
 * Input
    - name: &str - proposal and policy name
    - checkpoint: &str - checkpoint
 * Output
    - Result<Proposal, String>
*/
pub(crate) fn propose(name: &str, checkpoint: &str) -> Result<Proposal, String> {
    Proposal::ensure_free(name)?;
    let result = run("payments", checkpoint)?;
    let text = result["proposed_policy"]
        .as_str()
        .ok_or("the payments pack has no uncovered recommendation to propose")?
        .replacen("policy payments_pack {", &format!("policy {name} {{"), 1);
    crate::policy::parse(&text).map_err(|error| format!("generated policy is invalid: {error}"))?;
    let listed = result["recommendations"].clone();
    let zones = result["zone_recommendations"].clone();
    Proposal::create(
        name,
        checkpoint,
        json!({"kind": "pack", "pack": "payments", "version": "payments@1"}),
        (listed, text, zones, json!({"pack": "payments@1"})),
        "crane packs propose payments",
    )
}
