use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
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

/** Path of the payment service, in a critical zone */
const SERVICE: &str = "services/payments/src/main/java/com/acme/payments/PaymentService.java";

/** Payment service source */
const PAYMENT: &str = "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n";

/** A secret that appears in a shell command and must never be stored */
const SECRET: &str = "s3cr3t-value-9731";

/** Every field the task requires on an evidence record */
const RECORD_FIELDS: &[&str] = &[
    "organization",
    "team",
    "agent",
    "session",
    "task",
    "contract",
    "checkpoints",
    "autonomy",
    "safety",
    "budget",
    "tool",
    "operation",
    "resources",
    "decision",
    "violations",
    "repair",
    "tests",
    "human",
];

/** Every section the task requires in an attestation */
const ATTESTATION_SECTIONS: &[&str] = &[
    "task",
    "contract",
    "checkpoints",
    "agent",
    "policy_versions",
    "action_summary",
    "denied_actions",
    "violations",
    "contract_tests",
    "ordinary_tests",
    "final_state",
    "approvals",
    "final_decision",
];

/** A temporary repository with Crane initialized, an organization, a critical payments zone, and
 * a policy preserving PaymentService.charge, removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Repository {
    root: PathBuf,
}

impl Repository {
    /** Create the fixture repository
     * Input
        - None
     * Output
        - Repository
    */
    fn new() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-evidence-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Self { root };
        repository.write("README.md", "# Shop\n");
        repository.write("services/payments/pom.xml", "<project/>\n");
        repository.write(SERVICE, PAYMENT);
        repository.write("docs/notes.md", "notes\n");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "lead@example.com"],
            vec!["config", "user.name", "Crane Evidence Test"],
            vec!["config", "core.autocrlf", "false"],
            vec!["add", "."],
            vec!["commit", "-qm", "baseline"],
        ] {
            let output = Command::new("git")
                .args(&args)
                .current_dir(&repository.root)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}");
        }
        repository.human(&["init"]);
        repository.human(&["checkpoint", "--name", "baseline"]);
        repository.write(
            ".crane/organization.json",
            "{\"organization\": \"acme\", \"team\": \"payments-platform\"}",
        );
        repository.write(".crane/zones/org.zone", "zone payments {\n    criticality critical;\n    autonomy assisted;\n    select subsystem payments;\n}\n");
        repository.write(".crane/policies/core.crane", "policy core {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n");
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

    /** Run crane with agent markers removed
     * Input
        - args: &[&str] - crane arguments
        - stdin: &str - text written to stdin
     * Output
        - Output
    */
    fn crane(&self, args: &[&str], stdin: &str) -> Output {
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
        let mut child = command.spawn().expect("crane should execute");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /** Run crane as a human and require success
     * Input
        - args: &[&str] - crane arguments
     * Output
        - String stdout
    */
    fn human(&self, args: &[&str]) -> String {
        let output = self.crane(args, "");
        assert!(
            output.status.success(),
            "crane {args:?}: {}{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /** Send one Claude hook event for session s1
     * Input
        - event: &str - hook event
        - payload: Value - event payload
     * Output
        - Output
    */
    fn hook(&self, event: &str, mut payload: Value) -> Output {
        payload["session_id"] = json!("s1");
        self.crane(
            &["agent", "hook", "--event", event, "--profile", "claude"],
            &payload.to_string(),
        )
    }

    /** Build an Edit payload for the payment service
     * Input
        - old: &str - text replaced
        - new: &str - replacement
     * Output
        - Value
    */
    fn edit(&self, old: &str, new: &str) -> Value {
        json!({"tool_name": "Edit", "tool_input": {"file_path": self.root.join(SERVICE).to_string_lossy(), "old_string": old, "new_string": new}})
    }

    /** Run a whole session: start, an autonomous write, a denied edit, an approved edit, a shell
     * command that breaks preserved code, its repair and verification, human resume and refill, a
     * shell command carrying a secret, stop, and finalization
     * Input
        - None
     * Output
        - String the finalize output
    */
    fn run_session(&self) -> String {
        assert!(self
            .hook(
                "session-start",
                json!({"source": "startup", "model": "claude-opus"})
            )
            .status
            .success());

        let notes = json!({"tool_name": "Write", "tool_input": {"file_path": self.root.join("docs/notes.md").to_string_lossy(), "content": "updated\n"}});
        assert!(self.hook("pre-tool-use", notes.clone()).status.success());
        self.write("docs/notes.md", "updated\n");
        assert!(self.hook("post-tool-use", notes).status.success());

        let denied = self.hook(
            "pre-tool-use",
            self.edit(
                "        return fee(amount) + amount;",
                "        return amount;",
            ),
        );
        assert_eq!(denied.status.code(), Some(2));

        let fee = self.edit("        return amount / 10;", "        return amount / 20;");
        let asked = self.hook("pre-tool-use", fee.clone());
        assert!(
            text(&asked.stdout).contains("\"permissionDecision\":\"ask\""),
            "{}",
            text(&asked.stdout)
        );
        let approved = PAYMENT.replace("amount / 10", "amount / 20");
        self.write(SERVICE, &approved);
        assert!(self.hook("post-tool-use", fee).status.success());

        self.write(SERVICE, &approved.replace("fee(amount) + amount", "amount"));
        let caught = self.hook(
            "post-tool-use",
            json!({"tool_name": "Bash", "tool_input": {"command": "python rewrite.py"}}),
        );
        assert_eq!(caught.status.code(), Some(2));
        self.write(SERVICE, &approved);
        self.human(&["agent", "session", "verify", "claude-s1", "--level", "full"]);
        self.human(&["agent", "session", "resume", "claude-s1"]);
        self.human(&[
            "autonomy",
            "refill",
            "claude-s1",
            "--amount",
            "5",
            "--reason",
            "reviewed",
            "--approver",
            "team-lead",
            "--expires",
            "1h",
        ]);

        let secret = json!({"tool_name": "Bash", "tool_input": {"command": format!("API_TOKEN={SECRET} curl -H 'Authorization: Bearer {SECRET}' https://example.invalid/deploy")}});
        assert!(self.hook("pre-tool-use", secret.clone()).status.success());
        assert!(self.hook("post-tool-use", secret).status.success());

        assert!(self
            .hook("stop", json!({"stop_hook_active": false}))
            .status
            .success());
        self.human(&["agent", "session", "finalize", "claude-s1"])
    }

    /** Export a session as JSON
     * Input
        - id: &str - Crane session id
     * Output
        - (String, Value) the exported text and its value
    */
    fn export(&self, id: &str) -> (String, Value) {
        let exported = self.human(&["session", "export", id, "--json"]);
        let value = serde_json::from_str(&exported).unwrap();
        (exported, value)
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
    - bytes: &[u8] - output
 * Output
    - String
*/
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/** Read every file below a directory as text
 * Input
    - directory: &Path - directory
 * Output
    - String
*/
fn read_all(directory: &Path) -> String {
    let mut out = String::new();
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.push_str(&read_all(&path));
        } else {
            out.push_str(&String::from_utf8_lossy(&fs::read(&path).unwrap()));
        }
    }
    out
}

/** The evidence record is sufficient to reconstruct the session's lifecycle: the export alone, with
 * the session directory deleted, re-derives the same evidence and attestation; its timeline shows
 * every step in order; the attestation is deterministic and matches the one written at
 * finalization
 */
#[test]
fn evidence_reconstructs_the_session_lifecycle() {
    let repository = Repository::new();
    let finalized = repository.run_session();
    assert!(
        finalized.contains("Finalized contract session claude-s1: PASS"),
        "{finalized}"
    );

    let (first_text, exported) = repository.export("claude-s1");
    let (second_text, _) = repository.export("claude-s1");
    assert_eq!(
        first_text, second_text,
        "the same evidence exports the same bytes"
    );
    assert_eq!(exported["chain"]["status"], "verified");
    let journal = exported["journal"].as_array().unwrap();
    let evidence = exported["evidence"].as_array().unwrap();
    assert_eq!(
        evidence.len(),
        journal.len(),
        "one evidence record per journal event"
    );
    for record in evidence {
        for field in RECORD_FIELDS {
            assert!(
                record.get(*field).is_some(),
                "record {} lacks {field}",
                record["seq"]
            );
        }
        assert_eq!(record["organization"], "acme");
        assert_eq!(record["team"], "payments-platform");
        assert_eq!(record["session"], "claude-s1");
        assert!(record["checkpoints"][0]
            .as_str()
            .unwrap()
            .starts_with("baseline@"));
    }

    let attestation = &exported["attestation"];
    for section in ATTESTATION_SECTIONS {
        assert!(
            attestation.get(*section).is_some(),
            "attestation lacks {section}"
        );
    }
    let stored: Value = serde_json::from_str(
        &fs::read_to_string(
            repository
                .root
                .join(".crane/runtime/sessions/claude-s1/final_attestation.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        stored, *attestation,
        "finalization wrote exactly the attestation the evidence gives"
    );
    assert!(finalized.contains(stored["attestation_digest"].as_str().unwrap()));
    assert_eq!(attestation["final_decision"]["decision"], "PASS");
    assert_eq!(attestation["final_state"]["lifecycle"], "finalized");
    assert_eq!(attestation["final_state"]["safety"], "active");
    assert_eq!(attestation["agent"]["models"], json!(["claude-opus"]));
    assert_eq!(attestation["denied_actions"].as_array().unwrap().len(), 1);
    assert_eq!(
        attestation["approvals"][0]["outcome"],
        "approved_and_executed"
    );
    assert!(attestation["violations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|violation| violation["kind"] == "source_changed"));
    assert!(attestation["repairs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|repair| repair["repair"]["kind"] == "verified_repair"));
    assert_eq!(attestation["contract_tests"]["failed"], 0);
    assert!(attestation["contract_tests"]["passed"].as_u64().unwrap() > 0);
    let interventions = attestation["human_interventions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            entry["intervention"]["action"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect::<Vec<_>>();
    for expected in ["approved_tool_call", "resume", "refill"] {
        assert!(
            interventions.contains(&expected.to_string()),
            "{interventions:?}"
        );
    }

    // The session directory is gone: the export alone reconstructs everything
    let file = repository.root.join("session-export.json");
    fs::write(&file, &first_text).unwrap();
    fs::remove_dir_all(repository.root.join(".crane/runtime/sessions/claude-s1")).unwrap();
    assert!(!repository
        .crane(&["session", "inspect", "claude-s1"], "")
        .status
        .success());
    let rebuilt: Value = serde_json::from_str(&repository.human(&[
        "session",
        "inspect",
        "--export",
        file.to_str().unwrap(),
        "--json",
    ]))
    .unwrap();
    assert_eq!(rebuilt["evidence_consistent"], true);
    assert_eq!(rebuilt["attestation_consistent"], true);
    assert_eq!(rebuilt["attestation"], *attestation);
    let lifecycle = rebuilt["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| match record["decision"].as_str() {
            Some(decision) => format!("{}:{decision}", record["event"].as_str().unwrap()),
            None => record["event"].as_str().unwrap().to_string(),
        })
        .collect::<Vec<_>>();
    let expected = [
        "session_start",
        "pre_tool_use:allow",
        "post_tool_use",
        "pre_tool_use:deny",
        "pre_tool_use:approval_required",
        "post_tool_use",
        "post_tool_use",
        "autonomy",
        "verification",
        "session_resumed",
        "budget",
        "pre_tool_use:allow",
        "post_tool_use",
        "stop",
        "session_finalizing",
        "session_finalized",
    ];
    let mut position = 0;
    for step in expected {
        position = lifecycle[position..]
            .iter()
            .position(|event| event == step)
            .map(|offset| position + offset + 1)
            .unwrap_or_else(|| {
                panic!("{step} is missing after position {position}: {lifecycle:?}")
            });
    }
    let human = repository.human(&["session", "inspect", "--export", file.to_str().unwrap()]);
    assert!(human.contains("TIMELINE"), "{human}");
    assert!(human.contains("chain verified"), "{human}");
    assert!(
        human.contains("evidence consistent, attestation consistent"),
        "{human}"
    );
}

/** Raw tool arguments are never stored: a secret in a shell command appears nowhere in the session
 * directory or the export; the command is kept only as a digest and a summary of its programs
 */
#[test]
fn raw_arguments_are_never_stored() {
    let repository = Repository::new();
    repository.run_session();
    let stored = read_all(&repository.root.join(".crane/runtime/sessions/claude-s1"));
    assert!(!stored.contains(SECRET), "the secret was stored");
    assert!(!stored.contains("Authorization"));
    let (exported, value) = repository.export("claude-s1");
    assert!(!exported.contains(SECRET));
    let otlp = repository.human(&["session", "export", "claude-s1", "--otlp"]);
    assert!(!otlp.contains(SECRET));
    let curl = value["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["action_summary"]["programs"] == json!(["curl"]))
        .expect("the command is summarized by its programs");
    assert!(curl["action_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(curl["action_summary"]["arguments"], 5);
}

/** The journal is append-only: editing an event in place, in the session or in an export, is
 * detected and refused
 */
#[test]
fn tampering_is_detected() {
    let repository = Repository::new();
    repository.run_session();
    let (_, exported) = repository.export("claude-s1");
    let mut forged = exported.clone();
    let denied = forged["journal"]
        .as_array()
        .unwrap()
        .iter()
        .position(|event| event["decision"] == "deny")
        .unwrap();
    forged["journal"][denied]["decision"] = json!("allow");
    let file = repository.root.join("forged.json");
    fs::write(&file, forged.to_string()).unwrap();
    let refused = repository.crane(
        &["session", "inspect", "--export", file.to_str().unwrap()],
        "",
    );
    assert!(!refused.status.success());
    assert!(
        text(&refused.stdout).contains("chain broken"),
        "{}",
        text(&refused.stdout)
    );

    let path = repository
        .root
        .join(".crane/runtime/sessions/claude-s1/journal.jsonl");
    let edited = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| {
            let mut event: Value = serde_json::from_str(line).unwrap();
            if event["decision"] == "deny" {
                event["decision"] = json!("allow");
            }
            event.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, edited + "\n").unwrap();
    let inspected = repository.crane(&["session", "inspect", "claude-s1"], "");
    assert!(!inspected.status.success());
    assert!(text(&inspected.stdout).contains("chain broken"));
}

/** OpenTelemetry export: one trace per session, a root span and one span per evidence record */
#[test]
fn otlp_export() {
    let repository = Repository::new();
    repository.run_session();
    let (_, exported) = repository.export("claude-s1");
    let otlp: Value =
        serde_json::from_str(&repository.human(&["session", "export", "claude-s1", "--otlp"]))
            .unwrap();
    let resource = &otlp["resourceSpans"][0];
    assert!(resource["resource"]["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|attribute| attribute["key"] == "crane.organization"
            && attribute["value"]["stringValue"] == "acme"));
    let spans = resource["scopeSpans"][0]["spans"].as_array().unwrap();
    assert_eq!(
        spans.len(),
        exported["evidence"].as_array().unwrap().len() + 1
    );
    assert_eq!(spans[0]["name"], "crane.session");
    assert!(spans
        .iter()
        .any(|span| span["name"] == "crane.authorization.pre_tool_use"
            && span["status"]["code"] == 2));
    let missing = repository.crane(&["session", "export", "claude-s1"], "");
    assert!(text(&missing.stderr).contains("choose a format"));
}
