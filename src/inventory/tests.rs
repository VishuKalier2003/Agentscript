use std::sync::Arc;

use super::governance::{codeowners_match, words};
use super::graph::{is_test_path, module_name};
use super::index::SourceFile;
use super::outline::{language_of, outline, Outline, Symbol};

/** Extract the outline of a fixture file, choosing its language from the path
 * Input
    - path: &str - file path
    - source: &str - file content
 * Output
    - Outline
*/
fn extract(path: &str, source: &str) -> Outline {
    outline(
        source.as_bytes(),
        path,
        language_of(path).expect("known language"),
    )
}

/** List an outline's symbols as "kind qualified" strings in source order
 * Input
    - outline: &Outline - outline
 * Output
    - Vec<String>
*/
fn listing(outline: &Outline) -> Vec<String> {
    outline
        .symbols
        .iter()
        .map(|symbol| format!("{} {}", symbol.kind.name(), symbol.qualified))
        .collect()
}

/** Find a symbol by qualified name, panicking with the listing when it is missing
 * Input
    - outline: &'a Outline - outline
    - qualified: &str - qualified name
 * Output
    - &'a Symbol
*/
fn symbol<'a>(outline: &'a Outline, qualified: &str) -> &'a Symbol {
    outline
        .symbols
        .iter()
        .find(|symbol| symbol.qualified == qualified)
        .unwrap_or_else(|| panic!("{qualified} missing from {:?}", listing(outline)))
}

/** Test Java extraction: package, imports, classes, interfaces, enums, fields and constants,
 * methods with calls and complexity, @Test methods, and that only resolver-matched items are
 * targetable
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn java_outline() {
    let outline = extract(
        "src/main/java/com/acme/payments/PaymentService.java",
        "package com.acme.payments;\n\nimport java.util.List;\n\npublic class PaymentService {\n    private static final int MAX_RETRIES = 3;\n    private int rate = 2;\n\n    public int charge(int amount) {\n        if (amount > 100) { return fee(amount); }\n        return validate(amount) + rate;\n    }\n\n    int fee(int amount) { return amount / 10; }\n\n    int validate(int amount) { for (int i = 0; i < 3; i++) {} return 1; }\n\n    @Test\n    void chargesCustomers() { charge(5); }\n}\n\ninterface Gateway { void send(); }\n\nenum Currency { USD }\n",
    );
    assert_eq!(outline.status, "parsed");
    assert_eq!(outline.package.as_deref(), Some("com.acme.payments"));
    assert_eq!(outline.imports, ["java.util.List"]);
    assert_eq!(
        listing(&outline),
        [
            "class PaymentService",
            "data PaymentService.MAX_RETRIES",
            "variable PaymentService.rate",
            "method PaymentService.charge",
            "method PaymentService.fee",
            "method PaymentService.validate",
            "method PaymentService.chargesCustomers",
            "interface Gateway",
            "method Gateway.send",
            "enum Currency",
        ]
    );
    let charge = symbol(&outline, "PaymentService.charge");
    assert_eq!(charge.calls, ["fee", "validate"]);
    assert_eq!(
        (charge.branches, charge.start_line, charge.end_line),
        (1, 9, 12)
    );
    assert!(charge.targetable);
    assert_eq!(symbol(&outline, "PaymentService.validate").loops, 1);
    assert!(symbol(&outline, "PaymentService.chargesCustomers").test);
    assert!(!charge.test);
    assert!(!symbol(&outline, "Currency").targetable);
}

/** Test Python extraction: imports, module constants, classes, class attributes, methods with
 * calls, local variables left out, and test_ functions
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn python_outline() {
    let outline = extract(
        "analytics/report.py",
        "import os\nfrom billing.tax import compute_tax\n\nRATE = 3\n\nclass Report:\n    limit = 10\n\n    def build(self, rows):\n        total = 0\n        for row in rows:\n            total += compute_tax(row)\n        return self.render(total)\n\n    def render(self, total):\n        return str(total)\n\ndef test_build():\n    Report().build([1])\n",
    );
    assert_eq!(outline.imports, ["os", "billing.tax"]);
    assert_eq!(
        listing(&outline),
        [
            "data RATE",
            "class Report",
            "variable Report.limit",
            "method Report.build",
            "method Report.render",
            "function test_build",
        ]
    );
    let build = symbol(&outline, "Report.build");
    assert_eq!(build.calls, ["compute_tax", "render"]);
    assert_eq!(build.loops, 1);
    assert!(symbol(&outline, "test_build").test);
}

/** Test JavaScript extraction: constants and variables, functions, function-valued constants
 * (which are functions), classes, and methods
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn javascript_outline() {
    let outline = extract(
        "web/src/cart.js",
        "import { api } from './api';\nconst TIMEOUT = 5;\nlet counter = 0;\nexport function submit(order) { return api.post(order); }\nexport const handler = (event) => submit(event.body);\nclass Cart { add(item) { this.items.push(item); } }\n",
    );
    assert_eq!(outline.imports, ["./api"]);
    assert_eq!(
        listing(&outline),
        [
            "data TIMEOUT",
            "variable counter",
            "function submit",
            "function handler",
            "class Cart",
            "method Cart.add",
        ]
    );
    assert_eq!(symbol(&outline, "handler").calls, ["submit"]);
    assert_eq!(symbol(&outline, "submit").calls, ["post"]);
    assert!(symbol(&outline, "submit").targetable);
}

/** Test TypeScript and TSX extraction: interfaces, abstract classes, classes and methods,
 * functions, type aliases, enums, and constants, none of which policies can target yet
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn typescript_outline() {
    let outline = extract(
        "web/src/checkout.ts",
        "import { Charger } from \"./charger\";\nexport interface Gateway { charge(amount: number): number; }\nexport abstract class Base { abstract run(): void; }\nexport class Service implements Gateway {\n  private rate: number = 2;\n  charge(amount: number): number { return fee(amount) + this.rate; }\n}\nexport function fee(amount: number): number { return Math.max(amount, 1); }\nexport type Id = string;\nexport enum Color { Red }\nexport const LIMIT = 10;\n",
    );
    assert_eq!(outline.status, "parsed");
    assert_eq!(outline.imports, ["./charger"]);
    assert_eq!(
        listing(&outline),
        [
            "interface Gateway",
            "class Base",
            "class Service",
            "method Service.charge",
            "function fee",
            "type Id",
            "enum Color",
            "data LIMIT",
        ]
    );
    assert_eq!(symbol(&outline, "Service.charge").calls, ["fee"]);
    assert!(outline.symbols.iter().all(|symbol| !symbol.targetable));

    let tsx = extract(
        "web/src/App.tsx",
        "export function App() { return <div>{render()}</div>; }\n",
    );
    assert_eq!(listing(&tsx), ["function App"]);
    assert_eq!(symbol(&tsx, "App").calls, ["render"]);
}

/** Test Go extraction: package, grouped imports, constants and variables, structs, interfaces,
 * methods qualified by receiver, functions with selector calls, and Test functions
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn go_outline() {
    let outline = extract(
        "services/billing/invoice.go",
        "package billing\n\nimport (\n\t\"fmt\"\n\t\"strings\"\n)\n\nconst TaxRate = 7\n\nvar registry = map[string]int{}\n\ntype Invoice struct {\n\tTotal int\n}\n\ntype Store interface {\n\tSave(i Invoice) error\n}\n\nfunc (i *Invoice) Finalize() int {\n\tif i.Total > 0 {\n\t\treturn computeTax(i.Total)\n\t}\n\treturn 0\n}\n\nfunc computeTax(total int) int {\n\tlocal := 1\n\tfmt.Println(strings.ToUpper(\"x\"))\n\tfor j := 0; j < 2; j++ {\n\t}\n\treturn total * TaxRate * local\n}\n\nfunc TestFinalize(t *testing.T) {\n\t(&Invoice{}).Finalize()\n}\n",
    );
    assert_eq!(outline.status, "parsed");
    assert_eq!(outline.package.as_deref(), Some("billing"));
    assert_eq!(outline.imports, ["fmt", "strings"]);
    assert_eq!(
        listing(&outline),
        [
            "data TaxRate",
            "variable registry",
            "struct Invoice",
            "interface Store",
            "method Invoice.Finalize",
            "function computeTax",
            "function TestFinalize",
        ]
    );
    let finalize = symbol(&outline, "Invoice.Finalize");
    assert_eq!(
        (finalize.calls.as_slice(), finalize.branches),
        (["computeTax".to_string()].as_slice(), 1)
    );
    let tax = symbol(&outline, "computeTax");
    assert_eq!(tax.calls, ["Println", "ToUpper"]);
    assert_eq!(tax.loops, 1);
    assert!(symbol(&outline, "TestFinalize").test);
    assert_eq!(symbol(&outline, "TestFinalize").calls, ["Finalize"]);
}

/** Test Rust extraction through the resolver's matcher: constants and statics, structs, enums,
 * traits and their signatures, impl methods, functions, and #[test] functions
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn rust_outline() {
    let outline = extract(
        "engine/src/ledger.rs",
        "use std::collections::HashMap;\n\npub const LIMIT: u32 = 5;\nstatic mut COUNTER: u32 = 0;\n\npub struct Ledger { entries: Vec<u32> }\npub enum Kind { Debit }\npub trait Store { fn save(&self); }\n\nimpl Ledger {\n    pub fn post(&mut self, amount: u32) -> u32 {\n        let total = apply_fee(amount);\n        match total { 0 => 0, _ => total }\n    }\n}\n\nfn apply_fee(amount: u32) -> u32 { amount + 1 }\n\n#[test]\nfn posts_entries() { apply_fee(1); }\n",
    );
    assert_eq!(outline.imports, ["std::collections::HashMap"]);
    assert_eq!(
        listing(&outline),
        [
            "data LIMIT",
            "data COUNTER",
            "struct Ledger",
            "enum Kind",
            "interface Store",
            "method Store.save",
            "method Ledger.post",
            "function apply_fee",
            "function posts_entries",
        ]
    );
    let post = symbol(&outline, "Ledger.post");
    assert_eq!(
        (post.calls.as_slice(), post.branches),
        (["apply_fee".to_string()].as_slice(), 2)
    );
    assert!(post.targetable);
    assert!(symbol(&outline, "posts_entries").test);
    assert!(!symbol(&outline, "apply_fee").test);
}

/** Test C++ extraction: includes, namespaces, namespace-level constants and variables, classes
 * with inline methods, out-of-line Class::method definitions, structs (forward declarations
 * skipped), free functions, and qualified and member calls
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn cpp_outline() {
    let outline = extract(
        "native/ledger.cpp",
        "#include <vector>\n#include \"ledger.h\"\n\nnamespace ledger {\nconst int LIMIT = 10;\nint counter = 0;\n\nclass Ledger {\npublic:\n  int post(int amount);\n  int size() const { return items.size(); }\nprivate:\n  std::vector<int> items;\n};\n\nstruct Entry { int value; };\nstruct Forward;\n\nint Ledger::post(int amount) {\n  for (int i = 0; i < amount; i++) {}\n  return audit(amount) + helper::scale(amount);\n}\n\nstatic int audit(int amount) { return amount; }\n}\n\nint main() { ledger::Ledger book; return book.post(1); }\n",
    );
    assert_eq!(outline.status, "parsed");
    assert_eq!(outline.imports, ["vector", "ledger.h"]);
    assert_eq!(
        listing(&outline),
        [
            "data LIMIT",
            "variable counter",
            "class Ledger",
            "method Ledger.size",
            "struct Entry",
            "method Ledger.post",
            "function audit",
            "function main",
        ]
    );
    let post = symbol(&outline, "Ledger.post");
    assert_eq!(post.calls, ["audit", "scale"]);
    assert_eq!(post.loops, 1);
    assert_eq!(post.namespace.as_deref(), Some("ledger"));
    assert_eq!(symbol(&outline, "main").namespace, None);
    assert_eq!(symbol(&outline, "main").calls, ["post"]);
}

/** Test Kotlin extraction: package, imports, const and top-level properties, interfaces,
 * classes with members, objects, enum classes, expression-bodied functions, and @Test methods
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn kotlin_outline() {
    let outline = extract(
        "android/src/main/kotlin/com/acme/wallet/Wallet.kt",
        "package com.acme.wallet\n\nimport com.acme.ledger.Ledger\n\nconst val MAX = 5\nval registry = mutableListOf<String>()\n\ninterface Payer {\n    fun pay(amount: Int): Int\n}\n\nclass Wallet(private val owner: String) : Payer {\n    var balance: Int = 0\n\n    override fun pay(amount: Int): Int {\n        if (amount > balance) {\n            return 0\n        }\n        return authorize(amount)\n    }\n}\n\nobject Registry {\n    fun register(id: String) = registry.add(id)\n}\n\nenum class Currency { USD, EUR }\n\nfun authorize(amount: Int): Int = Ledger.post(amount)\n\nclass WalletTest {\n    @Test\n    fun paysOwner() {\n        Wallet(\"a\").pay(1)\n    }\n}\n",
    );
    assert_eq!(outline.package.as_deref(), Some("com.acme.wallet"));
    assert_eq!(outline.imports, ["com.acme.ledger.Ledger"]);
    assert_eq!(
        listing(&outline),
        [
            "data MAX",
            "variable registry",
            "interface Payer",
            "method Payer.pay",
            "class Wallet",
            "variable Wallet.balance",
            "method Wallet.pay",
            "class Registry",
            "method Registry.register",
            "enum Currency",
            "function authorize",
            "class WalletTest",
            "method WalletTest.paysOwner",
        ]
    );
    let pay = symbol(&outline, "Wallet.pay");
    assert_eq!(
        (pay.calls.as_slice(), pay.branches),
        (["authorize".to_string()].as_slice(), 1)
    );
    assert_eq!(symbol(&outline, "authorize").calls, ["post"]);
    assert_eq!(symbol(&outline, "Registry.register").calls, ["add"]);
    assert!(symbol(&outline, "WalletTest.paysOwner").test);
}

/** Test that files are classified without parsing when discovery has no grammar, and that
 * oversized, binary, and syntactically broken files are reported instead of failing
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn unsupported_broken_and_skipped_files() {
    assert_eq!(
        language_of("README.md").map(|spec| spec.name),
        Some("markdown")
    );
    assert_eq!(
        language_of("deploy/Dockerfile").map(|spec| spec.name),
        Some("dockerfile")
    );
    assert_eq!(language_of("lib/x.h").map(|spec| spec.name), Some("c"));
    assert!(language_of("image.png").is_none());
    let markdown = extract("README.md", "# Title\n\ntext\n");
    assert_eq!(
        (markdown.status.as_str(), markdown.lines),
        ("unsupported", 3)
    );
    let broken = extract("a.py", "def ok():\n    return 1\n\ndef broken(:\n");
    assert_eq!(broken.status, "partial");
    assert!(listing(&broken).contains(&"function ok".to_string()));
    let binary = outline(b"\x00\x01java", "x.java", language_of("x.java").unwrap());
    assert_eq!(binary.status, "skipped: binary");
    let huge = "a".repeat(1_000_001);
    assert!(extract("big.js", &huge)
        .status
        .starts_with("skipped: larger"));
}

/** Test that an outline survives the cache's JSON round trip unchanged
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn outlines_round_trip_through_the_cache() {
    let outline = extract(
        "native/a.cpp",
        "namespace n { int f() { return g(); } }\n#include <x>\n",
    );
    assert_eq!(Outline::from_json(&outline.to_json()), Some(outline));
    assert_eq!(Outline::from_json(&serde_json::json!({"lines": 1})), None);
}

/** Build a source file for module-name tests
 * Input
    - path: &str - file path
    - package: Option<&str> - declared package
 * Output
    - SourceFile
*/
fn file(path: &str, package: Option<&str>) -> SourceFile {
    SourceFile {
        path: path.into(),
        blob: String::new(),
        language: language_of(path),
        outline: Arc::new(Outline {
            lines: 0,
            status: "parsed".into(),
            package: package.map(String::from),
            imports: Vec::new(),
            calls: Vec::new(),
            symbols: Vec::new(),
        }),
    }
}

/** Test module naming per language convention, which makes symbol ids semantic: Java and Kotlin
 * use the declared package, Go the folder, Python dotted paths, JavaScript and TypeScript paths,
 * Rust crate module paths, and C++ the folder
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn module_names_follow_language_conventions() {
    let cases = [
        (
            "svc/src/main/java/x/A.java",
            Some("com.acme.pay"),
            "com.acme.pay",
        ),
        ("A.java", None, ""),
        (
            "app/src/main/kotlin/W.kt",
            Some("com.acme.wallet"),
            "com.acme.wallet",
        ),
        (
            "services/billing/invoice.go",
            Some("billing"),
            "services/billing",
        ),
        ("main.go", Some("main"), "main"),
        ("analytics/report.py", None, "analytics.report"),
        ("src/pkg/__init__.py", None, "pkg"),
        ("web/src/checkout.ts", None, "web/src/checkout"),
        ("src/util/index.js", None, "util"),
        ("src/inventory/mod.rs", None, "crate.inventory"),
        ("engine/src/lib.rs", None, "engine"),
        ("engine/src/a/b.rs", None, "engine.a.b"),
        ("engine/tests/settle.rs", None, "engine.tests.settle"),
        ("native/core/ledger.cpp", None, "native/core"),
    ];
    for (path, package, expected) in cases {
        assert_eq!(module_name(&file(path, package)), expected, "{path}");
    }
}

/** Test test-file detection and CODEOWNERS matching (anchored, unanchored, folder-only, and **
 * patterns), and identifier splitting for risk words
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn test_paths_owners_and_words() {
    for path in [
        "src/test/java/a/PaymentServiceTest.java",
        "tests/settle.rs",
        "web/src/checkout.test.ts",
        "web/src/cart.spec.js",
        "analytics/test_report.py",
        "services/billing/invoice_test.go",
        "pkg/__tests__/a.js",
    ] {
        assert!(is_test_path(path), "{path}");
    }
    for path in [
        "src/latest.py",
        "contest/a.go",
        "attestation.rs",
        "src/testing_utils.rs",
    ] {
        assert!(!is_test_path(path), "{path}");
    }
    assert!(codeowners_match("*", "a/b/c.rs"));
    assert!(codeowners_match("*.go", "services/billing/invoice.go"));
    assert!(!codeowners_match("*.go", "services/billing/invoice.rs"));
    assert!(codeowners_match(
        "/services/payments/",
        "services/payments/src/A.java"
    ));
    assert!(!codeowners_match(
        "/services/payments/",
        "other/services/payments/A.java"
    ));
    assert!(codeowners_match(
        "payments/",
        "other/services/payments/A.java"
    ));
    assert!(!codeowners_match("docs/", "docs"));
    assert!(codeowners_match("docs/*.md", "docs/a.md"));
    assert!(!codeowners_match("docs/*.md", "docs/x/a.md"));
    assert!(codeowners_match(
        "**/migrations/**",
        "db/migrations/001.sql"
    ));
    assert!(codeowners_match("/build.rs", "build.rs"));
    assert_eq!(
        words("PaymentService.chargeCustomer_now/settle-ledger"),
        ["payment", "service", "charge", "customer", "now", "settle", "ledger"]
    );
}

/** Caches that running code generates are never files anyone changed: an agent's session must not
 * be blamed for the bytecode its own tests (or Crane's) leave behind */
#[test]
fn generated_caches_are_not_repository_files() {
    use super::index::excluded;
    for path in [
        "services/payments/pay/__pycache__/service.cpython-312.pyc",
        "pay/service.pyc",
        ".pytest_cache/v/cache/lastfailed",
        "web/node_modules/left-pad/index.js",
    ] {
        assert!(excluded(path), "{path}");
    }
    for path in [
        "services/payments/pay/service.py",
        "docs/pycache.md",
        "src/cache.rs",
    ] {
        assert!(!excluded(path), "{path}");
    }
}
