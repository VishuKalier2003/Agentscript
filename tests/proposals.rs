use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/** Counter that keeps fixture directory names unique when tests run in parallel */
static REPOSITORIES: AtomicUsize = AtomicUsize::new(0);

/** Environment variables that mark a process as running for an agent */
const AGENT_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CRANE_AGENT",
];

/** Repository files: payments in Java with a test and owners, auth in Python with a widely used
 * helper and a key, migrations, infrastructure, and configuration */
const FILES: &[(&str, &str)] = &[
    ("README.md", "# Demo\n"),
    (".github/CODEOWNERS", "* @acme/core\n/services/payments/ @acme/payments\n"),
    ("services/payments/pom.xml", "<project/>\n"),
    (
        "services/payments/src/main/java/com/acme/payments/PaymentService.java",
        "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    public int refund(int amount) {\n        return charge(-amount);\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n",
    ),
    (
        "services/payments/src/test/java/com/acme/payments/PaymentServiceTest.java",
        "package com.acme.payments;\n\nclass PaymentServiceTest {\n    @Test\n    void charges() {\n        new PaymentService().charge(5);\n    }\n}\n",
    ),
    (
        "auth/login.py",
        "API_KEY = 'x'\n\n\ndef authenticate(user):\n    return check(user)\n\n\ndef check(value):\n    return value\n",
    ),
    (
        "app/views.py",
        "def a():\n    return check(1)\n\n\ndef b():\n    return check(2)\n\n\ndef c():\n    return check(3)\n\n\ndef d():\n    return check(4)\n\n\ndef e():\n    return check(5)\n",
    ),
    ("app/report.py", "def summarize(rows):\n    return len(rows)\n"),
    ("db/migrations/001_init.sql", "create table t (id int);\n"),
    ("infra/main.tf", "resource \"x\" \"y\" {}\n"),
    ("config/application-prod.yml", "db: prod\n"),
    ("config/secrets.yml", "key: 1\n"),
];

/** The organization says Payments is Critical */
const ZONES: &str = "zone payments {\n    criticality critical;\n    autonomy assisted;\n    select subsystem payments;\n}\n";

/** A temporary repository with Crane initialized, a baseline checkpoint, and zones, removed on
 * drop
 * Fields
    - root: PathBuf - repository root
*/
struct Repository {
    root: PathBuf,
}

impl Repository {
    /** Create the fixture repository
     * Input
        - initialize: bool - run crane init and checkpoint and write the zones
     * Output
        - Repository
    */
    fn new(initialize: bool) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-proposals-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Self { root };
        for (path, content) in FILES {
            repository.write(path, content);
        }
        repository.git(&["init", "-q"]);
        repository.git(&["config", "user.email", "reviewer@example.com"]);
        repository.git(&["config", "user.name", "Crane Proposals Test"]);
        repository.git(&["config", "core.autocrlf", "false"]);
        repository.git(&["add", "."]);
        repository.git(&["commit", "-qm", "baseline"]);
        if initialize {
            assert!(repository.human(&["init"]).status.success());
            assert!(repository
                .human(&["checkpoint", "--name", "baseline"])
                .status
                .success());
            repository.write(".crane/zones/org.zone", ZONES);
        }
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
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /** Run crane with the given environment additions and stdin, with every agent marker removed
     * first so the run is a human's unless a marker is added back
     * Input
        - args: &[&str] - crane arguments
        - environment: &[(&str, &str)] - variables to set
        - stdin: &str - text written to stdin
     * Output
        - Output of the finished process
    */
    fn run(&self, args: &[&str], environment: &[(&str, &str)], stdin: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
        command
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for marker in AGENT_MARKERS {
            command.env_remove(marker);
        }
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("crane should execute");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /** Run crane as a human
     * Input
        - args: &[&str] - crane arguments
     * Output
        - Output
    */
    fn human(&self, args: &[&str]) -> Output {
        self.run(args, &[], "")
    }

    /** Run crane as a human and parse its JSON output, requiring success
     * Input
        - args: &[&str] - crane arguments
     * Output
        - Value
    */
    fn json(&self, args: &[&str]) -> Value {
        let output = self.human(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("JSON output")
    }

    /** Read a proposal's JSON record
     * Input
        - name: &str - proposal id
     * Output
        - Value
    */
    fn proposal(&self, name: &str) -> Value {
        self.json(&["policy", "show", name, "--json"])
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

/** Decode process output as text
 * Input
    - bytes: &[u8] - stdout or stderr
 * Output
    - String
*/
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
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

/** Return the first 12 characters of a proposal's policy digest, as an approver quotes them
 * Input
    - proposal: &Value - proposal record
 * Output
    - String
*/
fn confirm(proposal: &Value) -> String {
    let digest = proposal["policy_digest"].as_str().unwrap();
    digest["sha256:".len().."sha256:".len() + 12].to_string()
}

/** Ask the Claude PreToolUse hook about a tool call in a session
 * Input
    - repository: &Repository - repository
    - session: &str - Claude session id
    - payload: Value - tool_name and tool_input
 * Output
    - Output of the hook process
*/
fn hook(repository: &Repository, session: &str, mut payload: Value) -> Output {
    payload["session_id"] = json!(session);
    payload["hook_event_name"] = json!("PreToolUse");
    repository.run(
        &[
            "agent",
            "hook",
            "--event",
            "pre-tool-use",
            "--profile",
            "claude",
        ],
        &[],
        &payload.to_string(),
    )
}

/** An Edit of the charge method, which the approved policy preserves */
fn charge_edit() -> Value {
    json!({"tool_name": "Edit", "tool_input": {
        "file_path": "services/payments/src/main/java/com/acme/payments/PaymentService.java",
        "old_string": "return fee(amount) + amount;",
        "new_string": "return amount;",
    }})
}

/** The expected candidate policy for the fixture */
const EXPECTED_POLICY: &str = "policy payments_guard {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n    preserve --function PaymentService.fee;\n    preserve --function PaymentService.refund;\n    preserve --data API_KEY;\n    preserve --function authenticate;\n    preserve --function check;\n}\n";

/** Policy discovery explains every candidate (reason, confidence, suggested rule or zone,
 * affected entities), is deterministic across runs and across identical repositories, works
 * without crane init, and enforces nothing
 */
#[test]
fn policy_discovery_is_deterministic_and_explained() {
    let first = Repository::new(true);
    let second = Repository::new(true);
    let check_before = first.human(&["check", "--json"]);
    let discovered = first.json(&["discover", "--policies", "--json"]);
    assert_eq!(discovered["advisory"], true);
    assert!(discovered["heuristics"].as_array().unwrap().len() >= 5);
    assert_eq!(
        first.json(&["discover", "--policies", "--json"]),
        discovered
    );
    assert_eq!(
        second.json(&["discover", "--policies", "--json"]),
        discovered
    );

    let candidates = &discovered["candidates"];
    let charge = find(
        candidates,
        "id",
        "symbol:java:com.acme.payments.PaymentService.charge",
    );
    assert_eq!(charge["candidate"], "PaymentService.charge");
    assert_eq!(charge["confidence"], "high");
    assert_eq!(charge["suggestion"], "preserve");
    assert_eq!(
        charge["suggested_rule"],
        "preserve --function PaymentService.charge;"
    );
    assert_eq!(
        charge["affected_entities"],
        json!(["symbol:java:com.acme.payments.PaymentService.charge"])
    );
    let reason = charge["reason"].as_str().unwrap();
    assert!(
        reason.starts_with(
            "criticality=Critical (zone payments) + payment name 'charge' + payment subsystem"
        ),
        "{reason}"
    );
    assert!(
        reason.contains("tested by symbol:java:com.acme.payments.PaymentServiceTest.charges"),
        "{reason}"
    );
    assert!(reason.contains("owners @acme/payments"), "{reason}");

    let key = find(candidates, "id", "symbol:python:auth.login.API_KEY");
    assert_eq!(
        (key["confidence"].as_str(), key["suggested_rule"].as_str()),
        (Some("high"), Some("preserve --data API_KEY;"))
    );
    let check = find(candidates, "id", "symbol:python:auth.login.check");
    assert_eq!(check["confidence"], "medium");
    assert!(check["reason"]
        .as_str()
        .unwrap()
        .contains("6 callers from 1 other modules"));
    let secrets = find(candidates, "id", "region:secrets_config:config/secrets.yml");
    assert_eq!(secrets["confidence"], "high");
    assert_eq!(secrets["suggested_rule"], Value::Null);
    assert!(secrets["suggested_zone"]
        .as_str()
        .unwrap()
        .contains("select path config/secrets.yml;"));
    for region in [
        "region:migration:db/migrations",
        "region:infrastructure:infra",
        "region:production_config:config/application-prod.yml",
    ] {
        assert_eq!(
            find(candidates, "id", region)["confidence"],
            "medium",
            "{region}"
        );
    }
    assert!(candidates
        .as_array()
        .unwrap()
        .iter()
        .all(|candidate| !candidate["id"].as_str().unwrap().contains("summarize")));
    assert!(candidates
        .as_array()
        .unwrap()
        .iter()
        .all(|candidate| !candidate["id"].as_str().unwrap().contains("Test")));
    // Confidence never rises along the list
    let order = candidates
        .as_array()
        .unwrap()
        .iter()
        .map(
            |candidate| match candidate["confidence"].as_str().unwrap() {
                "high" => 2,
                "medium" => 1,
                _ => 0,
            },
        )
        .collect::<Vec<_>>();
    assert!(order.windows(2).all(|pair| pair[0] >= pair[1]), "{order:?}");

    // Advisory: nothing was written to the policies and verification is unchanged
    assert_eq!(
        fs::read_dir(first.root.join(".crane/policies"))
            .unwrap()
            .count(),
        0
    );
    let check_after = first.human(&["check", "--json"]);
    assert_eq!(check_after.stdout, check_before.stdout);
    let summary = text(&first.human(&["discover", "--policies"]).stdout);
    assert!(summary.contains("high confidence ("), "{summary}");
    assert!(
        summary.contains("suggested rule: preserve --function PaymentService.charge;"),
        "{summary}"
    );

    // Without crane init there are no zone signals, but discovery still works
    let bare = Repository::new(false);
    let plain = bare.json(&["discover", "--policies", "--json"]);
    let charge = find(
        &plain["candidates"],
        "id",
        "symbol:java:com.acme.payments.PaymentService.charge",
    );
    assert!(!charge["reason"].as_str().unwrap().contains("criticality"));
    assert_eq!(charge["confidence"], "high");
}

/** crane policy propose writes deterministic candidate AgentScript and a reviewable record next
 * to it, never into .crane/policies, so nothing is enforced
 */
#[test]
fn propose_writes_candidate_agentscript_without_activating() {
    let first = Repository::new(true);
    let second = Repository::new(true);
    let created = first.json(&["policy", "propose", "--name", "payments_guard", "--json"]);
    let again = second.json(&["policy", "propose", "--name", "payments_guard", "--json"]);
    assert_eq!(created["policy"], EXPECTED_POLICY);
    assert_eq!(created["policy"], again["policy"]);
    assert_eq!(created["policy_digest"], again["policy_digest"]);
    assert_eq!(created["candidates"], again["candidates"]);
    assert_eq!(created["zone_suggestions"], again["zone_suggestions"]);

    assert_eq!(created["proposal_format"], 1);
    assert_eq!(created["status"], "pending");
    assert_eq!(created["active"], false);
    assert_eq!(created["revision"], 1);
    assert_eq!(
        created["source"]["generated_in_agent_environment"],
        Value::Null
    );
    assert_eq!(created["history"][0]["action"], "generated");
    let charge = find(&created["candidates"], "candidate", "PaymentService.charge");
    assert_eq!(charge["included"], true);
    assert_eq!(
        find(&created["candidates"], "candidate", "config/secrets.yml")["included"],
        false
    );
    assert_eq!(created["zone_suggestions"].as_array().unwrap().len(), 4);

    assert_eq!(
        first.read(".crane/proposals/payments_guard.crane"),
        EXPECTED_POLICY
    );
    assert!(!first
        .root
        .join(".crane/policies/payments_guard.crane")
        .exists());
    let parsed = first.human(&["parse", ".crane/proposals/payments_guard.crane"]);
    assert!(parsed.status.success(), "{}", text(&parsed.stderr));
    let check = first.json(&["check", "--json"]);
    assert_eq!(check["violations"], json!([]));

    let duplicate = first.human(&["policy", "propose", "--name", "payments_guard"]);
    assert!(!duplicate.status.success());
    assert!(text(&duplicate.stderr).contains("already exists (pending)"));
    let high_only = first.json(&[
        "policy",
        "propose",
        "--name",
        "strict",
        "--min-confidence",
        "high",
        "--json",
    ]);
    assert!(!high_only["policy"]
        .as_str()
        .unwrap()
        .contains("--function check;"));
    let listing = text(&first.human(&["policy", "proposals"]).stdout);
    assert!(
        listing.contains("payments_guard pending revision 1 sha256:"),
        "{listing}"
    );
    assert!(!first
        .human(&["policy", "propose", "--checkpoint", "missing"])
        .status
        .success());
}

/** Approval activates exactly the reviewed policy: it needs a named approver and the start of
 * the policy digest, refuses an unrecorded edit, accepts a recorded edit only with the new
 * digest, and writes the policy to .crane/policies, after which it is enforced
 */
#[test]
fn approval_activates_only_the_reviewed_policy() {
    let repository = Repository::new(true);
    let created = repository.json(&["policy", "propose", "--name", "payments_guard", "--json"]);
    let original = confirm(&created);
    assert_eq!(original.len(), 12);
    // A proposal is not enforced: an agent session started now may still change charge
    assert!(hook(&repository, "before", charge_edit()).status.success());

    let no_approver = repository.human(&[
        "policy",
        "approve",
        "payments_guard",
        "--confirm",
        &original,
    ]);
    assert!(text(&no_approver.stderr).contains("requires --approver"));
    let wrong = repository.human(&[
        "policy",
        "approve",
        "payments_guard",
        "--approver",
        "alice",
        "--confirm",
        "000000000000",
    ]);
    assert!(text(&wrong.stderr).contains("requires --confirm"));
    let short = repository.human(&[
        "policy",
        "approve",
        "payments_guard",
        "--approver",
        "alice",
        "--confirm",
        &original[..6],
    ]);
    assert!(!short.status.success());

    // An edit that was not recorded is never activated
    let edited = EXPECTED_POLICY.replace("    preserve --function check;\n", "");
    repository.write(".crane/proposals/payments_guard.crane", &edited);
    let unrecorded = repository.human(&[
        "policy",
        "approve",
        "payments_guard",
        "--approver",
        "alice",
        "--confirm",
        &original,
    ]);
    assert!(text(&unrecorded.stderr).contains("changed after the proposal was recorded"));
    assert!(!repository
        .root
        .join(".crane/policies/payments_guard.crane")
        .exists());

    let recorded = repository.human(&["policy", "edit", "payments_guard", "--by", "alice"]);
    assert!(recorded.status.success(), "{}", text(&recorded.stderr));
    let revised = repository.proposal("payments_guard");
    assert_eq!(revised["revision"], 2);
    assert_eq!(revised["policy"], edited);
    assert_eq!(revised["history"][1]["action"], "edited");
    assert_eq!(
        revised["history"][1]["detail"]["previous_digest"],
        created["policy_digest"]
    );
    let stale = repository.human(&[
        "policy",
        "approve",
        "payments_guard",
        "--approver",
        "alice",
        "--confirm",
        &original,
    ]);
    assert!(!stale.status.success(), "the old digest no longer matches");

    let approved = repository.human(&[
        "policy",
        "approve",
        "payments_guard",
        "--approver",
        "alice",
        "--confirm",
        &confirm(&revised),
    ]);
    assert!(approved.status.success(), "{}", text(&approved.stderr));
    assert_eq!(
        repository.read(".crane/policies/payments_guard.crane"),
        edited
    );
    let active = repository.proposal("payments_guard");
    assert_eq!(active["status"], "approved");
    assert_eq!(active["active"], true);
    assert_eq!(active["activation"]["approver"], "alice");
    assert_eq!(active["activation"]["git_user"], "reviewer@example.com");
    assert_eq!(
        active["activation"]["policy_digest"],
        revised["policy_digest"]
    );
    let check = repository.json(&["check", "--json"]);
    assert_eq!(check["status"], "passed", "{check}");
    // Activated: a new session is denied, a session bound before activation keeps its contract
    let denied = hook(&repository, "after", charge_edit());
    assert_eq!(denied.status.code(), Some(2));
    assert!(text(&denied.stderr).contains("protected by preserve function PaymentService.charge"));
    assert!(hook(&repository, "before", charge_edit()).status.success());

    for args in [
        vec![
            "policy",
            "approve",
            "payments_guard",
            "--approver",
            "alice",
            "--confirm",
            &confirm(&revised),
        ],
        vec!["policy", "regenerate", "payments_guard"],
        vec!["policy", "reject", "payments_guard", "--approver", "alice"],
    ] {
        assert!(!repository.human(&args).status.success(), "{args:?}");
    }
    let invalid = repository.human(&["policy", "edit", "payments_guard"]);
    assert!(text(&invalid.stderr).contains("only pending proposals"));
}

/** Reject and regenerate: rejection needs an approver and blocks approval; regeneration makes a
 * new pending revision, identical while the repository is unchanged and different after a change;
 * invalid edits are refused
 */
#[test]
fn reject_regenerate_and_edit_validation() {
    let repository = Repository::new(true);
    let created = repository.json(&["policy", "propose", "--name", "payments_guard", "--json"]);
    repository.write(
        ".crane/proposals/payments_guard.crane",
        "policy payments_guard {\n    preserve --function x\n}\n",
    );
    assert!(text(
        &repository
            .human(&["policy", "edit", "payments_guard"])
            .stderr
    )
    .contains("edited policy is invalid"));
    repository.write(
        "other.crane",
        &EXPECTED_POLICY.replace("payments_guard", "other"),
    );
    let renamed = repository.human(&["policy", "edit", "payments_guard", "--file", "other.crane"]);
    assert!(text(&renamed.stderr).contains("is named other"));
    repository.write(".crane/proposals/payments_guard.crane", EXPECTED_POLICY);

    assert!(text(
        &repository
            .human(&["policy", "reject", "payments_guard"])
            .stderr
    )
    .contains("requires --approver"));
    let rejected = repository.human(&[
        "policy",
        "reject",
        "payments_guard",
        "--approver",
        "bob",
        "--reason",
        "too broad",
    ]);
    assert!(rejected.status.success(), "{}", text(&rejected.stderr));
    let record = repository.proposal("payments_guard");
    assert_eq!(record["status"], "rejected");
    assert_eq!(
        record["rejection"],
        json!({"by": "bob", "reason": "too broad"})
    );
    let approve = repository.human(&[
        "policy",
        "approve",
        "payments_guard",
        "--approver",
        "bob",
        "--confirm",
        &confirm(&created),
    ]);
    assert!(text(&approve.stderr).contains("is rejected"));

    let regenerated = repository.human(&["policy", "regenerate", "payments_guard", "--by", "bob"]);
    assert!(
        regenerated.status.success(),
        "{}",
        text(&regenerated.stderr)
    );
    let record = repository.proposal("payments_guard");
    assert_eq!(
        (record["status"].as_str(), record["revision"].as_u64()),
        (Some("pending"), Some(2))
    );
    assert_eq!(record["policy_digest"], created["policy_digest"]);
    assert_eq!(record["history"][2]["detail"]["policy_changed"], false);

    repository.write(
        "auth/login.py",
        &repository
            .read("auth/login.py")
            .replace("authenticate", "verify_user"),
    );
    assert!(repository
        .human(&["policy", "regenerate", "payments_guard"])
        .status
        .success());
    let record = repository.proposal("payments_guard");
    assert_eq!(record["revision"], 3);
    assert_ne!(record["policy_digest"], created["policy_digest"]);
    assert!(!record["policy"].as_str().unwrap().contains("authenticate"));
    assert_eq!(
        repository.read(".crane/proposals/payments_guard.crane"),
        record["policy"].as_str().unwrap()
    );
}

/** An agent cannot review or activate proposals: in an agent environment approve, reject, edit,
 * and regenerate refuse to run, the pre-tool hook denies those commands and writes to the
 * proposal and policy files, and a proposal the agent generates is marked as such
 */
#[test]
fn agents_cannot_review_or_activate_proposals() {
    let repository = Repository::new(true);
    for marker in ["CLAUDECODE", "CODEX_SANDBOX", "CRANE_AGENT"] {
        let name = format!("agent_{}", marker.to_ascii_lowercase());
        let agent = [(marker, "1")];
        let generated = repository.run(
            &["policy", "propose", "--name", &name, "--json"],
            &agent,
            "",
        );
        assert!(generated.status.success(), "{}", text(&generated.stderr));
        let record: Value = serde_json::from_slice(&generated.stdout).unwrap();
        assert_eq!(record["source"]["generated_in_agent_environment"], marker);
        assert!(record["history"][0]["actor"]
            .as_str()
            .unwrap()
            .contains(marker));
        let digest = confirm(&record);
        for args in [
            vec![
                "policy",
                "approve",
                name.as_str(),
                "--approver",
                "agent",
                "--confirm",
                digest.as_str(),
            ],
            vec!["policy", "reject", name.as_str(), "--approver", "agent"],
            vec!["policy", "edit", name.as_str()],
            vec!["policy", "regenerate", name.as_str()],
        ] {
            let refused = repository.run(&args, &agent, "");
            assert_eq!(refused.status.code(), Some(1), "{args:?}");
            assert!(
                text(&refused.stderr).contains("refuses to run in an agent environment"),
                "{args:?}"
            );
        }
        assert!(!repository
            .root
            .join(format!(".crane/policies/{name}.crane"))
            .exists());
        let show = text(&repository.human(&["policy", "show", &name]).stdout);
        assert!(show.contains("Generated in an agent environment"), "{show}");
    }

    let hook = |payload: Value| hook(&repository, "p1", payload);
    for command in [
        "crane policy approve agent_claudecode --approver me --confirm 123456789012",
        "crane.exe policy reject agent_claudecode --approver me",
        "crane policy edit agent_claudecode",
        "cd x && crane policy regenerate agent_claudecode",
    ] {
        let denied = hook(json!({"tool_name": "Bash", "tool_input": {"command": command}}));
        assert_eq!(denied.status.code(), Some(2), "{command}");
    }
    for path in [
        ".crane/proposals/agent_claudecode.crane",
        ".crane/policies/agent_claudecode.crane",
    ] {
        let denied =
            hook(json!({"tool_name": "Write", "tool_input": {"file_path": path, "content": "x"}}));
        assert_eq!(denied.status.code(), Some(2), "{path}");
    }
    let read = hook(
        json!({"tool_name": "Bash", "tool_input": {"command": "crane policy show agent_claudecode"}}),
    );
    assert!(read.status.success(), "{}", text(&read.stderr));
}
