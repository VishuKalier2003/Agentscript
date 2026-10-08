use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/** Counter that keeps fixture directory names unique when tests run in parallel */
static REPOSITORIES: AtomicUsize = AtomicUsize::new(0);

/** Environment variables that mark a process as an agent or bind a hook */
const CLEARED: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CRANE_AGENT",
    "CRANE_SESSION",
    "CRANE_TASK_ID",
];

/** Slack signing secret */
const SECRET: &str = "golden-path-signing-secret";

/** The payment service (it becomes a Critical zone) */
const PAYMENT: &str = "services/payments/src/main/java/com/acme/payments/PaymentService.java";

/** The catalog service the task changes (outside every zone) */
const INVOICE: &str = "services/catalog/src/main/java/com/acme/catalog/CatalogService.java";

/** The catalog change the task asks for */
const ROUNDED: &str = "return name.trim();";

/** A Codex session id */
const CODEX: &str = "0199a3c2-7d41-7e2a-b6c0-5f1e2d3c4b5a";

/** A Jira stand-in reached through the configured transport (the issue's status, comments, and
 * calls live in a state file; it can be made unavailable) */
const FAKE_JIRA: &str = r#"import json, sys
path = sys.argv[1]
state = json.load(open(path))
request = json.load(sys.stdin)
state["calls"].append(request["method"] + " " + request["path"])
def answer(status, body=None):
    json.dump(state, open(path, "w")); print(json.dumps({"status": status, "body": body})); sys.exit(0)
if not state["available"]:
    json.dump(state, open(path, "w")); sys.stderr.write("connection refused"); sys.exit(7)
if request["method"] == "GET" and request["path"].endswith("/comment"): answer(200, {"comments": state["comments"]})
if request["method"] == "GET": answer(200, {"fields": {"status": {"name": state["status"], "statusCategory": {"key": "done" if state["status"] == "Done" else "indeterminate"}}}})
if request["path"].endswith("/comment"):
    state["comments"].append({"id": str(len(state["comments"]) + 1), "body": request["body"]["body"]}); answer(201, {"id": str(len(state["comments"]))})
state["transitions"].append(request["body"]["transition"]["id"]); state["status"] = "Done"; answer(204)
"#;

/** Find a Python interpreter (the Jira stand-in and Slack's signature) */
fn python() -> String {
    for candidate in ["python3", "python"] {
        if Command::new(candidate)
            .args(["-c", "print(1)"])
            .output()
            .is_ok_and(|output| output.status.success() && output.stdout.starts_with(b"1"))
        {
            return candidate.into();
        }
    }
    panic!("these tests need python3 or python on PATH");
}

/** A GitHub repository (origin github.com/acme/shop) with payment and catalog code, Jira task
 * snapshots, a Slack-reviewed merge policy, and a Jira stand-in; nothing of Crane exists yet
 * Fields
    - root: PathBuf - repository root
    - side: PathBuf - folder outside the repository for the Jira stand-in and action files
*/
struct Repository {
    root: PathBuf,
    side: PathBuf,
}

impl Repository {
    /** Create the repository */
    fn new() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let number = REPOSITORIES.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("crane-golden-{suffix}-{number}"));
        let side = std::env::temp_dir().join(format!("crane-golden-side-{suffix}-{number}"));
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&side).unwrap();
        let repository = Self { root, side };
        repository.write("services/payments/pom.xml", "<project/>\n");
        repository.write(PAYMENT, "package com.acme.payments;\n\npublic class PaymentService {\n    public int charge(int amount) {\n        return fee(amount) + amount;\n    }\n\n    public int refund(int amount) {\n        return charge(-amount);\n    }\n\n    int fee(int amount) {\n        return amount / 10;\n    }\n}\n");
        repository.write("services/catalog/pom.xml", "<project/>\n");
        repository.write(INVOICE, "package com.acme.catalog;\n\npublic class CatalogService {\n    public String label(String name) {\n        return name;\n    }\n}\n");
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "lead@example.com"],
            vec!["config", "user.name", "Golden Path"],
            vec!["config", "core.autocrlf", "false"],
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/acme/shop.git",
            ],
            vec!["add", "."],
            vec!["commit", "-qm", "acme/shop baseline"],
        ] {
            repository.git(&args);
        }
        fs::write(repository.side.join("fake_jira.py"), FAKE_JIRA).unwrap();
        repository.jira_state(json!({"available": true, "status": "In Progress", "comments": [], "transitions": [], "calls": []}));
        repository
    }

    /** Write the Jira tasks, the source mapping, and the delivery configuration (organization
     * settings an administrator provides once Crane exists) */
    fn configure(&self) {
        self.write(".crane/sources/config.json", r#"{"sources_format": 1, "checkpoint": "baseline", "jira": {"acceptance_field": "customfield_10050", "projects": {"PAY": {"repositories": ["acme/shop"], "team": "billing"}}}}"#);
        let issue = |key: &str, summary: &str, description: &str, criteria: &str| json!({"id": "1", "key": key, "fields": {"summary": summary, "project": {"key": "PAY"}, "assignee": {"displayName": "Crane Bot"}, "status": {"name": "To Do", "statusCategory": {"key": "new"}}, "description": description, "customfield_10050": criteria}});
        for value in [
            issue(
                "PAY-1830",
                "Trim catalog labels",
                "Make `CatalogService.label` trim the name.",
                "labels have no surrounding spaces",
            ),
            issue(
                "PAY-1831",
                "Trim catalog labels again",
                "Make `CatalogService.label` trim the name.",
                "labels have no surrounding spaces",
            ),
            issue(
                "PAY-1840",
                "Payments are slow",
                "Customers say payments take too long. Please improve things.",
                "",
            ),
        ] {
            self.write(
                &format!(
                    ".crane/sources/jira/issues/{}.json",
                    value["key"].as_str().unwrap()
                ),
                &value.to_string(),
            );
        }
        self.write(".crane/delivery.json", &json!({
            "merge_policy": {"default": {"name": "default", "approvals": 1, "approvers": ["payments-lead"]}},
            "slack": {"notify": ["#shop"], "signing_secret_env": "CRANE_TEST_SLACK_SECRET", "users": {"U100": "payments-lead"}},
            "trackers": {"jira": {"done_transition": "31", "base_url": "https://acme.atlassian.net", "transport": [python(), self.side.join("fake_jira.py").to_string_lossy(), self.side.join("jira.json").to_string_lossy()]}},
        }).to_string());
    }

    /** Write a file relative to the root */
    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /** Read a file relative to the root */
    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }

    /** Run git and require success */
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout).trim().to_string()
    }

    /** Run crane the way a human's shell does: Crane on PATH, the Slack secret set, no agent
     * markers, extra variables, and stdin */
    fn run(&self, args: &[&str], environment: &[(&str, &str)], stdin: &str) -> Output {
        let folder = Path::new(env!("CARGO_BIN_EXE_crane"))
            .parent()
            .unwrap()
            .to_path_buf();
        let mut paths = vec![folder];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
        command
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in CLEARED {
            command.env_remove(name);
        }
        command.env("PATH", std::env::join_paths(paths).unwrap());
        command.env("CRANE_TEST_SLACK_SECRET", SECRET);
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /** Run crane and require success */
    fn crane(&self, args: &[&str]) -> String {
        let output = self.run(args, &[], "");
        assert!(
            output.status.success(),
            "crane {args:?}: {}{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /** Run crane with --json and require success */
    fn json(&self, args: &[&str]) -> Value {
        let mut full = args.to_vec();
        full.push("--json");
        serde_json::from_str(&self.crane(&full)).unwrap()
    }

    /** The golden path, for the repository or one task */
    fn flow(&self, task: Option<&str>) -> Value {
        let mut args = vec!["flow", "status"];
        if let Some(task) = task {
            args.push(task);
        }
        self.json(&args)
    }

    /** The top-level stage */
    fn stage(&self, task: Option<&str>) -> String {
        self.flow(task)["stage"].as_str().unwrap().to_string()
    }

    /** Replace the Jira stand-in's state */
    fn jira_state(&self, state: Value) {
        fs::write(self.side.join("jira.json"), state.to_string()).unwrap();
    }

    /** Read the Jira stand-in's state */
    fn jira(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.side.join("jira.json")).unwrap()).unwrap()
    }

    /** Send a Codex hook event, as Codex does for the session started with CRANE_SESSION */
    fn codex(&self, event: &str, session: &str, mut fields: Value) -> Output {
        fields["session_id"] = json!(CODEX);
        fields["cwd"] = json!(self.root.to_string_lossy());
        fields["hook_event_name"] = json!(event);
        fields["model"] = json!("gpt-5-codex");
        fields["transcript_path"] =
            json!(format!("/home/dev/.codex/sessions/rollout-{CODEX}.jsonl"));
        self.run(
            &["agent", "hook", "--event", event, "--profile", "codex"],
            &[("CRANE_SESSION", session)],
            &fields.to_string(),
        )
    }

    /** Click a button of the latest Slack announcement, signed as Slack signs it */
    fn slack_click(&self, action: &str) -> Value {
        let folder = self.root.join(".crane/runtime/delivery/outbox");
        let mut names = fs::read_dir(&folder)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with("-slack.json"))
            .collect::<Vec<_>>();
        names.sort();
        let announcement = names
            .iter()
            .rev()
            .map(|name| {
                serde_json::from_str::<Value>(&fs::read_to_string(folder.join(name)).unwrap())
                    .unwrap()["request"]
                    .clone()
            })
            .find(|message| message["blocks"].is_array())
            .unwrap();
        let button = announcement["blocks"][1]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|button| button["action_id"] == action)
            .unwrap()
            .clone();
        let payload = json!({"type": "block_actions", "user": {"id": "U100"}, "actions": [{"action_id": action, "value": button["value"]}]});
        let body = format!(
            "payload={}",
            payload
                .to_string()
                .bytes()
                .map(|byte| if byte.is_ascii_alphanumeric() {
                    (byte as char).to_string()
                } else {
                    format!("%{byte:02X}")
                })
                .collect::<String>()
        );
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let signature = Command::new(python()).args(["-c", "import hmac, hashlib, sys; print('v0=' + hmac.new(sys.argv[1].encode(), ('v0:' + sys.argv[2] + ':' + sys.argv[3]).encode(), hashlib.sha256).hexdigest())", SECRET, &timestamp, &body]).output().unwrap();
        let file = self.side.join("slack.txt");
        fs::write(&file, &body).unwrap();
        let output = self.run(
            &[
                "deliver",
                "slack-action",
                "--body",
                &file.to_string_lossy(),
                "--timestamp",
                &timestamp,
                "--signature",
                text(&signature.stdout).trim(),
                "--json",
            ],
            &[],
            "",
        );
        assert!(output.status.success(), "{}", text(&output.stderr));
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /** Run the repository setup of the golden path (steps 1 to 5) */
    fn set_up(&self) {
        self.crane(&["repo", "connect"]);
        self.configure();
        self.crane(&["zones", "recommend"]);
        let recommendations = self.json(&["zones", "recommendations"]);
        for item in recommendations["recommendations"].as_array().unwrap() {
            let id = item["id"].as_str().unwrap();
            if item["zone_id"] == "payments" {
                let digest = self.json(&["zones", "review", id])["digest"]
                    .as_str()
                    .unwrap()
                    .to_string();
                self.crane(&[
                    "zones",
                    "approve",
                    id,
                    "--approver",
                    "security-lead",
                    "--confirm",
                    &digest[7..19],
                ]);
            } else {
                self.crane(&[
                    "zones",
                    "reject",
                    id,
                    "--approver",
                    "security-lead",
                    "--reason",
                    "not needed for the golden path",
                ]);
            }
        }
        self.crane(&["dashboard", "api", "POST", "/api/contracts", "--body", r#"{"draft": {"name": "payments_core", "checkpoint": "baseline", "rules": [{"rule": "preserve", "kind": "function", "target": "PaymentService.charge"}]}, "by": "payments-lead"}"#]);
        let digest = self.json(&["policy", "show", "payments_core"])["policy_digest"]
            .as_str()
            .unwrap()
            .to_string();
        self.crane(&[
            "policy",
            "approve",
            "payments_core",
            "--approver",
            "security-lead",
            "--confirm",
            &digest[7..19],
        ]);
        self.crane(&["agent", "install", "--profile", "codex"]);
    }

    /** Plan and approve a task's contract (steps 6 and 7) */
    fn approve_task(&self, task: &str) {
        self.crane(&["task", "prepare", task]);
        let shown = self.json(&["task", "show", task]);
        let digest = shown["contract"]["digest"].as_str().unwrap().to_string();
        self.crane(&[
            "task",
            "approve",
            task,
            "--approver",
            "payments-lead",
            "--confirm",
            &digest[7..19],
        ]);
    }
}

impl Drop for Repository {
    /** Remove the repository and the side folder */
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.side);
    }
}

/** Decode process output */
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/** List the stages recorded for a scope, in order */
fn recorded(repository: &Repository, scope: &str) -> Vec<String> {
    repository.json(&["flow", "audit", scope])["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["to"].as_str().unwrap().to_string())
        .collect()
}

/** The golden path end to end: one approved engineering task is executed by an AI agent under
 * Society governance and progresses from repository connection to verified merge and task
 * completion, with one stage at every point, every transition audited, and the identifiers of
 * every layer linked: repository, discovery, zones, policies, task, contract, agent, session,
 * authority, verification, attestation, pull request, approval, merge, and task completion */
#[test]
fn one_task_from_connection_to_completion() {
    let repository = Repository::new();
    // 1. Connect the GitHub repository
    assert_eq!(repository.stage(None), "CONNECT_REPOSITORY");
    repository.crane(&["repo", "connect"]);
    let connected = repository.flow(None);
    assert_eq!(connected["layers"]["repository"]["provider"], "github");
    assert_eq!(connected["layers"]["repository"]["full_name"], "acme/shop");
    // 2. Discovery ran with the connection; 3. zones need review
    assert_eq!(connected["layers"]["discovery"]["state"], "fresh");
    assert_eq!(connected["stage"], "REVIEW_REQUIRED");
    repository.configure();
    repository.crane(&["zones", "recommend"]);
    let review = repository.flow(None);
    assert_eq!(review["stage"], "REVIEW_REQUIRED");
    assert!(review["layers"]["zones"]["pending"]
        .as_array()
        .unwrap()
        .iter()
        .any(|id| id == "payments"));
    for item in repository.json(&["zones", "recommendations"])["recommendations"]
        .as_array()
        .unwrap()
    {
        let id = item["id"].as_str().unwrap();
        if item["zone_id"] == "payments" {
            let digest = repository.json(&["zones", "review", id])["digest"]
                .as_str()
                .unwrap()
                .to_string();
            // People read the summary first and get the command to run
            let record = repository.read(&format!(".crane/zone-proposals/{id}.json"));
            assert!(
                record.starts_with(
                    "{
  \"summary\": {"
                ),
                "{record}"
            );
            let approve = format!(
                "crane zones approve {id} --approver YOUR_NAME --confirm {}",
                &digest[7..19]
            );
            assert_eq!(
                serde_json::from_str::<Value>(&record).unwrap()["summary"]["approve"],
                approve
            );
            let readable = repository.crane(&["zones", "review", id]);
            assert!(
                readable.contains("Decision needed") && readable.contains(&approve),
                "{readable}"
            );
            assert!(repository
                .crane(&["zones", "recommendations"])
                .contains(&approve));
            repository.crane(&[
                "zones",
                "approve",
                id,
                "--approver",
                "security-lead",
                "--confirm",
                &digest[7..19],
            ]);
        } else {
            repository.crane(&[
                "zones",
                "reject",
                id,
                "--approver",
                "security-lead",
                "--reason",
                "not needed",
            ]);
        }
    }
    // 4. Approve policies
    assert_eq!(repository.stage(None), "POLICY_APPROVAL");
    // The organization's policy, drafted in the control plane's policy editor
    repository.crane(&["dashboard", "api", "POST", "/api/contracts", "--body", r#"{"draft": {"name": "payments_core", "checkpoint": "baseline", "rules": [{"rule": "preserve", "kind": "function", "target": "PaymentService.charge"}]}, "by": "payments-lead"}"#]);
    assert_eq!(
        repository.flow(None)["layers"]["policies"]["pending"],
        json!(["payments_core"])
    );
    let digest = repository.json(&["policy", "show", "payments_core"])["policy_digest"]
        .as_str()
        .unwrap()
        .to_string();
    let readable = repository.crane(&["policy", "show", "payments_core"]);
    assert!(
        readable.contains(&format!(
            "crane policy approve payments_core --approver YOUR_NAME --confirm {}",
            &digest[7..19]
        )),
        "{readable}"
    );
    assert!(repository
        .read(".crane/proposals/payments_core.json")
        .starts_with(
            "{
  \"summary\": {"
        ));
    repository.crane(&[
        "policy",
        "approve",
        "payments_core",
        "--approver",
        "security-lead",
        "--confirm",
        &digest[7..19],
    ]);
    // 5. Connect Codex
    assert_eq!(repository.stage(None), "AGENT_READY");
    repository.crane(&["agent", "install", "--profile", "codex"]);
    let ready = repository.flow(None);
    assert_eq!(ready["stage"], "TASK_READY");
    assert_eq!(ready["layers"]["agents"]["connected"], json!(["codex"]));
    assert_eq!(ready["layers"]["zones"]["active"], json!(["payments"]));
    assert_eq!(
        ready["layers"]["policies"]["active"],
        json!(["payments_core"])
    );

    // 6. Select the Jira task; 7. review and approve its contract
    assert_eq!(repository.stage(Some("PAY-1830")), "TASK_READY");
    repository.crane(&["task", "prepare", "PAY-1830"]);
    let pending = repository.flow(Some("PAY-1830"));
    assert_eq!(
        pending["current"]["reason"],
        "the task contract waits for approval"
    );
    let contract = pending["tasks"][0]["contract"]["digest"]
        .as_str()
        .unwrap()
        .to_string();
    let readable = repository.crane(&["task", "contract", "show", "PAY-1830"]);
    assert!(
        readable.contains(&format!(
            "crane task contract approve PAY-1830 --approver YOUR_NAME --confirm {}",
            &contract[7..19]
        )),
        "{readable}"
    );
    assert!(repository
        .read(".crane/task-contracts/PAY-1830/v1.json")
        .starts_with(
            "{
  \"summary\": {"
        ));
    repository.crane(&[
        "task",
        "approve",
        "PAY-1830",
        "--approver",
        "payments-lead",
        "--confirm",
        &contract[7..19],
    ]);
    assert_eq!(
        repository.flow(Some("PAY-1830"))["current"]["reason"],
        "the contract is approved; start the agent"
    );

    // 8. Start the agent; 9. Society governs its execution
    let started = repository.json(&["session", "run", "PAY-1830", "--agent", "codex", "--detach"]);
    let session = started["session_id"].as_str().unwrap().to_string();
    assert_eq!(session, "codex-task-PAY-1830-v1");
    assert_eq!(repository.stage(Some("PAY-1830")), "RUNNING");
    assert!(repository
        .codex("SessionStart", &session, json!({"source": "startup"}))
        .status
        .success());
    let patch = format!("*** Begin Patch\n*** Update File: {INVOICE}\n@@\n-        return name;\n+        {ROUNDED}\n*** End Patch\n");
    let allowed = repository.codex("PreToolUse", &session, json!({"tool_name": "apply_patch", "tool_input": {"command": patch}, "tool_use_id": "call_1"}));
    assert!(
        allowed.status.success() && text(&allowed.stdout).is_empty(),
        "{}{}",
        text(&allowed.stdout),
        text(&allowed.stderr)
    );
    let forbidden = repository.codex("PreToolUse", &session, json!({"tool_name": "Bash", "tool_input": {"command": "crane checkpoint --name baseline"}, "tool_use_id": "call_2"}));
    assert_eq!(
        forbidden.status.code(),
        Some(2),
        "Society denies what the agent may not do"
    );
    repository.write(
        INVOICE,
        &repository.read(INVOICE).replace("return name;", ROUNDED),
    );
    assert!(repository.codex("PostToolUse", &session, json!({"tool_name": "apply_patch", "tool_input": {"command": patch}, "tool_response": "Success.", "tool_use_id": "call_1"})).status.success());
    let running = repository.flow(Some("PAY-1830"));
    let layers = &running["tasks"][0];
    assert_eq!(running["stage"], "RUNNING");
    assert_eq!(layers["agent"]["profile"], "codex");
    assert_eq!(layers["agent"]["attachments"][0]["provider_session"], CODEX);
    assert_eq!(layers["authority"]["decisions"]["ALLOW"], 1);
    assert_eq!(layers["authority"]["decisions"]["DENY"], 1);
    assert_eq!(layers["authority"]["safety"], "active");

    // 10. Verify the changes (the agent's claims are not trusted)
    repository.codex(
        "Stop",
        &session,
        json!({"stop_hook_active": false, "last_assistant_message": "Done; all tests pass."}),
    );
    repository.crane(&["session", "finish", &session]);
    let verified = repository.flow(Some("PAY-1830"));
    assert_eq!(verified["stage"], "DELIVERY_READY");
    assert_eq!(verified["tasks"][0]["verification"]["final_status"], "PASS");

    // 11. PR and attestation (automatic)
    let advanced = repository.json(&["flow", "advance", "PAY-1830"]);
    assert_eq!(advanced["steps"][0]["from"], "DELIVERY_READY");
    assert_eq!(advanced["steps"][0]["to"], "REVIEW");
    let review = &advanced["status"]["tasks"][0];
    assert_eq!(advanced["status"]["stage"], "REVIEW");
    assert!(review["pull_request"]["url"]
        .as_str()
        .unwrap()
        .contains("/pull/"));
    assert!(
        repository.jira()["calls"].as_array().unwrap().is_empty(),
        "nothing touches Jira before the merge"
    );

    // 12. Review in Slack
    let approved = repository.slack_click("approve");
    assert_eq!(approved["delivery_state"], "APPROVED");
    assert_eq!(repository.stage(Some("PAY-1830")), "MERGING");
    assert!(
        repository.jira()["calls"].as_array().unwrap().is_empty(),
        "an approval alone completes nothing"
    );

    // 13. Merge; 14. the Jira task closes automatically
    let finished = repository.json(&["flow", "advance", "PAY-1830", "--by", "release-lead"]);
    let steps = finished["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            (
                step["from"].as_str().unwrap().to_string(),
                step["to"].as_str().unwrap().to_string(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        steps,
        [
            ("MERGING".to_string(), "COMPLETING_TASK".to_string()),
            ("COMPLETING_TASK".to_string(), "COMPLETED".to_string())
        ]
    );
    let done = &finished["status"];
    assert_eq!(done["stage"], "COMPLETED");
    let task = &done["tasks"][0];

    // The identifiers stay linked across the whole lifecycle
    assert_eq!(task["links"]["consistent"], true, "{}", task["links"]);
    assert_eq!(task["task"]["task_id"], "PAY-1830");
    assert_eq!(task["task"]["external_id"], "PAY-1830");
    assert_eq!(task["contract"]["digest"], contract.as_str());
    assert_eq!(
        task["session"]["task_contract"]["digest"],
        contract.as_str()
    );
    assert_eq!(task["pull_request"]["contract_digest"], contract.as_str());
    assert_eq!(
        task["task_completion"]["contract_digest"],
        contract.as_str()
    );
    assert_eq!(task["session"]["session_id"], session.as_str());
    let sha = task["merge"]["sha"].as_str().unwrap().to_string();
    assert_eq!(repository.git(&["rev-parse", "main"]), sha);
    assert_eq!(task["task_completion"]["merge_sha"], sha.as_str());
    assert_eq!(
        task["pull_request"]["attestation"],
        task["task_completion"]["attestation"]
    );
    let checkpoint: Value = serde_json::from_str(&repository.read(&format!(
        ".crane/checkpoints/{}.json",
        task["merge"]["trusted_checkpoint"].as_str().unwrap()
    )))
    .unwrap();
    assert_eq!(checkpoint["commit"], sha.as_str());
    assert_eq!(
        done["layers"]["repository"]["trusted_checkpoint"]["name"],
        task["merge"]["trusted_checkpoint"]
    );
    for condition in task["terminal"]["conditions"].as_array().unwrap() {
        assert_eq!(condition["met"], true, "{condition}");
    }
    let jira = repository.jira();
    assert_eq!(jira["status"], "Done");
    assert_eq!(
        jira["transitions"],
        json!(["31"]),
        "the Jira task closed exactly once"
    );
    assert!(jira["comments"]
        .to_string()
        .contains(task["task_completion"]["event_id"].as_str().unwrap()));
    assert_eq!(
        repository.json(&["task", "show", "PAY-1830"])["state"],
        "COMPLETED"
    );

    // Every transition is audited, in order, on a verified chain
    // Before the connection Crane has nowhere to record anything, so the audit starts there
    assert_eq!(
        recorded(&repository, "repository"),
        [
            "REVIEW_REQUIRED",
            "POLICY_APPROVAL",
            "AGENT_READY",
            "TASK_READY"
        ]
    );
    // The task waited for the repository setup, then went through the whole lifecycle
    assert_eq!(
        recorded(&repository, "PAY-1830"),
        [
            "REVIEW_REQUIRED",
            "POLICY_APPROVAL",
            "AGENT_READY",
            "TASK_READY",
            "RUNNING",
            "DELIVERY_READY",
            "REVIEW",
            "MERGING",
            "COMPLETING_TASK",
            "COMPLETED"
        ]
    );
    let audit = repository.json(&["flow", "audit"]);
    assert_eq!(audit["chain"]["status"], "verified");
    let last = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|event| event["scope"] == "PAY-1830")
        .unwrap()
        .clone();
    assert_eq!(last["identifiers"]["merge_sha"], sha.as_str());
    assert_eq!(last["identifiers"]["contract_digest"], contract.as_str());

    // Idempotent: advancing again does nothing; after a restart the state is derived again
    let events = audit["events"].as_array().unwrap().len();
    let calls = repository.jira()["calls"].as_array().unwrap().len();
    assert!(repository.json(&["flow", "advance", "PAY-1830"])["steps"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        repository.json(&["flow", "audit"])["events"]
            .as_array()
            .unwrap()
            .len(),
        events
    );
    assert_eq!(repository.jira()["calls"].as_array().unwrap().len(), calls);
    fs::remove_file(repository.root.join(".crane/runtime/flow/state.json")).unwrap();
    assert_eq!(
        repository.stage(Some("PAY-1830")),
        "COMPLETED",
        "recovered from the subsystems after a restart"
    );
    let human = repository.crane(&["flow", "status", "PAY-1830"]);
    for layer in [
        "Repository",
        "Discovery",
        "Policy",
        "Zones",
        "Task",
        "Contract",
        "Agent",
        "Session",
        "Authority",
        "Verification",
        "Attestation",
        "PR",
        "Approval",
        "Merge",
        "Completion",
    ] {
        assert!(human.contains(layer), "{layer} missing:\n{human}");
    }

    // COMPLETED is never reported when a terminal condition does not hold
    let event_path = repository.root.join(format!(
        ".crane/runtime/completions/{}.json",
        task["task_completion"]["event_id"].as_str().unwrap()
    ));
    let original = fs::read_to_string(&event_path).unwrap();
    assert!(
        original.starts_with(
            "{
  \"summary\": {"
        ),
        "{original}"
    );
    let summary = &serde_json::from_str::<Value>(&original).unwrap()["summary"];
    assert!(
        summary["state"].as_str().unwrap().starts_with("Done:"),
        "{summary}"
    );
    let mut tampered: Value = serde_json::from_str(&original).unwrap();
    tampered["merge_sha"] = json!(repository.git(&["rev-parse", "main~1"]));
    fs::write(&event_path, tampered.to_string()).unwrap();
    let broken = repository.flow(Some("PAY-1830"));
    assert_eq!(broken["stage"], "BLOCKED");
    assert!(broken["current"]["reason"]
        .as_str()
        .unwrap()
        .contains("merge commit"));
    fs::write(&event_path, original).unwrap();
    assert_eq!(repository.stage(Some("PAY-1830")), "COMPLETED");

    // The dashboard API serves the same golden path
    let api = repository.run(&["dashboard", "api", "GET", "/api/flow/PAY-1830"], &[], "");
    let api: Value = serde_json::from_slice(&api.stdout).unwrap();
    assert_eq!(api["stage"], "COMPLETED");
    assert_eq!(api["tasks"][0]["merge"]["sha"], sha.as_str());
}

/** The failure branches: a vague task needs clarification, an agent that escalates is
 * quarantined, an unverified change fails verification, a rejected pull request is denied, and an
 * unreachable Jira leaves the merged task in COMPLETION_RETRY_PENDING until it completes; none of
 * them is ever COMPLETED */
#[test]
fn failure_branches_are_reported() {
    let repository = Repository::new();
    repository.set_up();
    assert_eq!(repository.stage(None), "TASK_READY");

    let vague = repository.run(&["task", "prepare", "PAY-1840"], &[], "");
    assert!(!vague.status.success());
    assert_eq!(repository.stage(Some("PAY-1840")), "NEEDS_CLARIFICATION");

    repository.approve_task("PAY-1830");
    let actions = repository.side.join("escalate.json");
    fs::write(&actions, json!([{"tool": "Bash", "operation": "execute", "command": "crane autonomy refill codex-task-PAY-1830-v1 --amount 1000 --reason more"}]).to_string()).unwrap();
    repository.run(
        &[
            "session",
            "run",
            "PAY-1830",
            "--agent",
            "codex",
            "--actions",
            &actions.to_string_lossy(),
        ],
        &[],
        "",
    );
    let quarantined = repository.flow(Some("PAY-1830"));
    assert_eq!(quarantined["stage"], "QUARANTINED");
    assert_eq!(quarantined["current"]["failure"], true);

    repository.approve_task("PAY-1831");
    let nothing = repository.side.join("nothing.json");
    fs::write(
        &nothing,
        json!([{"operation": "claim", "text": "Done, everything passes."}]).to_string(),
    )
    .unwrap();
    repository.run(
        &[
            "session",
            "run",
            "PAY-1831",
            "--agent",
            "codex",
            "--actions",
            &nothing.to_string_lossy(),
        ],
        &[],
        "",
    );
    assert_eq!(repository.stage(Some("PAY-1831")), "VERIFICATION_FAILED");

    // A run that is rejected in review, and a merge whose Jira completion has to wait
    let other = Repository::new();
    other.set_up();
    other.approve_task("PAY-1830");
    let work = other.side.join("work.json");
    fs::write(&work, json!([{"tool": "Edit", "operation": "write", "path": INVOICE, "edits": [{"old": "return name;", "new": ROUNDED}]}]).to_string()).unwrap();
    other.crane(&[
        "session",
        "run",
        "PAY-1830",
        "--agent",
        "codex",
        "--actions",
        &work.to_string_lossy(),
    ]);
    other.crane(&["flow", "advance", "PAY-1830"]);
    assert_eq!(other.stage(Some("PAY-1830")), "REVIEW");
    other.slack_click("reject");
    assert_eq!(other.stage(Some("PAY-1830")), "DENIED");
    other.slack_click("approve");
    assert_eq!(
        other.stage(Some("PAY-1830")),
        "DENIED",
        "the rejection holds for this round"
    );

    let third = Repository::new();
    third.set_up();
    third.approve_task("PAY-1830");
    let work = third.side.join("work.json");
    fs::write(&work, json!([{"tool": "Edit", "operation": "write", "path": INVOICE, "edits": [{"old": "return name;", "new": ROUNDED}]}]).to_string()).unwrap();
    third.crane(&[
        "session",
        "run",
        "PAY-1830",
        "--agent",
        "codex",
        "--actions",
        &work.to_string_lossy(),
    ]);
    third.crane(&["flow", "advance", "PAY-1830"]);
    third.slack_click("approve");
    let mut state = third.jira();
    state["available"] = json!(false);
    third.jira_state(state);
    let waiting = third.json(&["flow", "advance", "PAY-1830"]);
    assert_eq!(waiting["status"]["stage"], "COMPLETION_RETRY_PENDING");
    assert_eq!(
        third.json(&["deliver", "status", "codex-task-PAY-1830-v1"])["delivery_state"],
        "COMPLETE",
        "merged, but not completed"
    );
    let mut state = third.jira();
    state["available"] = json!(true);
    third.jira_state(state);
    let completed = third.json(&["flow", "advance", "PAY-1830", "--now"]);
    assert_eq!(completed["status"]["stage"], "COMPLETED");
    assert_eq!(third.jira()["transitions"], json!(["31"]));
}

/** Agents can read the golden path but never advance it */
#[test]
fn agents_cannot_advance_the_flow() {
    let repository = Repository::new();
    repository.set_up();
    let read = repository.run(&["flow", "status", "--json"], &[("CLAUDECODE", "1")], "");
    assert!(read.status.success());
    let advance = repository.run(&["flow", "advance", "PAY-1830"], &[("CLAUDECODE", "1")], "");
    assert!(!advance.status.success());
    assert!(text(&advance.stderr).contains("refuses to run in an agent environment"));
}
