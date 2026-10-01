use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/** Counter that keeps fixture directory names unique when tests run in parallel */
static REPOSITORIES: AtomicUsize = AtomicUsize::new(0);

/** Path of the Java payment service in the fixture */
const SERVICE: &str = "services/payments/src/main/java/com/acme/payments/PaymentService.java";

/** Java payment service source */
const PAYMENT: &str = "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    public int refund(int amount) {\n        return charge(-amount);\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n";

/** Repository files: a payments service with tests, an auth package, and analytics */
const FILES: &[(&str, &str)] = &[
    ("README.md", "# Demo\n"),
    ("services/payments/pom.xml", "<project/>\n"),
    (SERVICE, PAYMENT),
    (
        "services/payments/src/test/java/com/acme/payments/PaymentServiceTest.java",
        "package com.acme.payments;\n\nclass PaymentServiceTest {\n    @Test\n    void charges() {\n        new PaymentService().charge(5);\n    }\n}\n",
    ),
    (
        "auth/login.py",
        "def authenticate(user):\n    return hash_password(user)\n\n\ndef hash_password(value):\n    return value\n",
    ),
    (
        "analytics/report.py",
        "def charge(rows):\n    return len(rows)\n",
    ),
    ("web/pay.js", "export function charge(card) { return card; }\n"),
];

/** The organization's zones: "Payments is Critical", "Authentication is Restricted", "Tests
 * are Routine" */
const ZONES: &str = "zone payments {\n    criticality critical;\n    autonomy assisted;\n    policy payments;\n    select subsystem payments;\n    select symbol java:com.acme.payments.PaymentService.charge;\n}\n\nzone authentication {\n    criticality restricted;\n    autonomy observe;\n    select subsystem auth*;\n}\n\nzone tests {\n    criticality routine;\n    autonomy autonomous;\n    select tests;\n}\n";

/** Policy preserving charge and requiring a change to refund */
const POLICY: &str = "policy payments {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n    target --function PaymentService.refund;\n}\n";

/** A temporary repository with Crane initialized, a baseline checkpoint, a policy, and zones,
 * removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Repository {
    root: PathBuf,
}

impl Repository {
    /** Create the fixture repository with the given zone file
     * Input
        - zones: &str - content of .crane/zones/org.zone
     * Output
        - Repository
    */
    fn new(zones: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-zones-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Self { root };
        for (path, content) in FILES {
            repository.write(path, content);
        }
        repository.git(&["init", "-q"]);
        repository.git(&["config", "user.email", "crane@example.com"]);
        repository.git(&["config", "user.name", "Crane Zones Test"]);
        repository.git(&["config", "core.autocrlf", "false"]);
        repository.git(&["add", "."]);
        repository.git(&["commit", "-qm", "baseline"]);
        assert!(repository.crane(&["init"], "").status.success());
        assert!(repository
            .crane(&["checkpoint", "--name", "baseline"], "")
            .status
            .success());
        repository.write(".crane/policies/payments.crane", POLICY);
        repository.write(".crane/zones/org.zone", zones);
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

    /** Read a file relative to the root
     * Input
        - path: &str - relative path
     * Output
        - String
    */
    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }

    /** Run git in the repository and require success
     * Input
        - args: &[&str] - git arguments
     * Output
        - None
    */
    fn git(&self, args: &[&str]) {
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
    }

    /** Run crane in the repository with the given stdin
     * Input
        - args: &[&str] - crane arguments
        - stdin: &str - text written to stdin
     * Output
        - Output of the finished process
    */
    fn crane(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_crane"))
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("crane should execute");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /** Run crane zones --json and parse it, requiring success
     * Input
        - None
     * Output
        - Value zones JSON
    */
    fn zones(&self) -> Value {
        let output = self.crane(&["zones", "--json"], "");
        assert!(
            output.status.success(),
            "zones failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("zones --json prints JSON")
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

/** Find the result of one selector of one zone
 * Input
    - zones: &'a Value - zones JSON
    - zone: &str - zone id
    - selector: &str - selector text
 * Output
    - &'a Value selector result
*/
fn selector<'a>(zones: &'a Value, zone: &str, selector: &str) -> &'a Value {
    find(
        &find(&zones["zones"], "zone_id", zone)["selectors"],
        "selector",
        selector,
    )
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

/** Id of the charge method */
const CHARGE: &str = "symbol:java:com.acme.payments.PaymentService.charge";

/** Semantic resolution: subsystems resolve to services, modules, and folders by name; zones say
 * "Payments is Critical", "Authentication is Restricted", and "Tests are Routine"; overlapping
 * zones combine to the most restrictive values and are reported; zones grant nothing
 */
#[test]
fn zones_resolve_semantic_selectors() {
    let repository = Repository::new(ZONES);
    let zones = repository.zones();
    assert_eq!(zones["grants_permissions"], false);
    assert!(zones["zone_set_version"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(zones["problems"], json!([]));

    let payments = selector(&zones, "payments", "subsystem payments");
    assert_eq!(payments["status"], "resolved");
    for target in [
        "service:services/payments",
        "module:java:com.acme.payments",
        "folder:services/payments",
    ] {
        assert!(has(&payments["targets"], target), "{target}: {payments}");
    }
    let auth = selector(&zones, "authentication", "subsystem auth*");
    assert!(has(&auth["targets"], "module:python:auth.login"), "{auth}");
    assert!(has(&auth["targets"], "folder:auth"), "{auth}");
    assert!(!has(&auth["targets"], "module:python:analytics.report"));

    let charge = find(&zones["entities"], "id", CHARGE);
    assert_eq!(charge["criticality"], "critical");
    assert_eq!(charge["autonomy"], "assisted");
    assert_eq!(charge["safety_state"], "active");
    assert_eq!(charge["contracts"], json!(["payments:preserve"]));
    let login = find(
        &zones["entities"],
        "id",
        "symbol:python:auth.login.authenticate",
    );
    assert_eq!(
        (login["criticality"].as_str(), login["autonomy"].as_str()),
        (Some("restricted"), Some("observe"))
    );
    let report = zones["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entity| entity["id"] == "symbol:python:analytics.report.charge");
    assert!(report.is_none(), "analytics is in no zone");

    // A payments test is both Critical (payments) and Routine (tests): most restrictive wins
    let test = find(
        &zones["entities"],
        "id",
        "symbol:java:com.acme.payments.PaymentServiceTest.charges",
    );
    assert_eq!(test["zones"], json!(["payments", "tests"]));
    assert_eq!(
        (test["criticality"].as_str(), test["autonomy"].as_str()),
        (Some("critical"), Some("assisted"))
    );
    let overlap = find(&zones["conflicts"], "kind", "overlap");
    assert_eq!(overlap["zones"], json!(["payments", "tests"]));
    assert!(overlap["message"]
        .as_str()
        .unwrap()
        .contains("most restrictive values apply: critical, assisted, active"));
    let readme = zones["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "README.md");
    assert!(readme.is_none());

    let summary = String::from_utf8_lossy(&repository.crane(&["zones"], "").stdout).into_owned();
    for text in [
        "zones only restrict, they never grant",
        "zone payments [zones/org.zone]",
        "criticality critical, autonomy assisted, state active, policy payments",
        "select subsystem payments: resolved -> ",
        "Unresolved selectors (0):",
        "Conflicts (",
    ] {
        assert!(summary.contains(text), "{text}\n{summary}");
    }
    let one = String::from_utf8_lossy(&repository.crane(&["zones", "authentication"], "").stdout)
        .into_owned();
    assert!(
        one.contains("symbol:python:auth.login.hash_password (restricted, observe, active)"),
        "{one}"
    );
    assert!(one.contains("file auth/login.py"), "{one}");
    assert!(!one.contains("zone payments"), "{one}");
    assert!(!repository.crane(&["zones", "nope"], "").status.success());
}

/** Rename and move handling: moving a Java class to another folder of the same package keeps
 * semantic selectors resolved while a folder selector reports where the content went, and
 * renaming a method reports the selector missing with the new name as a candidate, never
 * re-binding it silently
 */
#[test]
fn zones_survive_moves_and_report_renames() {
    let zones_file = format!(
        "{ZONES}\nzone ledger {{\n    criticality sensitive;\n    autonomy delegated;\n    select folder services/payments/src/main/java/com/acme/payments;\n}}\n"
    );
    let repository = Repository::new(&zones_file);
    let before = repository.zones();
    assert_eq!(
        selector(
            &before,
            "ledger",
            "folder services/payments/src/main/java/com/acme/payments"
        )["status"],
        "resolved"
    );

    let moved = "services/payments/src/main/java/com/acme/core/PaymentService.java";
    fs::create_dir_all(
        repository
            .root
            .join("services/payments/src/main/java/com/acme/core"),
    )
    .unwrap();
    repository.git(&["mv", SERVICE, moved]);
    let after_move = repository.zones();
    let symbol = selector(
        &after_move,
        "payments",
        "symbol java:com.acme.payments.PaymentService.charge",
    );
    assert_eq!(symbol["status"], "resolved");
    assert_eq!(symbol["targets"], json!([CHARGE]));
    assert_eq!(
        selector(&after_move, "payments", "subsystem payments")["status"],
        "resolved"
    );
    assert_eq!(find(&after_move["entities"], "id", CHARGE)["file"], moved);
    let folder = selector(
        &after_move,
        "ledger",
        "folder services/payments/src/main/java/com/acme/payments",
    );
    assert_eq!(folder["status"], "missing");
    assert_eq!(
        folder["rename_candidates"],
        json!(["folder:services/payments/src/main/java/com/acme/core"])
    );
    assert_eq!(
        find(&after_move["zones"], "zone_id", "ledger")["effective_safety_state"],
        "degraded"
    );

    // Renaming charge to collect: the zone does not follow silently, it suggests the new name
    repository.write(moved, &PAYMENT.replace("charge(", "collect("));
    let renamed = repository.zones();
    let symbol = selector(
        &renamed,
        "payments",
        "symbol java:com.acme.payments.PaymentService.charge",
    );
    assert_eq!(symbol["status"], "missing");
    assert_eq!(symbol["previous_targets"], json!([CHARGE]));
    assert_eq!(
        symbol["rename_candidates"],
        json!(["symbol:java:com.acme.payments.PaymentService.collect"])
    );
    let payments = find(&renamed["zones"], "zone_id", "payments");
    assert_eq!(payments["effective_safety_state"], "degraded");
    assert!(has(
        &payments["entities"],
        "symbol:java:com.acme.payments.PaymentService.collect"
    ));
    assert!(has(
        &find(&renamed["unresolved"], "zone_id", "payments")["rename_candidates"],
        "symbol:java:com.acme.payments.PaymentService.collect"
    ));
    let summary =
        String::from_utf8_lossy(&repository.crane(&["zones", "payments"], "").stdout).into_owned();
    assert!(
        summary.contains(
            "possibly renamed or moved to symbol:java:com.acme.payments.PaymentService.collect"
        ),
        "{summary}"
    );

    // Undoing the rename resolves the selector again
    repository.write(moved, PAYMENT);
    assert_eq!(
        selector(
            &repository.zones(),
            "payments",
            "symbol java:com.acme.payments.PaymentService.charge"
        )["status"],
        "resolved"
    );
}

/** Deleted targets: a selector whose target was deleted is reported missing with what it used
 * to cover and no candidate, a selector that never resolved is unresolved, and both degrade
 * their zone, capping its autonomy
 */
#[test]
fn deleted_targets_degrade_their_zone() {
    let zones_file = "zone tooling {\n    criticality routine;\n    autonomy autonomous;\n    select symbol java:com.acme.payments.PaymentService.fee;\n    select symbol Ghost.run;\n}\n";
    let repository = Repository::new(zones_file);
    let before = repository.zones();
    let tooling = find(&before["zones"], "zone_id", "tooling");
    assert_eq!(tooling["effective_autonomy"], "assisted");
    assert_eq!(
        selector(&before, "tooling", "symbol Ghost.run")["status"],
        "unresolved"
    );
    assert_eq!(
        selector(
            &before,
            "tooling",
            "symbol java:com.acme.payments.PaymentService.fee"
        )["status"],
        "resolved"
    );

    repository.write(
        SERVICE,
        &PAYMENT.replace(
            "    int fee(int amount) {\n        return amount / 10;\n    }\n",
            "",
        ),
    );
    let after = repository.zones();
    let fee = selector(
        &after,
        "tooling",
        "symbol java:com.acme.payments.PaymentService.fee",
    );
    assert_eq!(fee["status"], "missing");
    assert_eq!(
        fee["previous_targets"],
        json!(["symbol:java:com.acme.payments.PaymentService.fee"])
    );
    assert_eq!(fee["rename_candidates"], json!([]));
    let tooling = find(&after["zones"], "zone_id", "tooling");
    assert_eq!(tooling["entities"], json!([]));
    assert_eq!(tooling["effective_safety_state"], "degraded");
    assert_eq!(after["unresolved"].as_array().unwrap().len(), 2);
}

/** Ambiguous selectors: an exact symbol name matching several symbols is reported ambiguous,
 * covers every match (restricting more, never less), and degrades the zone; a wildcard is not
 * ambiguous
 */
#[test]
fn ambiguous_selectors_are_reported() {
    let zones_file = "zone billing {\n    criticality sensitive;\n    autonomy delegated;\n    select symbol charge;\n}\n\nzone everything {\n    criticality routine;\n    autonomy autonomous;\n    select symbol python:*;\n}\n";
    let repository = Repository::new(zones_file);
    let zones = repository.zones();
    let charge = selector(&zones, "billing", "symbol charge");
    assert_eq!(charge["status"], "ambiguous");
    assert_eq!(
        charge["targets"],
        json!([
            "symbol:javascript:web/pay.charge",
            "symbol:python:analytics.report.charge"
        ])
    );
    let billing = find(&zones["zones"], "zone_id", "billing");
    assert_eq!(billing["effective_safety_state"], "degraded");
    assert_eq!(billing["effective_autonomy"], "assisted");
    assert_eq!(
        selector(&zones, "everything", "symbol python:*")["status"],
        "resolved"
    );
    assert_eq!(
        find(&zones["unresolved"], "zone_id", "billing")["status"],
        "ambiguous"
    );
}

/** Policy conflicts: a zone declaring more autonomy than its criticality allows, a zone
 * referencing a policy that does not exist or covers nothing in it, and a target rule requiring a
 * change in code its zones only let agents observe
 */
#[test]
fn policy_conflicts_are_reported() {
    let zones_file = "zone lockdown {\n    criticality restricted;\n    autonomy autonomous;\n    policy missing;\n    select symbol java:com.acme.payments.PaymentService.refund;\n}\n\nzone analytics {\n    criticality sensitive;\n    autonomy delegated;\n    policy payments;\n    select subsystem analytics;\n}\n";
    let repository = Repository::new(zones_file);
    let zones = repository.zones();
    let kinds = zones["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|conflict| conflict["kind"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    for kind in [
        "autonomy_exceeds_criticality",
        "policy_reference_missing",
        "policy_reference_outside_zone",
        "policy_requires_change",
    ] {
        assert!(kinds.contains(&kind.to_string()), "{kind}: {kinds:?}");
    }
    let lockdown = find(&zones["zones"], "zone_id", "lockdown");
    assert_eq!(lockdown["effective_autonomy"], "observe");
    let requires = find(&zones["conflicts"], "kind", "policy_requires_change");
    assert_eq!(requires["zones"], json!(["lockdown"]));
    assert_eq!(
        requires["entities"],
        json!(["symbol:java:com.acme.payments.PaymentService.refund"])
    );
    let outside = find(&zones["conflicts"], "kind", "policy_reference_outside_zone");
    assert_eq!(outside["zones"], json!(["analytics"]));
}

/** Persistence: zone definitions and versions do not depend on checkpoints or file layout, so
 * committing changes, re-baselining the checkpoint, and editing policies leave the zones and
 * their resolution unchanged while the contract version moves on
 */
#[test]
fn zones_persist_across_checkpoint_changes() {
    let repository = Repository::new(ZONES);
    let before = repository.zones();
    repository.write(SERVICE, &PAYMENT.replace("amount / 10", "amount / 20"));
    repository.git(&["commit", "-qam", "new fee"]);
    assert!(repository
        .crane(&["checkpoint", "--name", "baseline"], "")
        .status
        .success());
    repository.write(".crane/policies/payments.crane", &format!("{POLICY}\n"));
    let after = repository.zones();
    assert_ne!(after["contract_version"], before["contract_version"]);
    assert_eq!(after["zone_set_version"], before["zone_set_version"]);
    for zone in before["zones"].as_array().unwrap() {
        let id = zone["zone_id"].as_str().unwrap();
        let now = find(&after["zones"], "zone_id", id);
        assert_eq!(now["version"], zone["version"], "{id}");
        assert_eq!(now["entities"], zone["entities"], "{id}");
        assert_eq!(now["effective_safety_state"], "active", "{id}");
    }
    assert_eq!(repository.read(".crane/zones/org.zone"), ZONES);
}

/** Zones never grant: a Routine, Autonomous zone over preserved code does not let an agent change
 * it, agents cannot edit zone files, a malformed zone file is reported without hiding valid
 * zones, and a repository without zones lists none
 */
#[test]
fn zones_never_grant_and_are_protected() {
    let open = "zone open {\n    criticality routine;\n    autonomy autonomous;\n    select symbol java:com.acme.payments.PaymentService.charge;\n}\n";
    let repository = Repository::new(open);
    assert_eq!(
        find(&repository.zones()["entities"], "id", CHARGE)["autonomy"],
        "autonomous"
    );
    let edit = json!({
        "session_id": "z1",
        "hook_event_name": "PreToolUse",
        "tool_name": "Edit",
        "tool_input": {"file_path": SERVICE, "old_string": "return fee(amount) + amount;", "new_string": "return amount;"},
    });
    let denied = repository.crane(
        &[
            "agent",
            "hook",
            "--event",
            "pre-tool-use",
            "--profile",
            "claude",
        ],
        &edit.to_string(),
    );
    assert_eq!(denied.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&denied.stderr)
        .contains("protected by preserve function PaymentService.charge"));
    let write = json!({
        "session_id": "z1",
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": ".crane/zones/open.zone", "content": "zone x {}"},
    });
    let zone_edit = repository.crane(
        &[
            "agent",
            "hook",
            "--event",
            "pre-tool-use",
            "--profile",
            "claude",
        ],
        &write.to_string(),
    );
    assert_eq!(zone_edit.status.code(), Some(2));

    repository.write(
        ".crane/zones/broken.zone",
        "zone broken {\n    criticality red;\n}\n",
    );
    let output = repository.crane(&["zones", "--json"], "");
    assert!(!output.status.success());
    let zones: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(zones["problems"][0]
        .as_str()
        .unwrap()
        .contains("zones/broken.zone: line 2: invalid criticality 'red'"));
    find(&zones["zones"], "zone_id", "open");

    fs::remove_dir_all(repository.root.join(".crane/zones")).unwrap();
    let empty = repository.crane(&["zones"], "");
    assert!(empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stdout).contains("No zones defined"));
}
