use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

/** Counter that keeps fixture directory names unique when tests run in parallel */
static REPOSITORIES: AtomicUsize = AtomicUsize::new(0);

/** A polyglot monorepo with one service per language, tests, and CODEOWNERS */
const MONOREPO: &[(&str, &str)] = &[
    ("README.md", "# Demo\n"),
    (
        ".github/CODEOWNERS",
        "* @acme/core\n/services/payments/ @acme/payments\n*.go @acme/go-team\n",
    ),
    ("services/payments/pom.xml", "<project/>\n"),
    (
        "services/payments/src/main/java/com/acme/payments/PaymentService.java",
        "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    public int refund(int amount) {\n        return charge(-amount);\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n",
    ),
    (
        "services/payments/src/test/java/com/acme/payments/PaymentServiceTest.java",
        "package com.acme.payments;\n\nclass PaymentServiceTest {\n    @Test\n    void charges() {\n        new PaymentService().charge(5);\n    }\n}\n",
    ),
    ("services/billing/go.mod", "module example.com/billing\n"),
    (
        "services/billing/invoice.go",
        "package billing\n\nfunc CreateInvoice(total int) int {\n\treturn computeTax(total) + total\n}\n\nfunc computeTax(total int) int {\n\treturn total / 5\n}\n",
    ),
    (
        "services/billing/invoice_test.go",
        "package billing\n\nimport \"testing\"\n\nfunc TestCreateInvoice(t *testing.T) {\n\tCreateInvoice(10)\n}\n",
    ),
    ("web/package.json", "{}\n"),
    (
        "web/src/checkout.ts",
        "export function submitPayment(card: string): boolean {\n  return validateCard(card);\n}\n\nfunction validateCard(card: string): boolean {\n  return card.length > 0;\n}\n",
    ),
    (
        "web/src/checkout.test.ts",
        "import { submitPayment } from \"./checkout\";\n\ntest(\"submits\", () => {\n  expect(submitPayment(\"4242\")).toBe(true);\n});\n",
    ),
    (
        "web/src/legacy.js",
        "export function legacyTotal(items) { return items.length; }\n",
    ),
    ("analytics/pyproject.toml", "[project]\nname = \"analytics\"\n"),
    (
        "analytics/report.py",
        "def build_report(rows):\n    return load_rows(rows)\n\n\ndef load_rows(rows):\n    return list(rows)\n",
    ),
    (
        "analytics/test_report.py",
        "from report import build_report\n\n\ndef test_build_report():\n    build_report([1])\n",
    ),
    ("engine/Cargo.toml", "[package]\nname = \"engine\"\n"),
    (
        "engine/src/lib.rs",
        "pub fn settle_balance(amount: u32) -> u32 {\n    apply_fee(amount)\n}\n\nfn apply_fee(amount: u32) -> u32 {\n    amount + 1\n}\n",
    ),
    ("native/CMakeLists.txt", "project(native)\n"),
    (
        "native/ledger.cpp",
        "namespace ledger {\nint audit(int amount) { return amount; }\n\nclass Ledger {\npublic:\n  int post(int amount);\n};\n\nint Ledger::post(int amount) { return audit(amount); }\n}\n",
    ),
    ("android/build.gradle.kts", "plugins {}\n"),
    (
        "android/src/main/kotlin/com/acme/wallet/Wallet.kt",
        "package com.acme.wallet\n\nclass Wallet {\n    fun pay(amount: Int): Int {\n        return authorize(amount)\n    }\n}\n\nfun authorize(amount: Int): Int = amount\n",
    ),
    ("node_modules/left-pad/index.js", "module.exports = 1;\n"),
];

/** Policies used to check contract coverage: one resolved preserve, one target, one missing */
const POLICIES: &str = "policy payments {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n    target --function PaymentService.refund;\n}\n";

/** Policy whose target does not exist */
const GHOST: &str =
    "policy ghost {\n    checkpoint baseline;\n    preserve --function Ghost.missing;\n}\n";

/** A temporary Git repository, removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Repository {
    root: PathBuf,
}

impl Repository {
    /** Create a repository holding the given files in one commit
     * Input
        - files: &[(&str, &str)] - relative paths and contents
     * Output
        - Repository
    */
    fn new(files: &[(&str, &str)]) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-discover-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Self { root };
        for (path, content) in files {
            repository.write(path, content);
        }
        repository.git(&["init", "-q"]);
        repository.git(&["config", "user.email", "crane@example.com"]);
        repository.git(&["config", "user.name", "Crane Discover Test"]);
        repository.git(&["config", "core.autocrlf", "false"]);
        repository.git(&["add", "."]);
        repository.git(&["commit", "-qm", "baseline"]);
        repository
    }

    /** Write a file relative to the root, creating parent folders
     * Input
        - path: &str - relative path
        - content: &str - file content
     * Output
        - None
    */
    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /** Run git in the repository and require success
     * Input
        - args: &[&str] - git arguments
     * Output
        - String stdout
    */
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("git should execute");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /** Run crane in the repository
     * Input
        - args: &[&str] - crane arguments
     * Output
        - Output of the finished process
    */
    fn crane(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_crane"))
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("crane should execute")
    }

    /** Initialize Crane, create the baseline checkpoint, and write policies
     * Input
        - policies: &[(&str, &str)] - policy file names and contents
     * Output
        - None
    */
    fn initialize(&self, policies: &[(&str, &str)]) {
        assert!(self.crane(&["init"]).status.success());
        assert!(self
            .crane(&["checkpoint", "--name", "baseline"])
            .status
            .success());
        for (name, content) in policies {
            self.write(&format!(".crane/policies/{name}.crane"), content);
        }
    }

    /** Run crane discover --json (plus extra arguments) and parse the inventory
     * Input
        - extra: &[&str] - extra arguments such as --full
     * Output
        - Value inventory
    */
    fn discover(&self, extra: &[&str]) -> Value {
        let mut args = vec!["discover", "--json"];
        args.extend_from_slice(extra);
        let output = self.crane(&args);
        assert!(
            output.status.success(),
            "discover failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("discover --json prints JSON")
    }
}

impl Drop for Repository {
    /** Remove the temporary repository
     * Input
        - None (uses self)
     * Output
        - None
    */
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/** Find an entity by id, panicking with the known ids when it is missing
 * Input
    - inventory: &'a Value - inventory
    - id: &str - entity id
 * Output
    - &'a Value entity
*/
fn entity<'a>(inventory: &'a Value, id: &str) -> &'a Value {
    let entities = inventory["entities"].as_array().unwrap();
    entities
        .iter()
        .find(|entity| entity["id"] == id)
        .unwrap_or_else(|| {
            let ids = entities
                .iter()
                .map(|entity| entity["id"].as_str().unwrap())
                .collect::<Vec<_>>();
            panic!("{id} missing from {ids:?}")
        })
}

/** Find an element of an array by a field value
 * Input
    - array: &'a Value - JSON array
    - key: &str - field name
    - value: &str - wanted value
 * Output
    - &'a Value element
*/
fn find<'a>(array: &'a Value, key: &str, value: &str) -> &'a Value {
    array
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item[key] == value)
        .unwrap_or_else(|| panic!("no {key} = {value} in {array}"))
}

/** Check that a JSON array of strings contains a value
 * Input
    - array: &Value - JSON array
    - value: &str - wanted value
 * Output
    - bool
*/
fn has(array: &Value, value: &str) -> bool {
    array
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item == value))
}

/** The inventory of a polyglot monorepo: languages (with which are enforceable), semantic ids
 * per language convention, callers and callees, test relationships by symbol and by file,
 * services from manifests, modules, folders, CODEOWNERS owners, and excluded vendored code
 */
#[test]
fn polyglot_inventory_has_semantic_entities_and_relationships() {
    let repository = Repository::new(MONOREPO);
    let inventory = repository.discover(&[]);
    assert_eq!(inventory["advisory"], true);
    assert_eq!(inventory["repository"]["excluded_files"], 1);

    for (language, enforceable) in [
        ("java", true),
        ("javascript", true),
        ("python", true),
        ("rust", true),
        ("typescript", false),
        ("go", false),
        ("cpp", false),
        ("kotlin", false),
    ] {
        let totals = find(&inventory["languages"], "language", language);
        assert_eq!(totals["enforceable"], enforceable, "{language}");
        assert!(totals["symbols"].as_u64().unwrap() > 0, "{language}");
        assert_eq!(totals["partial"], 0, "{language}");
    }

    // Java: package-based ids, callers and callees, and a symbol-level test link
    let charge = entity(
        &inventory,
        "symbol:java:com.acme.payments.PaymentService.charge",
    );
    assert_eq!(charge["kind"], "method");
    assert_eq!(charge["module"], "module:java:com.acme.payments");
    assert_eq!(charge["service"], "service:services/payments");
    assert_eq!(charge["owners"], serde_json::json!(["@acme/payments"]));
    assert_eq!(charge["policy_target"], "--function PaymentService.charge");
    assert!(has(
        &charge["callers"],
        "symbol:java:com.acme.payments.PaymentService.refund"
    ));
    assert!(has(
        &charge["callees"],
        "symbol:java:com.acme.payments.PaymentService.fee"
    ));
    assert!(has(
        &charge["tested_by"],
        "symbol:java:com.acme.payments.PaymentServiceTest.charges"
    ));
    assert_eq!(
        entity(
            &inventory,
            "symbol:java:com.acme.payments.PaymentServiceTest.charges"
        )["test"],
        true
    );

    // Go: folder-based package ids, a Test function, and owners from a *.go rule
    let invoice = entity(&inventory, "symbol:go:services/billing.CreateInvoice");
    assert!(has(
        &invoice["callees"],
        "symbol:go:services/billing.computeTax"
    ));
    assert!(has(
        &invoice["tested_by"],
        "symbol:go:services/billing.TestCreateInvoice"
    ));
    assert_eq!(invoice["owners"], serde_json::json!(["@acme/go-team"]));
    assert_eq!(invoice["policy_target"], Value::Null);

    // TypeScript: a Jest-style test reaches the code only through a callback, so the link is
    // file-level
    let submit = entity(
        &inventory,
        "symbol:typescript:web/src/checkout.submitPayment",
    );
    assert!(has(
        &submit["callees"],
        "symbol:typescript:web/src/checkout.validateCard"
    ));
    assert!(has(&submit["tested_by"], "file:web/src/checkout.test.ts"));
    entity(&inventory, "symbol:javascript:web/src/legacy.legacyTotal");

    // Python, Rust, C++, Kotlin
    let report = entity(&inventory, "symbol:python:analytics.report.build_report");
    assert!(has(
        &report["callees"],
        "symbol:python:analytics.report.load_rows"
    ));
    assert!(has(
        &report["tested_by"],
        "symbol:python:analytics.test_report.test_build_report"
    ));
    assert_eq!(report["owners"], serde_json::json!(["@acme/core"]));
    let settle = entity(&inventory, "symbol:rust:engine.settle_balance");
    assert!(has(&settle["callees"], "symbol:rust:engine.apply_fee"));
    let post = entity(&inventory, "symbol:cpp:ledger.Ledger.post");
    assert!(has(&post["callees"], "symbol:cpp:ledger.audit"));
    let pay = entity(&inventory, "symbol:kotlin:com.acme.wallet.Wallet.pay");
    assert!(has(
        &pay["callees"],
        "symbol:kotlin:com.acme.wallet.authorize"
    ));

    // Services from manifests and layout, and file-level relationships
    for (service, evidence) in [
        ("service:services/payments", "pom.xml"),
        ("service:services/billing", "go.mod"),
        ("service:web", "package.json"),
        ("service:analytics", "pyproject.toml"),
        ("service:engine", "Cargo.toml"),
        ("service:native", "CMakeLists.txt"),
        ("service:android", "build.gradle.kts"),
    ] {
        assert!(has(
            &find(&inventory["services"], "id", service)["evidence"],
            evidence
        ));
    }
    let test_file = find(&inventory["files"], "path", "web/src/checkout.test.ts");
    assert_eq!(test_file["test"], true);
    assert!(has(&test_file["tests"], "web/src/checkout.ts"));
    let java_test = find(
        &inventory["files"],
        "path",
        "services/payments/src/test/java/com/acme/payments/PaymentServiceTest.java",
    );
    assert!(has(
        &java_test["tests"],
        "services/payments/src/main/java/com/acme/payments/PaymentService.java"
    ));
    let folder = find(&inventory["folders"], "path", "services");
    assert!(has(&folder["children"], "services/billing"));
    assert_eq!(inventory["ownership"]["source"], ".github/CODEOWNERS");

    // Critical candidates: advisory scores with rule suggestions only where policies could hold
    let critical = &inventory["critical_candidates"];
    let settle_candidate = find(critical, "id", "symbol:rust:engine.settle_balance");
    assert_eq!(
        settle_candidate["suggestion"],
        "preserve --function settle_balance;"
    );
    assert!(has(
        &settle_candidate["risk_signals"],
        "sensitive_name:settle"
    ));
    let wallet = find(critical, "id", "symbol:kotlin:com.acme.wallet.Wallet.pay");
    assert_eq!(wallet["suggestion"], Value::Null);
    assert!(wallet["suggestion_note"]
        .as_str()
        .unwrap()
        .contains("cannot be targeted"));
}

/** Discovery works before crane init, writes nothing (there is no cache without .crane), and
 * prints a human summary with every section
 */
#[test]
fn discovery_without_crane_init_is_read_only() {
    let repository = Repository::new(MONOREPO);
    let output = repository.crane(&["discover"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = String::from_utf8_lossy(&output.stdout);
    for section in [
        "advisory: nothing here is enforced",
        "Languages:",
        "Services (",
        "Modules (",
        "Critical candidates (",
        "Contract coverage:",
        "run 'crane init'",
        "Index:",
    ] {
        assert!(summary.contains(section), "{section}\n{summary}");
    }
    assert!(summary.contains("kotlin"), "{summary}");
    assert!(!repository.root.join(".crane").exists());
    assert_eq!(repository.git(&["status", "--porcelain"]), "");
    let inventory = repository.discover(&[]);
    assert_eq!(inventory["contracts"], Value::Null);
    assert_eq!(inventory["index"]["cache"], Value::Null);
    let unknown = repository.crane(&["discover", "--fast"]);
    assert!(!unknown.status.success());
}

/** Existing contracts, coverage, and checkpoint relationships are described without being
 * changed or enforced: policies, checkpoints, and the result of crane check are the same before
 * and after discovery
 */
#[test]
fn contracts_and_checkpoints_are_described_not_enforced() {
    let repository = Repository::new(MONOREPO);
    repository.initialize(&[("payments", POLICIES), ("ghost", GHOST)]);
    let check_before = repository.crane(&["check", "--json"]);
    let policies_before =
        fs::read_to_string(repository.root.join(".crane/policies/payments.crane")).unwrap();
    let checkpoint_before =
        fs::read_to_string(repository.root.join(".crane/checkpoints/baseline.json")).unwrap();

    let inventory = repository.discover(&[]);
    let clauses = &inventory["contracts"]["clauses"];
    let preserve = find(clauses, "target", "PaymentService.charge");
    assert_eq!(preserve["status"], "resolved");
    assert_eq!(
        preserve["anchors"],
        serde_json::json!(["symbol:java:com.acme.payments.PaymentService.charge"])
    );
    assert_eq!(
        find(clauses, "target", "Ghost.missing")["status"],
        "missing"
    );
    let charge = entity(
        &inventory,
        "symbol:java:com.acme.payments.PaymentService.charge",
    );
    assert_eq!(
        charge["contracts"],
        serde_json::json!(["payments:preserve"])
    );
    let refund = entity(
        &inventory,
        "symbol:java:com.acme.payments.PaymentService.refund",
    );
    assert_eq!(refund["contracts"], serde_json::json!(["payments:target"]));
    assert_eq!(inventory["coverage"]["callables"], 16);
    assert_eq!(inventory["coverage"]["callables_covered"], 2);
    assert!(inventory["contracts"]["contract_version"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));

    let baseline = find(&inventory["checkpoints"], "name", "baseline");
    assert_eq!(baseline["status"], "valid");
    assert_eq!(baseline["commits_since"], 0);
    assert_eq!(
        baseline["policies"],
        serde_json::json!(["ghost", "payments"])
    );

    // Advisory only: nothing about enforcement changed
    let check_after = repository.crane(&["check", "--json"]);
    assert_eq!(check_after.status.code(), check_before.status.code());
    assert_eq!(check_after.stdout, check_before.stdout);
    assert_eq!(
        fs::read_to_string(repository.root.join(".crane/policies/payments.crane")).unwrap(),
        policies_before
    );
    assert_eq!(
        fs::read_to_string(repository.root.join(".crane/checkpoints/baseline.json")).unwrap(),
        checkpoint_before
    );
    assert!(repository
        .root
        .join(".crane/runtime/inventory/index.json")
        .is_file());

    // After a commit touching covered code, the checkpoint relationship shows it
    let path = "services/payments/src/main/java/com/acme/payments/PaymentService.java";
    let source = fs::read_to_string(repository.root.join(path)).unwrap();
    repository.write(path, &source.replace("amount / 10", "amount / 20"));
    repository.git(&["commit", "-qam", "new fee"]);
    let moved = repository.discover(&[]);
    let baseline = find(&moved["checkpoints"], "name", "baseline");
    assert_eq!(baseline["commits_since"], 1);
    assert!(has(&baseline["changed_files"], path));
    assert_eq!(baseline["covered_entities_changed"], 2);
    let summary = String::from_utf8_lossy(&repository.crane(&["discover"]).stdout).into_owned();
    assert!(
        summary.contains("ghost preserve --function Ghost.missing: target is missing"),
        "{summary}"
    );
    assert!(summary.contains("checkpoint baseline"), "{summary}");
}

/** Incremental indexing: a second run parses nothing and relinks nothing; adding or changing a
 * file parses only that file and relinks only the callers that could be affected; and the
 * incremental inventory equals a full rebuild
 */
#[test]
fn incremental_index_reparses_and_relinks_only_what_changed() {
    let repository = Repository::new(MONOREPO);
    repository.initialize(&[]);
    let first = repository.discover(&[]);
    assert_eq!(first["index"]["incremental"], false);
    let indexed = first["index"]["files_parsed"].as_u64().unwrap();
    assert_eq!(indexed, 19, "{}", first["index"]);

    let second = repository.discover(&[]);
    assert_eq!(second["index"]["incremental"], true);
    assert_eq!(second["index"]["files_parsed"], 0);
    assert_eq!(second["index"]["files_reused"], indexed);
    assert_eq!(second["index"]["files_changed"], 0);
    assert_eq!(second["index"]["symbols_relinked"], 0);
    assert_eq!(second["entities"], first["entities"]);

    // A new caller in a new file: one file parsed, one symbol relinked, and the callee learns it
    repository.write(
        "analytics/export.py",
        "from report import build_report\n\n\ndef export():\n    return build_report([])\n",
    );
    let added = repository.discover(&[]);
    assert_eq!(added["index"]["files_parsed"], 1);
    assert_eq!(added["index"]["files_changed"], 1);
    assert_eq!(added["index"]["symbols_relinked"], 1);
    assert!(has(
        &entity(&added, "symbol:python:analytics.report.build_report")["callers"],
        "symbol:python:analytics.export.export"
    ));

    // A rename inside one file relinks only that file's callers of the renamed names
    let lib = "engine/src/lib.rs";
    let source = fs::read_to_string(repository.root.join(lib)).unwrap();
    repository.write(lib, &source.replace("apply_fee", "apply_charge"));
    let renamed = repository.discover(&[]);
    assert_eq!(renamed["index"]["files_parsed"], 1);
    assert_eq!(renamed["index"]["symbols_relinked"], 2);
    assert!(has(
        &entity(&renamed, "symbol:rust:engine.settle_balance")["callees"],
        "symbol:rust:engine.apply_charge"
    ));

    // Moving a function to another file of the same module keeps resolution right everywhere
    let go = "services/billing/invoice.go";
    let source = fs::read_to_string(repository.root.join(go)).unwrap();
    let (head, tax) = source.split_at(source.find("func computeTax").unwrap());
    repository.write(go, head);
    repository.write(
        "services/billing/tax.go",
        &format!("package billing\n\n{tax}"),
    );
    let moved = repository.discover(&[]);
    assert_eq!(moved["index"]["files_parsed"], 2);
    assert!(has(
        &entity(&moved, "symbol:go:services/billing.CreateInvoice")["callees"],
        "symbol:go:services/billing.computeTax"
    ));

    let full = repository.discover(&["--full"]);
    assert_eq!(full["index"]["incremental"], false);
    assert_eq!(full["entities"], moved["entities"]);
    assert_eq!(full["modules"], moved["modules"]);
}

/** A larger repository is indexed once; afterwards a one-file change parses one file and
 * relinks only the changed function and its caller, and a no-change run parses nothing
 */
#[test]
fn large_repository_is_not_reparsed_after_each_change() {
    let mut files = Vec::new();
    for index in 0..400 {
        let call = if index == 0 {
            "0".to_string()
        } else {
            format!("f{}()", index - 1)
        };
        files.push((
            format!("pkg/m{index}.py"),
            format!("def f{index}():\n    return {call}\n"),
        ));
    }
    let borrowed = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect::<Vec<_>>();
    let repository = Repository::new(&borrowed);
    repository.initialize(&[]);
    let started = Instant::now();
    let first = repository.discover(&[]);
    let cold = started.elapsed();
    assert_eq!(first["index"]["files_parsed"], 400);
    assert!(has(
        &entity(&first, "symbol:python:pkg.m100.f100")["callers"],
        "symbol:python:pkg.m101.f101"
    ));

    let started = Instant::now();
    let warm = repository.discover(&[]);
    let warm_time = started.elapsed();
    assert_eq!(warm["index"]["files_parsed"], 0);
    assert_eq!(warm["index"]["symbols_relinked"], 0);

    repository.write("pkg/m200.py", "def f200():\n    return f199() + 1\n");
    let changed = repository.discover(&[]);
    assert_eq!(changed["index"]["files_parsed"], 1);
    assert_eq!(changed["index"]["symbols_relinked"], 2);
    eprintln!("400 files: cold {cold:?}, warm {warm_time:?}");
}
