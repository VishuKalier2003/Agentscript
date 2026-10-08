use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/** Counter that keeps fixture directory names unique when tests run in parallel */
static REPOSITORIES: AtomicUsize = AtomicUsize::new(0);

/** Environment variables that mark an agent environment */
const AGENT_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CRANE_AGENT",
];

/** Token of the observability server in tests */
const TOKEN: &str = "observe-token-0123456789";

/** Payment service: charge is protected by a policy, the file is in a critical zone */
const SERVICE: &str = "class PaymentService:\n    def charge(self, amount):\n        return amount + self.fee(amount)\n\n    def fee(self, amount):\n        return amount // 10\n";

/** A temporary repository with Crane initialized, removed on drop
 * Fields
    - root: PathBuf - repository root
*/
struct Repository {
    root: PathBuf,
}

impl Repository {
    /** Create the fixture: a payment service under a preserve policy and a critical zone, a
     * catalog outside every zone, and a unique root commit
     * Input
        - None
     * Output
        - Repository
    */
    fn new() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-observe-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Self { root };
        repository.write("payments/service.py", SERVICE);
        repository.write(
            "catalog/labels.py",
            "def label(name):\n    return name.strip()\n",
        );
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "crane@example.com"],
            vec!["config", "user.name", "Crane Observe Test"],
            vec!["config", "core.autocrlf", "false"],
            vec!["add", "."],
        ] {
            repository.git(&args);
        }
        repository.git(&["commit", "-qm", &format!("baseline {suffix}")]);
        repository.ok(&["init"]);
        repository.ok(&["checkpoint", "--name", "baseline"]);
        repository.write(
            ".crane/policies/payments.crane",
            "policy payments {\n    checkpoint baseline;\n    preserve --function PaymentService.charge;\n}\n",
        );
        repository.write(
            ".crane/zones/org.zone",
            "zone money {\n    criticality critical;\n    autonomy assisted;\n    select path payments/service.py;\n}\n",
        );
        repository
    }

    /** Write a file relative to the root
     * Input
        - path: &str - relative path
        - content: &str - content
     * Output
        - None
    */
    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /** Run git and require success
     * Input
        - args: &[&str] - arguments
     * Output
        - None
    */
    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
    }

    /** Run crane as a human, with stdin
     * Input
        - args: &[&str] - arguments
        - stdin: &str - standard input
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
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /** Run crane and require success
     * Input
        - args: &[&str] - arguments
     * Output
        - String stdout
    */
    fn ok(&self, args: &[&str]) -> String {
        let output = self.crane(args, "");
        assert!(
            output.status.success(),
            "crane {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    /** Query the observability API through the CLI and require success
     * Input
        - path: &str - API path with query
     * Output
        - Value
    */
    fn observe(&self, path: &str) -> Value {
        let value: Value = serde_json::from_str(&self.ok(&["observe", path])).unwrap();
        assert_eq!(value["read_only"], true, "{path}");
        value
    }

    /** Send one agent tool call through the Claude Code pre-tool-use hook
     * Input
        - tool: &str - tool name
        - input: Value - tool input
     * Output
        - Output
    */
    fn hook(&self, tool: &str, input: Value) -> Output {
        let event = json!({"session_id": "obs-1", "hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": input});
        self.crane(
            &[
                "agent",
                "hook",
                "--event",
                "pre-tool-use",
                "--profile",
                "claude",
            ],
            &event.to_string(),
        )
    }

    /** Make one HTTP request to the observability server (started for that one request)
     * Input
        - method: &str - HTTP method
        - path: &str - request path
        - token: &str - X-Crane-Token
     * Output
        - (u16, String) status and body
    */
    fn http(&self, method: &str, path: &str, token: &str) -> (u16, String) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
        command
            .args(["observe", "serve", "--addr", "127.0.0.1:0", "--once"])
            .current_dir(&self.root)
            .env("CRANE_OBSERVE_TOKEN", TOKEN)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for marker in AGENT_MARKERS {
            command.env_remove(marker);
        }
        let mut server = command.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(server.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line
            .split("http://")
            .nth(1)
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .to_string();
        let mut stream = TcpStream::connect(&address).unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {address}\r\nX-Crane-Token: {token}\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        server.wait().unwrap();
        let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        (status, body)
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

/** Snapshot every file under a directory: relative path to content
 * Input
    - root: &Path - directory
 * Output
    - BTreeMap<String, Vec<u8>>
*/
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    files
}

/** The observability API answers what agents did, which decisions were made and why (policies and
 * zones), violations and their severity, and how autonomy changed, from the existing records;
 * it accepts GET only, with typed parameters, and changes no byte under .crane, whether queried
 * through the CLI or the server */
#[test]
fn observability_is_read_only_and_answers_the_questions() {
    let repository = Repository::new();
    // An agent session: an allowed edit, a denied edit (the payments policy), an edit that needs
    // approval (the money zone), and an attempt to raise its own autonomy (critical)
    let allowed = repository.hook(
        "Edit",
        json!({"file_path": "catalog/labels.py", "old_string": "return name.strip()", "new_string": "return name"}),
    );
    assert!(allowed.status.success() && allowed.stdout.is_empty());
    let denied = repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount + self.fee(amount)", "new_string": "return amount"}),
    );
    assert_eq!(denied.status.code(), Some(2));
    let gated = repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount // 10", "new_string": "return amount // 20"}),
    );
    assert!(String::from_utf8_lossy(&gated.stdout).contains("\"ask\""));
    let escalation = repository.hook(
        "Bash",
        json!({"command": "crane autonomy promote claude-obs-1 --to autonomous --approver me"}),
    );
    assert_eq!(escalation.status.code(), Some(2));

    let before = snapshot(&repository.root.join(".crane"));

    let overview = repository.observe("/api/v1/overview");
    assert_eq!(overview["sessions"]["total"], 1);
    assert_eq!(overview["decisions"]["allow"], 1);
    assert_eq!(overview["decisions"]["deny"], 2);
    assert_eq!(overview["decisions"]["approval_required"], 1);
    assert_eq!(overview["violations"]["critical"], 1);

    let agents = repository.observe("/api/v1/agents");
    assert_eq!(agents["agents"][0]["agent"], "claude");
    assert_eq!(agents["agents"][0]["provider"], "Claude");

    let sessions = repository.observe("/api/v1/sessions?agent=claude");
    assert_eq!(sessions["total"], 1);
    let id = sessions["items"][0]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(sessions["items"][0]["safety"]["state"], "quarantined");
    assert_eq!(
        sessions["items"][0]["evidence"]["journal_chain"],
        "verified"
    );

    // Which policy caused the denial, and which zones were involved
    let denials = repository.observe("/api/v1/decisions?decision=deny&policy=payments");
    assert_eq!(denials["total"], 1, "{denials}");
    let denial = &denials["items"][0];
    assert_eq!(denial["policies"], json!(["payments"]));
    assert_eq!(denial["zones"], json!(["money"]));
    assert!(denial["reasons"][0]
        .as_str()
        .unwrap()
        .contains("protected by preserve function PaymentService.charge"));
    let zoned = repository.observe(&format!(
        "/api/v1/sessions/{id}/actions?decision=approval_required"
    ));
    assert_eq!(zoned["items"][0]["zones"], json!(["money"]));
    assert_eq!(
        repository.observe("/api/v1/decisions?zone=money")["total"],
        2
    );

    let violations = repository.observe("/api/v1/violations?severity=critical");
    assert_eq!(violations["items"][0]["kind"], "self_escalation");
    let autonomy = repository.observe(&format!("/api/v1/sessions/{id}/autonomy"));
    assert!(autonomy["autonomy_changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|change| change["safety"] == "quarantined"));
    let detail = repository.observe(&format!("/api/v1/sessions/{id}"));
    assert!(detail["attestation"]["attestation_digest"].is_string());
    assert_eq!(repository.observe("/api/v1/tasks")["total"], 0);
    assert!(repository.observe("/api/v1/repositories")["repositories"].is_array());
    assert_eq!(
        repository.observe("/api/v1/audit")["sessions"][0]["chain"]["status"],
        "verified"
    );
    let endpoints = repository.observe("/api/v1/endpoints");
    assert!(endpoints["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .all(|endpoint| endpoint["method"] == "GET"));

    // Typed parameters only
    let refused = repository.crane(&["observe", "/api/v1/decisions?sql=select"], "");
    assert!(!refused.status.success());

    // The server: its own token, GET only, no body
    let (status, body) = repository.http("GET", "/api/v1/overview", TOKEN);
    assert_eq!(status, 200, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), overview);
    assert_eq!(
        repository
            .http("GET", "/api/v1/overview", "wrong-token-000000000000")
            .0,
        401
    );
    for method in ["POST", "PUT", "DELETE"] {
        assert_eq!(
            repository.http(method, "/api/v1/sessions", TOKEN).0,
            405,
            "{method}"
        );
    }

    // Nothing under .crane changed
    assert_eq!(snapshot(&repository.root.join(".crane")), before);

    // Serving is a human's: the hook denies it to agents, while querying stays a read
    let serve = repository.hook("Bash", json!({"command": "crane observe serve"}));
    assert_eq!(serve.status.code(), Some(2));
}

/** The global Overview: headline metrics, multi-select filters, time windows, active sessions,
 * and a clickable activity stream, all computed read-only; the page is served at / and the
 * records under .crane are untouched */
#[test]
fn overview_dashboard_filters_and_streams() {
    let repository = Repository::new();
    repository.hook(
        "Edit",
        json!({"file_path": "catalog/labels.py", "old_string": "return name.strip()", "new_string": "return name"}),
    );
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount + self.fee(amount)", "new_string": "return amount"}),
    );
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount // 10", "new_string": "return amount // 20"}),
    );
    repository.hook(
        "Bash",
        json!({"command": "crane autonomy promote claude-obs-1 --to autonomous --approver me"}),
    );
    let before = snapshot(&repository.root.join(".crane"));

    let overview = repository.observe("/api/v1/dashboard/overview?range=1h");
    let metric = |value: &Value, key: &str| {
        value["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|metric| metric["key"] == key)
            .unwrap_or_else(|| panic!("no metric {key}"))["value"]
            .clone()
    };
    assert_eq!(overview["metrics"].as_array().unwrap().len(), 20);
    assert_eq!(metric(&overview, "active_sessions"), 1);
    assert_eq!(metric(&overview, "quarantined_sessions"), 1);
    assert_eq!(metric(&overview, "critical_violations"), 1);
    assert_eq!(metric(&overview, "human_intervention_rate"), 100.0);
    assert!(metric(&overview, "average_autonomy_score").is_number());
    assert_eq!(overview["trends"].as_array().unwrap().len(), 8);
    let active = &overview["active_now"][0];
    assert_eq!(active["agent"], "Claude Code");
    assert_eq!(active["safety"], "quarantined");
    assert_eq!(active["agent_id"], "claude.default");
    // Every metric card drills into the records it counts: the page maps each metric key to a view
    let page =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/observe/app.html"))
            .unwrap();
    let links = &page[page.find("const METRIC_LINKS").unwrap()..];
    let links = &links[..links.find("};").unwrap()];
    for item in overview["metrics"].as_array().unwrap() {
        let key = item["key"].as_str().unwrap();
        assert!(
            links.contains(&format!("  {key}: () => drill(")),
            "metric {key} has no click-through"
        );
    }
    let id = active["session_id"].as_str().unwrap().to_string();
    let events = overview["recent_activity"]["items"].as_array().unwrap();
    let denied = events
        .iter()
        .find(|event| {
            event["outcome"] == "DENY" && event["description"] == "payments/service.py modified"
        })
        .unwrap_or_else(|| panic!("no denied edit in {events:?}"));
    assert_eq!(denied["detail"], "Policy: payments/preserve");
    assert_eq!(denied["actor"], "Claude Code");
    assert_eq!(
        denied["agent_id"], "claude.default",
        "the side panel links the agent"
    );
    assert!(denied["link"]
        .as_str()
        .unwrap()
        .starts_with(&format!("#/runs/{id}?seq=")));
    assert!(events
        .iter()
        .any(|event| event["outcome"] == "ALLOW"
            && event["description"] == "catalog/labels.py modified"));
    assert!(events
        .iter()
        .any(|event| event["outcome"] == "ASK" && event["detail"] == "Zone: money"));
    assert!(overview["facets"]["agent"]
        .as_array()
        .unwrap()
        .contains(&json!("claude")));
    assert!(overview["facets"]["zone"]
        .as_array()
        .unwrap()
        .contains(&json!("money")));

    // Multi-select filters: OR within a filter, AND across filters
    let sessions = |path: &str| repository.observe(path)["counts"]["sessions"].clone();
    assert_eq!(sessions("/api/v1/dashboard/overview?agent=codex"), 0);
    assert_eq!(sessions("/api/v1/dashboard/overview?agent=claude,codex"), 1);
    assert_eq!(
        sessions("/api/v1/dashboard/overview?zone=money&policy=payments&severity=critical"),
        1
    );
    assert_eq!(
        sessions("/api/v1/dashboard/overview?agent=claude&safety=active"),
        0
    );
    let filtered = repository.observe("/api/v1/dashboard/overview?agent=codex");
    assert!(filtered["active_now"].as_array().unwrap().is_empty());
    assert_eq!(filtered["recent_activity"]["total"], 0);

    // Time windows: an old custom range holds none of today's events
    let old = repository.observe("/api/v1/dashboard/overview?range=custom&from=1000&to=2000");
    assert_eq!(old["recent_activity"]["total"], 0);
    assert_eq!(metric(&old, "policy_violations"), 0);
    assert_eq!(
        metric(&old, "active_sessions"),
        1,
        "current-state metrics ignore the window"
    );
    for bad in [
        "/api/v1/dashboard/overview?range=2h",
        "/api/v1/dashboard/overview?autonomy=superuser",
        "/api/v1/dashboard/overview?range=custom",
        "/api/v1/dashboard/overview?owner=me",
    ] {
        assert!(
            !repository.crane(&["observe", bad], "").status.success(),
            "{bad}"
        );
    }

    // The page, served read-only at /
    let (status, page) = repository.http("GET", "/", TOKEN);
    assert_eq!(status, 200);
    assert!(page.contains("<title>Society Overview</title>") && page.contains("READ-ONLY"));
    assert_eq!(repository.http("POST", "/", TOKEN).0, 405);

    assert_eq!(snapshot(&repository.root.join(".crane")), before);
}

/** Write an orchestration task record
 * Input
    - repository: &Repository - fixture
    - id: &str - task id
    - state: &str - current state
    - sessions: &[&str] - its session ids
    - history: &[(&str, u64)] - states reached and when
 * Output
    - None
*/
fn task_record(
    repository: &Repository,
    id: &str,
    state: &str,
    sessions: &[&str],
    history: &[(&str, u64)],
) {
    let history = history
        .iter()
        .map(|(to, at)| json!({"from": null, "to": to, "at": at, "cause": "test", "reason": format!("reached {to}")}))
        .collect::<Vec<_>>();
    repository.write(
        &format!(".crane/runtime/tasks/{id}/state.json"),
        &json!({"orchestration_format": 1, "task_id": id, "source": "jira", "external_id": id, "state": state, "sessions": sessions, "history": history}).to_string(),
    );
}

/** The current time in Unix seconds
 * Input
    - None
 * Output
    - u64
*/
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/** The Tasks page: one row per task with its sessions' facts, filtered, searched, sorted, and
 * paginated on the server, read-only */
#[test]
fn tasks_page_filters_searches_sorts_and_pages() {
    let repository = Repository::new();
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount + self.fee(amount)", "new_string": "return amount"}),
    );
    repository.hook(
        "Bash",
        json!({"command": "crane autonomy promote claude-obs-1 --to autonomous --approver me"}),
    );
    let session = repository.observe("/api/v1/sessions")["items"][0]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let now = now();
    task_record(
        &repository,
        "PAY-1",
        "EXECUTING",
        &[&session],
        &[("RECEIVED", now - 100), ("EXECUTING", now - 90)],
    );
    task_record(
        &repository,
        "PAY-2",
        "COMPLETED",
        &[],
        &[("RECEIVED", now - 3_600), ("COMPLETED", now - 600)],
    );
    task_record(
        &repository,
        "PAY-3",
        "FAILED",
        &[],
        &[("RECEIVED", now - 7_200), ("FAILED", now - 7_000)],
    );
    let before = snapshot(&repository.root.join(".crane"));

    let ids = |path: &str| {
        repository.observe(path)["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["task_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    let base = "/api/v1/dashboard/tasks?range=24h";
    assert_eq!(ids(base), ["PAY-1", "PAY-2", "PAY-3"]);
    assert_eq!(
        ids(&format!("{base}&sort=oldest")),
        ["PAY-3", "PAY-2", "PAY-1"]
    );
    assert_eq!(ids(&format!("{base}&sort=duration"))[0], "PAY-2");
    assert_eq!(ids(&format!("{base}&sort=violations"))[0], "PAY-1");
    assert_eq!(ids(&format!("{base}&sort=risk"))[0], "PAY-1");
    assert_eq!(
        ids(&format!("{base}&sort=status")),
        ["PAY-1", "PAY-2", "PAY-3"]
    );

    let row = &repository.observe(&format!("{base}&task=PAY-1"))["items"][0];
    assert_eq!(row["runs"], 1);
    assert_eq!(row["agents"], json!(["Claude Code"]));
    assert_eq!(row["providers"], json!(["Claude"]));
    assert_eq!(row["status"], "EXECUTING");
    assert_eq!(row["final"], false);
    assert_eq!(row["critical_violations"], 1);
    assert_eq!(row["link"], "#/tasks/PAY-1");
    for key in [
        "title",
        "repository",
        "team",
        "start",
        "duration",
        "autonomy",
        "autonomy_score",
        "approvals",
        "contract_verification",
        "repository_tests",
        "pull_request",
        "merge_status",
        "completion_status",
    ] {
        assert!(row.get(key).is_some(), "row lacks {key}");
    }

    // Search by task ID, by session ID, and by agent ID; filters reach tasks through sessions
    assert_eq!(ids(&format!("{base}&q=pay-2")), ["PAY-2"]);
    assert_eq!(ids(&format!("{base}&q={session}")), ["PAY-1"]);
    assert_eq!(ids(&format!("{base}&q=claude")), ["PAY-1"]);
    assert_eq!(ids(&format!("{base}&agent=claude")), ["PAY-1"]);
    assert_eq!(
        ids(&format!("{base}&severity=critical&zone=money")),
        ["PAY-1"]
    );
    assert_eq!(
        ids(&format!("{base}&task_status=COMPLETED,FAILED")),
        ["PAY-2", "PAY-3"]
    );

    // A task is listed when it overlaps the window
    assert_eq!(ids("/api/v1/dashboard/tasks?range=15m"), ["PAY-1", "PAY-2"]);

    // Pagination
    let second = repository.observe(&format!("{base}&page_size=1&page=2"));
    assert_eq!(
        (second["total"].clone(), second["pages"].clone()),
        (json!(3), json!(3))
    );
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert_eq!(second["items"][0]["task_id"], "PAY-2");
    for bad in ["sort=worst", "page_size=1000", "page=0", "q=a&q=b"] {
        assert!(
            !repository
                .crane(&["observe", &format!("{base}&{bad}")], "")
                .status
                .success(),
            "{bad}"
        );
    }
    assert_eq!(snapshot(&repository.root.join(".crane")), before);
}

/** The Tasks page stays fast with tens of thousands of tasks: filtering, sorting, and paging
 * happen on the server, and a page carries only its rows */
#[test]
fn tasks_page_scales_to_tens_of_thousands() {
    let repository = Repository::new();
    let now = now();
    let count = 20_000u64;
    for index in 0..count {
        let state = if index % 3 == 0 {
            "COMPLETED"
        } else {
            "EXECUTING"
        };
        task_record(
            &repository,
            &format!("LOAD-{index:05}"),
            state,
            &[],
            &[
                ("RECEIVED", now - 86_000 + index),
                (state, now - 85_000 + index),
            ],
        );
    }
    let started = std::time::Instant::now();
    let first = repository.observe("/api/v1/dashboard/tasks?range=30d&page_size=50");
    let elapsed = started.elapsed();
    assert_eq!(first["total"], count);
    assert_eq!(first["items"].as_array().unwrap().len(), 50);
    assert_eq!(first["items"][0]["task_id"], "LOAD-19999");
    let last = repository.observe("/api/v1/dashboard/tasks?range=30d&page_size=200&page=100&task_status=COMPLETED&sort=oldest");
    assert_eq!(last["total"], 6_667);
    assert_eq!(last["pages"], 34);
    assert_eq!(
        last["items"].as_array().unwrap().len(),
        0,
        "page 100 is past the end"
    );
    let searched = repository.observe("/api/v1/dashboard/tasks?range=30d&q=load-12345");
    assert_eq!(searched["total"], 1);
    assert!(
        elapsed.as_secs() < 30,
        "one page over {count} tasks took {elapsed:?}"
    );
}

/** Task Details: what happened when Society let an agent execute one task, section by section,
 * from the records that own it, read-only and without secrets */
#[test]
fn task_details_explain_the_execution() {
    let repository = Repository::new();
    let start = json!({"session_id": "obs-1", "hook_event_name": "SessionStart", "source": "startup", "model": "claude-opus-5-5"});
    repository.crane(
        &[
            "agent",
            "hook",
            "--event",
            "session-start",
            "--profile",
            "claude",
        ],
        &start.to_string(),
    );
    repository.hook("Read", json!({"file_path": "payments/service.py"}));
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount + self.fee(amount)", "new_string": "return amount"}),
    );
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount // 10", "new_string": "return amount // 20"}),
    );
    repository.hook(
        "Bash",
        json!({"command": "crane autonomy promote claude-obs-1 --to autonomous --approver me"}),
    );
    let session = repository.observe("/api/v1/sessions")["items"][0]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let now = now();
    task_record(
        &repository,
        "PAY-184",
        "REVIEW",
        &[&session],
        &[
            ("RECEIVED", now - 600),
            ("EXECUTING", now - 500),
            ("REVIEW", now - 60),
        ],
    );
    repository.write(
        ".crane/task-contracts/PAY-184/v1.json",
        &json!({
            "task_contract_format": 1, "task_id": "PAY-184", "contract_id": "PAY-184@v1", "version": 1,
            "status": "approved", "digest": "sha256:c0ffee0000000000000000000000000000000000000000000000000000000000",
            "task": {"title": "Reject negative refunds", "description": "Refunds below zero must be rejected.", "acceptance_criteria": ["a negative refund raises"], "requester": "pm@example.com"},
            "bindings": {"checkpoint": {"name": "baseline", "sha": "abc123"}, "policy_version": "sha256:p1", "zone_set_version": "sha256:z1", "autonomy": {"policy_version": "sha256:a1", "max_autonomy": "delegated"}, "budget": {"model_version": "sha256:b1"}},
            "contract": {
                "MUST_CHANGE": [{"qualified": "PaymentService.fee", "file": "payments/service.py", "because": ["named in description"]}],
                "MUST_NOT_CHANGE": [{"qualified": "PaymentService.charge", "file": "payments/service.py", "because": ["preserved by payments"]}],
                "MAY_CHANGE": ["symbol:python:payments.service.PaymentService"],
                "REQUIRES_APPROVAL": [],
                "TASK_SCOPE": {"services": [], "modules": ["module:python:payments.service"], "files": ["payments/service.py"]},
                "EXPECTED_TESTS": [{"add_test_for": "symbol:python:payments.service.PaymentService.fee", "reason": "no existing test calls it"}]
            },
            "approval": {"approver": "payments-lead", "at": now - 550},
            "invalidation": null, "agentscript": "policy task_pay_184_v1 {\n    checkpoint baseline;\n    target --function PaymentService.fee;\n}\n"
        })
        .to_string(),
    );
    let delivery = [
        json!({"kind": "submitted", "session": session, "branch": "crane/PAY-184", "base": "main", "head": "a1b2c3d4e5f6", "checks": [{"name": "ruff", "kind": "lint", "status": "passed"}, {"name": "bandit", "kind": "security", "status": "failed"}], "seq": 1, "at": now - 120}),
        json!({"kind": "pull_request", "number": 184, "url": "https://github.com/acme/shop/pull/184", "seq": 2, "at": now - 110}),
        json!({"kind": "approval", "by": "payments-lead", "via": "slack", "seq": 3, "at": now - 90}),
    ];
    repository.write(
        &format!(".crane/runtime/delivery/{session}/journal.jsonl"),
        &(delivery
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"),
    );
    let before = snapshot(&repository.root.join(".crane"));

    let detail = repository.observe("/api/v1/dashboard/tasks/PAY-184");
    let header = &detail["header"];
    assert_eq!(header["title"], "Reject negative refunds");
    assert_eq!(header["branch"], "crane/PAY-184");
    assert_eq!(header["agents"], json!(["Claude Code"]));
    assert_eq!(header["status"], "REVIEW");
    assert_eq!(header["risk"]["level"], "critical");
    for key in [
        "repository",
        "providers",
        "start",
        "duration",
        "autonomy",
        "autonomy_score",
    ] {
        assert!(header.get(key).is_some(), "header lacks {key}");
    }

    // 1. Task and 2. Contract (as stored)
    assert_eq!(detail["task"]["source"], "jira");
    assert_eq!(
        detail["task"]["description"],
        "Refunds below zero must be rejected."
    );
    assert_eq!(detail["task"]["lifecycle"].as_array().unwrap().len(), 3);
    let contract = &detail["contract"];
    assert_eq!(contract["contract_id"], "PAY-184@v1");
    assert_eq!(contract["checkpoint"]["sha"], "abc123");
    assert_eq!(contract["zone_set_version"], "sha256:z1");
    assert_eq!(
        contract["must_not_change"][0]["qualified"],
        "PaymentService.charge"
    );
    assert_eq!(
        contract["task_scope"]["files"],
        json!(["payments/service.py"])
    );
    assert_eq!(contract["expected_tests"].as_array().unwrap().len(), 1);

    // 3. Agent and 4. Autonomy
    let agent = &detail["agents"]["sessions"][0];
    assert_eq!(detail["agents"]["session_count"], 1);
    assert_eq!(agent["agent_id"], "obs-1");
    assert_eq!(agent["models"], json!(["claude-opus-5-5"]));
    assert_eq!(agent["adapter_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(agent["safety"], "quarantined");
    let autonomy = &detail["autonomy"];
    assert_eq!(autonomy["initial"], "delegated");
    assert_eq!(autonomy["quarantines"][0]["kind"], "self_escalation");
    assert!(autonomy["budget"]["start"]["max"].is_number());

    // 5. Timeline: chronological, with what was read, written, decided, and why
    let events = detail["timeline"]["events"].as_array().unwrap();
    assert!(events
        .windows(2)
        .all(|pair| pair[0]["at"].as_u64() <= pair[1]["at"].as_u64()));
    let find = |kind: &str, decision: &str| {
        events
            .iter()
            .find(|event| {
                event["type"] == kind && (decision.is_empty() || event["decision"] == decision)
            })
            .unwrap_or_else(|| panic!("no {kind} {decision} in {events:?}"))
    };
    assert_eq!(
        find("READ", "ALLOW")["resource"],
        json!(["payments/service.py"])
    );
    let denied = find("WRITE", "DENY");
    assert_eq!(denied["policy"], json!(["payments/preserve"]));
    assert_eq!(denied["zone"], json!(["money"]));
    assert_eq!(denied["risk"], "high");
    assert_eq!(find("WRITE", "ASK")["result"], "approval_required");
    assert_eq!(find("AUTONOMY", "")["risk"], "critical");
    assert!(events
        .iter()
        .any(|event| event["type"] == "TASK" && event["result"] == "REVIEW"));

    // 6. Violations
    let violations = &detail["violations"];
    assert_eq!(
        (
            violations["total"].clone(),
            violations["critical"].clone(),
            violations["unresolved"].clone()
        ),
        (json!(1), json!(1), json!(1))
    );
    let violation = &violations["items"][0];
    assert_eq!(violation["kind"], "self_escalation");
    assert_eq!(violation["decision"], "deny");
    assert!(violation["consequence"]
        .as_str()
        .unwrap()
        .contains("quarantined"));
    assert_eq!(violation["resulting_state"]["safety"], "quarantined");

    // 7. Verification, 8. Delivery, 9. Evidence
    let verification = &detail["verification"][0];
    assert_eq!(
        verification["lint"],
        json!([{"name": "ruff", "status": "passed"}])
    );
    assert_eq!(
        verification["security"],
        json!([{"name": "bandit", "status": "failed"}])
    );
    assert!(verification["attestation"]["digest"].is_string());
    let delivered = &detail["delivery"]["deliveries"][0];
    assert_eq!(delivered["pr_status"], "approved");
    assert_eq!(delivered["pull_request"]["number"], 184);
    assert_eq!(delivered["approvals"][0]["by"], "payments-lead");
    assert_eq!(delivered["commit"], "a1b2c3d4e5f6");
    let kinds = detail["evidence"]["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reference| reference["kind"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    for kind in [
        "contract digest",
        "session journal",
        "attestation",
        "commit",
        "pull request",
        "delivery journal",
    ] {
        assert!(
            kinds.contains(&kind.to_string()),
            "no {kind} reference in {kinds:?}"
        );
    }
    assert!(!detail["evidence"]["notable_events"]
        .as_array()
        .unwrap()
        .is_empty());

    // Nothing local or secret leaks: no absolute path of the repository, no command text
    let text = detail.to_string();
    let root = repository.root.to_string_lossy().replace('\\', "/");
    assert!(
        !text.replace("\\\\", "/").contains(&root),
        "the answer exposes the local path"
    );
    assert!(
        !text.contains("--approver me"),
        "raw command arguments are never shown"
    );

    assert_eq!(
        repository
            .crane(&["observe", "/api/v1/dashboard/tasks/NOPE-1"], "")
            .status
            .code(),
        Some(1)
    );
    assert!(!repository
        .crane(&["observe", "/api/v1/dashboard/tasks/..%2F.."], "")
        .status
        .success());
    assert_eq!(snapshot(&repository.root.join(".crane")), before);
}

/** The Agents section: one row per agent (adapter profile and model), one aggregate per provider,
 * and Agent Details with everything the agent's sessions met, for a time window and filters,
 * read-only */
#[test]
fn agents_show_who_operates_and_how() {
    let repository = Repository::new();
    let start = json!({"session_id": "obs-1", "hook_event_name": "SessionStart", "source": "startup", "model": "claude-opus-5-5"});
    repository.crane(
        &[
            "agent",
            "hook",
            "--event",
            "session-start",
            "--profile",
            "claude",
        ],
        &start.to_string(),
    );
    repository.hook(
        "Edit",
        json!({"file_path": "catalog/labels.py", "old_string": "return name.strip()", "new_string": "return name"}),
    );
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount + self.fee(amount)", "new_string": "return amount"}),
    );
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount // 10", "new_string": "return amount // 20"}),
    );
    repository.hook(
        "Bash",
        json!({"command": "crane autonomy promote claude-obs-1 --to autonomous --approver me"}),
    );
    let session = repository.observe("/api/v1/sessions")["items"][0]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let now = now();
    task_record(
        &repository,
        "PAY-7",
        "EXECUTING",
        &[&session],
        &[("RECEIVED", now - 100)],
    );
    let before = snapshot(&repository.root.join(".crane"));

    let page = repository.observe("/api/v1/dashboard/agents?range=24h");
    let providers = page["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|provider| provider["provider"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(providers, ["Claude", "Codex", "Gemini", "Other"]);
    assert_eq!(page["providers"][0]["sessions"], 1);
    assert_eq!(page["providers"][0]["tasks"], 1);
    assert_eq!(page["providers"][1]["sessions"], 0);
    assert!(page["providers"][0]["violation_rate"].is_number());
    let agent = &page["agents"][0];
    assert_eq!(agent["agent_id"], "claude.claude-opus-5-5");
    assert_eq!(agent["name"], "Claude Code");
    assert_eq!(agent["provider"], "Claude");
    assert_eq!(agent["model"], "claude-opus-5-5");
    assert_eq!(agent["adapter_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(agent["status"], "active");
    assert_eq!(agent["safety"], "quarantined");
    assert_eq!(
        (
            agent["sessions"].clone(),
            agent["tasks"].clone(),
            agent["repositories"].clone()
        ),
        (json!(1), json!(1), json!(1))
    );
    assert_eq!(agent["critical_violations"], 1);
    assert_eq!(agent["human_intervention_rate"], 100.0);
    for key in [
        "autonomous_sessions",
        "success_rate",
        "violations",
        "average_autonomy_score",
        "average_budget_consumption",
        "average_task_duration",
        "last_active",
    ] {
        assert!(agent.get(key).is_some(), "agent row lacks {key}");
    }

    // Filters and the window
    let count = |path: &str| repository.observe(path)["agents"].as_array().unwrap().len();
    assert_eq!(
        count("/api/v1/dashboard/agents?range=24h&policy=payments"),
        1
    );
    assert_eq!(count("/api/v1/dashboard/agents?range=24h&task=PAY-7"), 1);
    assert_eq!(count("/api/v1/dashboard/agents?range=24h&task=PAY-404"), 0);
    assert_eq!(
        count("/api/v1/dashboard/agents?range=custom&from=1000&to=2000"),
        0
    );

    // Agent Details
    let detail = repository.observe("/api/v1/dashboard/agents/claude.claude-opus-5-5?range=24h");
    assert_eq!(detail["identity"]["provider"], "Claude");
    assert_eq!(detail["identity"]["provider_session_ids"], json!(["obs-1"]));
    assert_eq!(detail["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(detail["tasks"][0]["task_id"], "PAY-7");
    assert_eq!(detail["sessions"][0]["session_id"], session.as_str());
    let payments = detail["policies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|policy| policy["policy"] == "payments")
        .unwrap();
    assert_eq!(payments["deny"], 1);
    assert_eq!(payments["bound_sessions"], 1);
    let money = &detail["zones"][0];
    assert_eq!(
        (
            money["zone"].clone(),
            money["deny"].clone(),
            money["approval_required"].clone()
        ),
        (json!("money"), json!(1), json!(1))
    );
    assert_eq!(detail["violations"][0]["kind"], "self_escalation");
    assert!(detail["autonomy_history"]
        .as_array()
        .unwrap()
        .iter()
        .any(|change| change["to"]["safety"] == "quarantined"));
    assert_eq!(
        detail["budget_history"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(detail["verification_history"].as_array().unwrap().len(), 1);
    assert!(detail["delivery_history"].as_array().unwrap().is_empty());
    assert_eq!(
        repository
            .crane(&["observe", "/api/v1/dashboard/agents/codex.gpt-5"], "")
            .status
            .code(),
        Some(1)
    );
    assert_eq!(snapshot(&repository.root.join(".crane")), before);
}

/** Runs: one row per governed execution, filtered and paginated on the server; a run's timeline
 * from start to delivery and an inspection of each action that never shows raw arguments; the
 * policy and repository pages; and deep links that open the page at a view */
#[test]
fn runs_show_each_governed_execution() {
    let repository = Repository::new();
    let start = json!({"session_id": "obs-1", "hook_event_name": "SessionStart", "source": "startup", "model": "claude-opus-5-5"});
    repository.crane(
        &[
            "agent",
            "hook",
            "--event",
            "session-start",
            "--profile",
            "claude",
        ],
        &start.to_string(),
    );
    repository.hook("Read", json!({"file_path": "payments/service.py"}));
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount + self.fee(amount)", "new_string": "return amount"}),
    );
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount // 10", "new_string": "return amount // 20"}),
    );
    repository.hook(
        "Bash",
        json!({"command": "crane autonomy promote claude-obs-1 --to autonomous --approver me"}),
    );
    let run = repository.observe("/api/v1/sessions")["items"][0]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let now = now();
    task_record(
        &repository,
        "PAY-9",
        "REVIEW",
        &[&run],
        &[("RECEIVED", now - 100)],
    );
    repository.write(
        &format!(".crane/runtime/delivery/{run}/journal.jsonl"),
        &[
            json!({"kind": "submitted", "branch": "crane/PAY-9", "head": "f00dfeed", "seq": 1, "at": now - 50}),
            json!({"kind": "pull_request", "number": 9, "url": "https://github.com/acme/shop/pull/9", "seq": 2, "at": now - 40}),
            json!({"kind": "approval", "by": "lead", "seq": 3, "at": now - 30}),
            json!({"kind": "merged", "sha": "beefcafe", "by": "lead", "trusted_checkpoint": "baseline", "seq": 4, "at": now - 20}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n"),
    );
    let before = snapshot(&repository.root.join(".crane"));

    // The table
    let page = repository.observe("/api/v1/dashboard/runs?range=24h");
    assert_eq!(page["total"], 1);
    let row = &page["items"][0];
    assert_eq!(row["run_id"], run.as_str());
    assert_eq!(row["task_id"], "PAY-9");
    assert_eq!(row["agent"], "Claude Code");
    assert_eq!(row["provider"], "Claude");
    assert_eq!(row["safety"], "quarantined");
    assert_eq!(
        (
            row["actions"].clone(),
            row["allowed"].clone(),
            row["denied"].clone(),
            row["approvals"].clone()
        ),
        (json!(4), json!(1), json!(2), json!(1))
    );
    assert_eq!(row["pull_request"]["number"], 9);
    assert_eq!(row["merge"], "merged");
    for key in [
        "repository",
        "status",
        "autonomy",
        "start",
        "duration",
        "violations",
        "budget_consumed",
        "verification",
        "final_status",
    ] {
        assert!(row.get(key).is_some(), "run row lacks {key}");
    }
    let total = |path: &str| repository.observe(path)["total"].clone();
    for filter in [
        format!("session={run}"),
        "task=PAY-9".to_string(),
        "agent=claude".to_string(),
        "provider=Claude".to_string(),
        "policy=payments".to_string(),
        "zone=money".to_string(),
        "autonomy=delegated".to_string(),
        "safety=quarantined".to_string(),
        "session_status=active".to_string(),
        "severity=critical".to_string(),
        format!("repository={}", row["repository"].as_str().unwrap()),
    ] {
        assert_eq!(
            total(&format!("/api/v1/dashboard/runs?range=24h&{filter}")),
            1,
            "{filter}"
        );
    }
    assert_eq!(total("/api/v1/dashboard/runs?range=24h&provider=Codex"), 0);
    assert_eq!(
        total("/api/v1/dashboard/runs?range=custom&from=1000&to=2000"),
        0
    );
    assert_eq!(
        total(&format!("/api/v1/dashboard/runs?range=24h&q={run}")),
        1
    );
    assert_eq!(
        repository.observe("/api/v1/dashboard/runs?range=24h&page_size=1&page=2")["items"],
        json!([])
    );

    // A run: its timeline and the inspection of each action
    let detail = repository.observe(&format!("/api/v1/dashboard/runs/{run}"));
    let kinds = detail["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    for kind in [
        "session",
        "contract",
        "checkpoint",
        "agent",
        "decision",
        "violation",
        "budget",
        "autonomy",
        "delivery",
        "approval",
        "merge",
    ] {
        assert!(kinds.contains(kind), "the timeline lacks {kind}: {kinds:?}");
    }
    let times = detail["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["at"].as_u64().unwrap_or(0))
        .collect::<Vec<_>>();
    assert!(times.windows(2).all(|pair| pair[0] <= pair[1]));
    let actions = detail["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 4);
    let read = &actions[0];
    assert_eq!(read["normalized_action"], "read payments/service.py");
    let denied = actions
        .iter()
        .find(|action| action["decision"] == "deny" && action["tool"] == "Edit")
        .unwrap();
    assert_eq!(denied["policy"], json!(["payments/preserve"]));
    assert_eq!(denied["zone"], json!(["money"]));
    assert_eq!(denied["risk"], "high");
    assert_eq!(denied["resource"], json!(["payments/service.py"]));
    assert!(denied["action_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    for key in [
        "at",
        "tool",
        "action_type",
        "budget_impact",
        "execution_duration",
        "effect",
        "verification",
    ] {
        assert!(denied.get(key).is_some(), "the inspection lacks {key}");
    }
    let command = actions
        .iter()
        .find(|action| action["tool"] == "Bash")
        .unwrap();
    assert_eq!(command["normalized_action"], "run crane (7 arguments)");
    let text = detail.to_string();
    assert!(
        !text.contains("--approver") && !text.contains("promote claude-obs-1"),
        "raw arguments leak"
    );
    assert!(!text.contains("return amount // 20"), "edited text leaks");

    // Policy and repository pages
    let policy = repository.observe("/api/v1/dashboard/policies/payments?range=24h");
    assert_eq!(policy["metadata"]["status"], "active");
    assert_eq!(policy["runs"][0]["run_id"], run.as_str());
    assert_eq!(policy["counts"]["denials"], 1);
    assert_eq!(policy["zones"][0]["zone"], "money");
    assert_eq!(policy["tasks"][0]["task_id"], "PAY-9");
    let repository_id = row["repository_id"].as_str().unwrap().to_string();
    let home = repository.observe(&format!(
        "/api/v1/dashboard/repositories/{repository_id}?range=24h"
    ));
    assert_eq!(home["zones"][0]["zone_id"], "money");
    assert_eq!(home["policies"][0]["policy_id"], "payments");
    assert_eq!(home["health"]["sessions"], 1);
    assert!(!repository
        .crane(&["observe", "/api/v1/dashboard/policies/none"], "")
        .status
        .success());
    assert!(!repository
        .crane(&["observe", "/api/v1/dashboard/repositories/elsewhere"], "")
        .status
        .success());

    // Deep links open the page; anything else outside the API does not
    for path in [
        format!("/runs/{run}"),
        "/tasks/PAY-9".to_string(),
        "/agents/claude.claude-opus-5-5".to_string(),
        "/policies/payments".to_string(),
        format!("/repositories/{repository_id}"),
        "/runs".to_string(),
    ] {
        let (status, body) = repository.http("GET", &path, TOKEN);
        assert_eq!(status, 200, "{path}");
        assert!(body.contains("<title>Society Overview</title>"), "{path}");
    }
    assert_eq!(repository.http("GET", "/runs/../.crane", TOKEN).0, 404);
    assert_eq!(repository.http("GET", "/elsewhere", TOKEN).0, 404);
    assert_eq!(snapshot(&repository.root.join(".crane")), before);
}

/** Policies, zones, and violations: read-only catalogs of what governed the runs, with the
 * violations explorer's filters, severity trend, and top lists */
#[test]
fn policies_zones_and_violations_are_explained() {
    let repository = Repository::new();
    // The zones' resolution is what `crane zones` keeps; the dashboard only reads it
    repository.ok(&["zones"]);
    let start = json!({"session_id": "obs-1", "hook_event_name": "SessionStart", "source": "startup", "model": "claude-opus-5-5"});
    repository.crane(
        &[
            "agent",
            "hook",
            "--event",
            "session-start",
            "--profile",
            "claude",
        ],
        &start.to_string(),
    );
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount + self.fee(amount)", "new_string": "return amount"}),
    );
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount // 10", "new_string": "return amount // 20"}),
    );
    repository.hook(
        "Bash",
        json!({"command": "crane autonomy promote claude-obs-1 --to autonomous --approver me"}),
    );
    let run = repository.observe("/api/v1/sessions")["items"][0]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    task_record(
        &repository,
        "PAY-11",
        "EXECUTING",
        &[&run],
        &[("RECEIVED", now() - 100)],
    );
    let before = snapshot(&repository.root.join(".crane"));

    // Policies
    let policies = repository.observe("/api/v1/dashboard/policies?range=24h");
    let payments = &policies["policies"][0];
    assert_eq!(payments["policy_id"], "payments");
    assert_eq!(payments["type"], "repository");
    assert_eq!(payments["status"], "active");
    assert!(payments["version"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(payments["zone_scope"], json!(["money"]));
    assert_eq!(
        (
            payments["sessions"].clone(),
            payments["tasks"].clone(),
            payments["denied"].clone()
        ),
        (json!(1), json!(1), json!(1))
    );
    for key in [
        "name",
        "repository_scope",
        "allowed",
        "violations",
        "critical_violations",
        "last_triggered",
        "success_rate",
    ] {
        assert!(payments.get(key).is_some(), "policy row lacks {key}");
    }
    let policy = repository.observe("/api/v1/dashboard/policies/payments?range=24h");
    assert_eq!(
        policy["selectors"]["rules"],
        json!(["preserve --function PaymentService.charge scope block"])
    );
    assert_eq!(policy["counts"]["denials"], 1);
    assert_eq!(policy["counts"]["executions"], 1);
    assert_eq!(policy["zones"][0]["zone"], "money");
    assert_eq!(policy["task_distribution"][0]["task_id"], "PAY-11");
    assert_eq!(
        policy["agent_distribution"][0]["agent_id"],
        "claude.claude-opus-5-5"
    );
    assert_eq!(policy["false_positives"]["available"], false);
    assert!(!policy["timeline"].as_array().unwrap().is_empty());
    assert!(policy["definition"]
        .as_str()
        .unwrap()
        .contains("preserve --function PaymentService.charge"));
    assert!(!repository
        .crane(&["observe", "/api/v1/dashboard/policies/nothing"], "")
        .status
        .success());

    // Zones
    let zones = repository.observe("/api/v1/dashboard/zones?range=24h");
    let money = &zones["zones"][0];
    assert_eq!(money["zone_id"], "money");
    assert_eq!(money["criticality"], "critical");
    assert_eq!(money["resources"]["files"], 1);
    assert_eq!(money["active_sessions"], 1);
    assert_eq!(money["tasks"], 1);
    assert_eq!(money["autonomy"]["effective"], "assisted");
    assert!(money["last_triggered"].is_number());
    let zone = repository.observe("/api/v1/dashboard/zones/money?range=24h");
    assert_eq!(
        zone["resources"]["selectors"][0]["files"],
        json!(["payments/service.py"])
    );
    assert_eq!(zone["counts"]["denied"], 1);
    assert_eq!(zone["counts"]["approval_required"], 1);
    assert!(zone["history"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["kind"] == "decision"));

    // Violations
    let violations = repository.observe("/api/v1/dashboard/violations?range=24h");
    assert_eq!(violations["summary"]["critical"], 1);
    assert_eq!(violations["summary"]["unresolved"], 1);
    let row = &violations["items"][0];
    assert_eq!(row["kind"], "self_escalation");
    assert_eq!(row["task_id"], "PAY-11");
    assert_eq!(row["run_id"], run.as_str());
    assert_eq!(row["provider"], "Claude");
    assert_eq!(
        row["autonomy_impact"],
        "delegated / active → delegated / quarantined"
    );
    assert_eq!(row["resolution"], "unresolved");
    for key in [
        "severity",
        "agent",
        "repository",
        "policy",
        "zone",
        "resource",
        "action",
        "decision",
        "consequence",
        "budget_impact",
    ] {
        assert!(row.get(key).is_some(), "violation row lacks {key}");
    }
    assert!(violations["trend"]
        .as_array()
        .unwrap()
        .iter()
        .any(|point| point["critical"] == 1));
    assert_eq!(
        violations["top_agents"][0]["agent_id"],
        "claude.claude-opus-5-5"
    );
    assert_eq!(violations["top_repositories"][0]["count"], 1);
    let total = |path: &str| repository.observe(path)["total"].clone();
    assert_eq!(
        total("/api/v1/dashboard/violations?range=24h&severity=critical"),
        1
    );
    assert_eq!(
        total("/api/v1/dashboard/violations?range=24h&severity=high,medium,low"),
        0
    );
    assert_eq!(
        total("/api/v1/dashboard/violations?range=24h&status=unresolved"),
        1
    );
    assert_eq!(
        total("/api/v1/dashboard/violations?range=24h&status=resolved"),
        0
    );
    assert_eq!(
        total("/api/v1/dashboard/violations?range=24h&agent=codex"),
        0
    );
    assert_eq!(
        total(&format!(
            "/api/v1/dashboard/violations?range=24h&task=PAY-11&session={run}"
        )),
        1
    );
    assert_eq!(
        total("/api/v1/dashboard/violations?range=custom&from=1000&to=2000"),
        0
    );
    assert!(!repository
        .crane(
            &["observe", "/api/v1/dashboard/violations?status=fixed"],
            ""
        )
        .status
        .success());
    assert!(!repository
        .crane(
            &["observe", "/api/v1/dashboard/violations?severity=finding"],
            ""
        )
        .status
        .success());

    for path in ["/zones", "/zones/money", "/violations", "/policies"] {
        assert_eq!(repository.http("GET", path, TOKEN).0, 200, "{path}");
    }
    assert_eq!(snapshot(&repository.root.join(".crane")), before);
}

/** Repositories: the list with each repository's health, and Repository Details with its
 * overview, health metrics, activity over time, most active agents, most triggered policies,
 * highest-risk zones, and its tasks, agents, sessions, policies, zones, violations, deliveries,
 * and attestations, read-only */
#[test]
fn repositories_show_their_health() {
    let repository = Repository::new();
    repository.ok(&["repo", "connect", "--provider", "local"]);
    let start = json!({"session_id": "obs-1", "hook_event_name": "SessionStart", "source": "startup", "model": "claude-opus-5-5"});
    repository.crane(
        &[
            "agent",
            "hook",
            "--event",
            "session-start",
            "--profile",
            "claude",
        ],
        &start.to_string(),
    );
    repository.hook(
        "Edit",
        json!({"file_path": "catalog/labels.py", "old_string": "return name.strip()", "new_string": "return name"}),
    );
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount + self.fee(amount)", "new_string": "return amount"}),
    );
    repository.hook(
        "Bash",
        json!({"command": "crane autonomy promote claude-obs-1 --to autonomous --approver me"}),
    );
    let run = repository.observe("/api/v1/sessions")["items"][0]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let now = now();
    task_record(
        &repository,
        "PAY-21",
        "COMPLETED",
        &[&run],
        &[("RECEIVED", now - 300), ("COMPLETED", now - 10)],
    );
    repository.write(
        &format!(".crane/runtime/delivery/{run}/journal.jsonl"),
        &[
            json!({"kind": "submitted", "branch": "crane/PAY-21", "head": "f00dfeed", "seq": 1, "at": now - 50}),
            json!({"kind": "pull_request", "number": 21, "url": "https://example.com/pull/21", "seq": 2, "at": now - 40}),
            json!({"kind": "approval", "by": "lead", "seq": 3, "at": now - 30}),
            json!({"kind": "merged", "sha": "beefcafe", "by": "lead", "trusted_checkpoint": "baseline", "seq": 4, "at": now - 20}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n"),
    );
    let before = snapshot(&repository.root.join(".crane"));

    let list = repository.observe("/api/v1/dashboard/repositories?range=24h");
    let row = &list["repositories"][0];
    assert_eq!(row["provider"], "local");
    assert_eq!(row["status"], "connected");
    assert_eq!(row["trusted_checkpoint"]["name"], "baseline");
    assert_eq!(
        (
            row["tasks"].clone(),
            row["sessions"].clone(),
            row["agents"].clone(),
            row["active_agents"].clone()
        ),
        (json!(1), json!(1), json!(1), json!(1))
    );
    assert_eq!(
        (
            row["violations"].clone(),
            row["critical_violations"].clone()
        ),
        (json!(1), json!(1))
    );
    assert_eq!(
        (
            row["pull_requests"].clone(),
            row["merged_pull_requests"].clone()
        ),
        (json!(1), json!(1))
    );
    assert_eq!(
        row["autonomous_completion_rate"], 100.0,
        "no tool call of the run needed a human"
    );
    for key in [
        "repository",
        "default_branch",
        "verification_rate",
        "last_activity",
    ] {
        assert!(row.get(key).is_some(), "repository row lacks {key}");
    }

    let id = row["repository_id"].as_str().unwrap().to_string();
    let detail = repository.observe(&format!("/api/v1/dashboard/repositories/{id}?range=24h"));
    let health = &detail["health"];
    assert_eq!(health["manually_intervened_tasks"], 0);
    assert_eq!(health["autonomous_tasks"], 1);
    assert_eq!(health["successful_tasks"], 1);
    assert_eq!(health["failed_tasks"], 0);
    assert_eq!(health["critical_violations"], 1);
    assert_eq!(health["pr_success_rate"], 100.0);
    assert_eq!(health["merge_rate"], 100.0);
    assert_eq!(health["average_task_duration"], 290.0);
    for key in [
        "verification_failures",
        "violations",
        "average_autonomy_score",
        "average_budget_consumption",
    ] {
        assert!(health.get(key).is_some(), "health lacks {key}");
    }
    let activity = detail["activity"].as_array().unwrap();
    let sum = |key: &str| {
        activity
            .iter()
            .map(|point| point[key].as_u64().unwrap())
            .sum::<u64>()
    };
    assert_eq!(
        (sum("runs"), sum("decisions"), sum("denied"), sum("merged")),
        (1, 3, 2, 1)
    );
    assert_eq!(
        detail["most_active_agents"][0]["agent_id"],
        "claude.claude-opus-5-5"
    );
    assert_eq!(
        detail["most_triggered_policies"][0]["policy_id"],
        "payments"
    );
    assert_eq!(detail["highest_risk_zones"][0]["zone_id"], "money");
    for section in [
        "tasks",
        "agents",
        "sessions",
        "policies",
        "zones",
        "violations",
        "deliveries",
        "attestations",
    ] {
        assert!(
            !detail[section].as_array().unwrap().is_empty(),
            "{section} is empty"
        );
    }
    assert_eq!(detail["deliveries"][0]["status"], "merged");
    assert!(detail["attestations"][0]["digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(!repository
        .crane(&["observe", "/api/v1/dashboard/repositories/elsewhere"], "")
        .status
        .success());
    assert_eq!(repository.http("GET", "/repositories", TOKEN).0, 200);
    assert_eq!(snapshot(&repository.root.join(".crane")), before);
}

/** Autonomy and risk: the autonomy actually exercised and how safe it was, from recorded state
 * only (the decided autonomy of each action, the budget replayed through its model, decisions,
 * violations, quarantines), with documented formulas and no way to change anything */
#[test]
fn autonomy_and_risk_are_observed() {
    let repository = Repository::new();
    let start = json!({"session_id": "obs-1", "hook_event_name": "SessionStart", "source": "startup", "model": "claude-opus-5-5"});
    repository.crane(
        &[
            "agent",
            "hook",
            "--event",
            "session-start",
            "--profile",
            "claude",
        ],
        &start.to_string(),
    );
    // An allowed change that runs (it consumes budget), then a human-reported milestone that
    // regenerates some
    let write =
        json!({"file_path": "catalog/labels.py", "content": "def label(name):\n    return name\n"});
    assert!(repository.hook("Write", write.clone()).stdout.is_empty());
    repository.write("catalog/labels.py", "def label(name):\n    return name\n");
    let after = json!({"session_id": "obs-1", "hook_event_name": "PostToolUse", "tool_name": "Write", "tool_input": write, "tool_response": {"success": true}});
    assert!(repository
        .crane(
            &[
                "agent",
                "hook",
                "--event",
                "post-tool-use",
                "--profile",
                "claude"
            ],
            &after.to_string()
        )
        .status
        .success());
    repository.ok(&[
        "autonomy",
        "credit",
        "claude-obs-1",
        "--event",
        "human_review",
        "--reference",
        "PR-1",
        "--approver",
        "lead",
    ]);
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount + self.fee(amount)", "new_string": "return amount"}),
    );
    repository.hook(
        "Edit",
        json!({"file_path": "payments/service.py", "old_string": "return amount // 10", "new_string": "return amount // 20"}),
    );
    repository.hook(
        "Bash",
        json!({"command": "crane autonomy promote claude-obs-1 --to autonomous --approver me"}),
    );
    task_record(
        &repository,
        "PAY-31",
        "EXECUTING",
        &["claude-obs-1"],
        &[("RECEIVED", now() - 100)],
    );
    let before = snapshot(&repository.root.join(".crane"));

    let page = repository.observe("/api/v1/dashboard/autonomy?range=24h");
    let metrics = &page["metrics"];
    assert_eq!(
        metrics["autonomy_score"], 66.7,
        "every change was decided at delegated"
    );
    assert_eq!(
        metrics["autonomy_mode"]["active_runs"],
        json!({"delegated": 1})
    );
    assert_eq!(
        metrics["safety_state"]["active_runs"],
        json!({"quarantined": 1})
    );
    assert!(
        metrics["budget_consumed"].as_u64().unwrap() > 0,
        "{metrics}"
    );
    assert!(
        metrics["budget_regeneration"].as_u64().unwrap() > 0,
        "{metrics}"
    );
    assert_eq!(metrics["mutating_actions"], 4);
    assert_eq!(metrics["denied_action_rate"], 50.0);
    assert_eq!(metrics["human_intervention_rate"], 100.0);
    assert_eq!(
        (
            metrics["violation_rate"].clone(),
            metrics["critical_violation_rate"].clone(),
            metrics["quarantine_rate"].clone()
        ),
        (json!(100.0), json!(100.0), json!(100.0))
    );
    for key in [
        "budget_remaining",
        "verification_failure_rate",
        "autonomous_completion_rate",
    ] {
        assert!(metrics.get(key).is_some(), "metrics lack {key}");
    }
    for key in [
        "autonomy_score",
        "budget_regeneration",
        "quarantine_rate",
        "autonomous_completion_rate",
    ] {
        assert!(page["definitions"][key].is_string(), "no formula for {key}");
    }
    let sum = |key: &str| {
        page[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|point| point["v"].as_u64().unwrap_or(0))
            .sum::<u64>()
    };
    assert_eq!(
        sum("budget_consumed_over_time"),
        metrics["budget_consumed"].as_u64().unwrap()
    );
    assert_eq!(
        sum("budget_regeneration_over_time"),
        metrics["budget_regeneration"].as_u64().unwrap()
    );
    assert!(page["score_over_time"]
        .as_array()
        .unwrap()
        .iter()
        .any(|point| point["v"] == 66.7));
    assert_eq!(page["by_agent"][0]["delegated"], 4);
    assert_eq!(page["by_task"][0]["name"], "PAY-31");
    assert!(!page["by_repository"].as_array().unwrap().is_empty());
    let delegated = page["against_autonomy"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["autonomy"] == "delegated")
        .unwrap();
    assert_eq!(
        (
            delegated["actions"].clone(),
            delegated["denied"].clone(),
            delegated["approval_requests"].clone(),
            delegated["violations"].clone()
        ),
        (json!(4), json!(2), json!(1), json!(1))
    );
    assert_eq!(page["quarantine_events"][0]["kind"], "self_escalation");
    assert!(page["degradation_events"].as_array().unwrap().is_empty());

    // Narrowing by filters and window
    let other = repository.observe("/api/v1/dashboard/autonomy?range=24h&agent=codex");
    assert_eq!(other["metrics"]["runs"], 0);
    assert!(other["metrics"]["autonomy_score"].is_null());
    let old = repository.observe("/api/v1/dashboard/autonomy?range=custom&from=1000&to=2000");
    assert_eq!(old["metrics"]["mutating_actions"], 0);
    assert_eq!(repository.http("GET", "/autonomy", TOKEN).0, 200);
    assert_eq!(
        repository
            .http("POST", "/api/v1/dashboard/autonomy", TOKEN)
            .0,
        405
    );
    assert_eq!(snapshot(&repository.root.join(".crane")), before);
}

/** A long-running observability (or control-plane) server for tests that make many requests,
 * stopped on drop
 * Fields
    - process: std::process::Child - the server
    - address: String - host:port it listens on
*/
struct Server {
    process: std::process::Child,
    address: String,
}

impl Server {
    /** Start `crane observe serve` (or `crane dashboard serve`) in a repository
     * Input
        - repository: &Repository - where to serve
        - site: &str - "observe" or "dashboard"
        - token: &str - the operator token
     * Output
        - Server
    */
    fn start(repository: &Repository, site: &str, token: &str) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
        command
            .args([site, "serve", "--addr", "127.0.0.1:0"])
            .current_dir(&repository.root)
            .env(
                if site == "observe" {
                    "CRANE_OBSERVE_TOKEN"
                } else {
                    "CRANE_DASHBOARD_TOKEN"
                },
                token,
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for marker in AGENT_MARKERS {
            command.env_remove(marker);
        }
        let mut process = command.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(process.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line
            .split("http://")
            .nth(1)
            .unwrap_or_else(|| panic!("no address in {line:?}"))
            .split('/')
            .next()
            .unwrap()
            .to_string();
        Self { process, address }
    }

    /** Send one request
     * Input
        - method: &str - HTTP method
        - path: &str - path and query
        - token: &str - X-Crane-Token ("" for none)
        - headers: &[(&str, &str)] - other headers
        - body: &str - request body
     * Output
        - (u16, String) status and body
    */
    fn send(
        &self,
        method: &str,
        path: &str,
        token: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, String) {
        let mut stream = TcpStream::connect(&self.address).unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\n", self.address);
        if !token.is_empty() {
            request.push_str(&format!("X-Crane-Token: {token}\r\n"));
        }
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .unwrap_or(0);
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        (status, body)
    }

    /** GET a path with a token, requiring 200, as JSON
     * Input
        - path: &str - path and query
        - token: &str - X-Crane-Token
     * Output
        - Value
    */
    fn json(&self, path: &str, token: &str) -> Value {
        let (status, body) = self.send("GET", path, token, &[], "");
        assert_eq!(status, 200, "GET {path}: {body}");
        serde_json::from_str(&body).unwrap()
    }
}

impl Drop for Server {
    /** Stop the server
     * Input
        - None (uses self)
     * Output
        - None
    */
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/** Run one agent tool call through the Claude Code hook in a named session
 * Input
    - repository: &Repository - repository
    - session: &str - the agent's own session id
    - event: &str - hook event (session-start, pre-tool-use)
    - payload: Value - hook payload without the session id
 * Output
    - Output
*/
fn hook_as(repository: &Repository, session: &str, event: &str, mut payload: Value) -> Output {
    payload["session_id"] = json!(session);
    repository.crane(
        &["agent", "hook", "--event", event, "--profile", "claude"],
        &payload.to_string(),
    )
}

/** Register a viewer through the CLI and return its token (printed once)
 * Input
    - repository: &Repository - repository
    - args: &[&str] - viewer add arguments after "add"
 * Output
    - String
*/
fn viewer(repository: &Repository, args: &[&str]) -> String {
    let mut full = vec!["observe", "viewer", "add"];
    full.extend_from_slice(args);
    let output = repository.ok(&full);
    let token = output.lines().last().unwrap().trim().to_string();
    assert_eq!(token.len(), 64, "{output}");
    token
}

/** Two teams' sessions in one repository: payments and catalog, each with a task; a session
 * records the team named in .crane/organization.json when it starts
 * Input
    - repository: &Repository - repository
 * Output
    - (String, String) the payments and the catalog session ids
*/
fn two_teams(repository: &Repository) -> (String, String) {
    let start =
        json!({"hook_event_name": "SessionStart", "source": "startup", "model": "claude-opus-5-5"});
    repository.write(
        ".crane/organization.json",
        r#"{"organization": "acme", "team": "payments"}"#,
    );
    hook_as(repository, "pay-1", "session-start", start.clone());
    hook_as(
        repository,
        "pay-1",
        "pre-tool-use",
        json!({"hook_event_name": "PreToolUse", "tool_name": "Edit", "tool_input": {"file_path": "payments/service.py", "old_string": "return amount + self.fee(amount)", "new_string": "return amount"}}),
    );
    repository.write(
        ".crane/organization.json",
        r#"{"organization": "acme", "team": "catalog"}"#,
    );
    hook_as(repository, "cat-1", "session-start", start);
    hook_as(
        repository,
        "cat-1",
        "pre-tool-use",
        json!({"hook_event_name": "PreToolUse", "tool_name": "Edit", "tool_input": {"file_path": "catalog/labels.py", "old_string": "return name.strip()", "new_string": "return name"}}),
    );
    hook_as(
        repository,
        "cat-1",
        "pre-tool-use",
        json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "crane autonomy promote claude-cat-1 --to autonomous --approver me"}}),
    );
    let now = now();
    task_record(
        repository,
        "PAY-1",
        "EXECUTING",
        &["claude-pay-1"],
        &[("RECEIVED", now - 100)],
    );
    task_record(
        repository,
        "CAT-1",
        "EXECUTING",
        &["claude-cat-1"],
        &[("RECEIVED", now - 100)],
    );
    ("claude-pay-1".into(), "claude-cat-1".into())
}

/** Read authorization: a viewer reads only its organizations, repositories, and teams; every
 * session, task, decision, violation, and filter value of another tenant is invisible, a record
 * it may not read answers exactly like one that does not exist (no IDOR, no existence oracle),
 * filters cannot widen the scope, and unknown, revoked, or other-tenant credentials are refused */
#[test]
fn viewers_read_only_their_tenant() {
    let repository = Repository::new();
    let (payments, catalog) = two_teams(&repository);
    let pay = viewer(
        &repository,
        &[
            "pay",
            "--organization",
            "acme",
            "--repository",
            "*",
            "--team",
            "payments",
        ],
    );
    let everyone = viewer(
        &repository,
        &["everyone", "--organization", "acme", "--repository", "*"],
    );
    let elsewhere = viewer(
        &repository,
        &[
            "elsewhere",
            "--organization",
            "acme",
            "--repository",
            "other/shop",
        ],
    );
    let globex = viewer(
        &repository,
        &["globex", "--organization", "globex", "--repository", "*"],
    );

    // The registry keeps digests only; listing never shows a token
    let registry =
        fs::read_to_string(repository.root.join(".crane/runtime/observe/viewers.json")).unwrap();
    for token in [&pay, &everyone, &elsewhere, &globex] {
        assert!(!registry.contains(token.as_str()));
    }
    let listed = repository.ok(&["observe", "viewers"]);
    assert!(
        listed.contains("pay: organizations acme") && listed.contains("teams payments"),
        "{listed}"
    );
    assert!(!listed.contains(&pay));
    // Reserved names, duplicates, and missing scopes are refused
    for args in [
        vec![
            "observe",
            "viewer",
            "add",
            "operator",
            "--organization",
            "acme",
            "--repository",
            "*",
        ],
        vec![
            "observe",
            "viewer",
            "add",
            "pay",
            "--organization",
            "acme",
            "--repository",
            "*",
        ],
        vec![
            "observe",
            "viewer",
            "add",
            "nobody-else",
            "--repository",
            "*",
        ],
    ] {
        assert!(!repository.crane(&args, "").status.success(), "{args:?}");
    }

    let server = Server::start(&repository, "observe", TOKEN);
    let before = snapshot(&repository.root);

    // The operator and an all-teams viewer see both teams
    for token in [TOKEN, everyone.as_str()] {
        assert_eq!(server.json("/api/v1/sessions", token)["total"], 2);
        assert_eq!(
            server.json("/api/v1/dashboard/runs?range=24h", token)["total"],
            2
        );
    }
    let whoami = server.json("/api/v1/whoami", &pay);
    assert_eq!(whoami["viewer"], "pay");
    assert_eq!(whoami["teams"], json!(["payments"]));
    assert_eq!(whoami["repository"]["readable"], true);
    assert!(
        !whoami.to_string().contains(&pay),
        "whoami never echoes the credential"
    );

    // The payments viewer: its own team only, on every page
    let sessions = server.json("/api/v1/sessions", &pay);
    assert_eq!(sessions["total"], 1);
    assert_eq!(sessions["items"][0]["session_id"], payments.as_str());
    assert_eq!(
        server.json("/api/v1/dashboard/runs?range=24h", &pay)["total"],
        1
    );
    let tasks = server.json("/api/v1/dashboard/tasks?range=24h", &pay);
    assert_eq!(
        tasks["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["task_id"].clone())
            .collect::<Vec<_>>(),
        vec![json!("PAY-1")]
    );
    let overview = server.json("/api/v1/dashboard/overview?range=24h", &pay);
    let metric = |key: &str| {
        overview["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|metric| metric["key"] == key)
            .unwrap()["value"]
            .clone()
    };
    assert_eq!(metric("active_sessions"), 1);
    assert_eq!(
        metric("critical_violations"),
        0,
        "the catalog session's critical violation is not the payments viewer's"
    );
    let agent = format!(
        "/api/v1/dashboard/agents/{}",
        server.json("/api/v1/sessions", &pay)["items"][0]["agent_id"]
            .as_str()
            .unwrap()
    );
    for path in [
        "/api/v1/overview",
        "/api/v1/agents",
        "/api/v1/tasks",
        "/api/v1/decisions",
        "/api/v1/violations",
        "/api/v1/audit",
        "/api/v1/dashboard/overview?range=24h",
        "/api/v1/dashboard/tasks?range=24h",
        "/api/v1/dashboard/runs?range=24h",
        "/api/v1/dashboard/agents?range=24h",
        agent.as_str(),
        "/api/v1/dashboard/policies?range=24h",
        "/api/v1/dashboard/policies/payments?range=24h",
        "/api/v1/dashboard/zones?range=24h",
        "/api/v1/dashboard/zones/money?range=24h",
        "/api/v1/dashboard/violations?range=24h",
        "/api/v1/dashboard/autonomy?range=24h",
        "/api/v1/dashboard/repositories?range=24h",
    ] {
        let body = server.json(path, &pay).to_string();
        assert!(
            !body.contains(&catalog) && !body.contains("cat-1") && !body.contains("CAT-1"),
            "{path} leaks the other team: {body}"
        );
        assert!(
            !body.contains("\"catalog\""),
            "{path} leaks the other team's name"
        );
    }

    // IDOR: another tenant's record answers exactly like a record that does not exist
    for template in [
        "/api/v1/sessions/{}",
        "/api/v1/sessions/{}/actions",
        "/api/v1/sessions/{}/autonomy",
        "/api/v1/dashboard/runs/{}",
        "/api/v1/dashboard/runs/{}/events",
    ] {
        let hidden = server.send("GET", &template.replace("{}", &catalog), &pay, &[], "");
        let missing = server.send(
            "GET",
            &template.replace("{}", "claude-none-1"),
            &pay,
            &[],
            "",
        );
        assert_eq!(hidden.0, 404, "{template}: {}", hidden.1);
        assert_eq!(missing.0, 404);
        assert_eq!(
            hidden.1.replace(&catalog, "ID"),
            missing.1.replace("claude-none-1", "ID"),
            "{template}"
        );
        assert_eq!(
            server
                .send("GET", &template.replace("{}", &catalog), TOKEN, &[], "")
                .0,
            200,
            "{template}"
        );
    }
    for template in ["/api/v1/tasks/{}", "/api/v1/dashboard/tasks/{}"] {
        let hidden = server.send("GET", &template.replace("{}", "CAT-1"), &pay, &[], "");
        let missing = server.send("GET", &template.replace("{}", "NONE-1"), &pay, &[], "");
        assert_eq!((hidden.0, missing.0), (404, 404), "{template}");
        assert_eq!(
            hidden.1.replace("CAT-1", "ID"),
            missing.1.replace("NONE-1", "ID")
        );
        assert_eq!(
            server
                .send("GET", &template.replace("{}", "CAT-1"), TOKEN, &[], "")
                .0,
            200
        );
    }

    // Filters narrow within the scope; they never widen it
    for path in [
        "/api/v1/dashboard/runs?range=24h&team=catalog".to_string(),
        format!("/api/v1/dashboard/runs?range=24h&session={catalog}"),
        "/api/v1/sessions?task=CAT-1".to_string(),
        format!("/api/v1/decisions?session={catalog}"),
        format!("/api/v1/violations?session={catalog}"),
        format!("/api/v1/dashboard/violations?range=24h&session={catalog}"),
    ] {
        assert_eq!(server.json(&path, &pay)["total"], 0, "{path}");
    }

    // Another repository or organization reads nothing; whoami says so
    for token in [&elsewhere, &globex] {
        for path in [
            "/api/v1/overview",
            "/api/v1/dashboard/overview?range=24h",
            &format!("/api/v1/sessions/{payments}"),
            "/api/v1/audit",
        ] {
            let (status, body) = server.send("GET", path, token, &[], "");
            assert_eq!(status, 403, "{path}: {body}");
            assert!(!body.contains(&payments));
        }
        assert_eq!(
            server.json("/api/v1/whoami", token)["repository"]["readable"],
            false
        );
        assert_eq!(
            server.send("GET", "/api/v1/endpoints", token, &[], "").0,
            200
        );
    }

    // Unknown, short, and missing credentials are refused before anything is read
    for token in ["wrong-token-000000000000000000", "short", ""] {
        assert_eq!(
            server.send("GET", "/api/v1/sessions", token, &[], "").0,
            401,
            "{token:?}"
        );
    }
    // Revoking a viewer takes effect on its next request
    repository.ok(&["observe", "viewer", "remove", "pay"]);
    assert_eq!(server.send("GET", "/api/v1/sessions", &pay, &[], "").0, 401);
    assert_eq!(
        server.send("GET", "/api/v1/sessions", &everyone, &[], "").0,
        200
    );

    // Reading changed nothing but the registry the CLI itself edited
    let mut after = snapshot(&repository.root);
    let mut expected = before;
    for map in [&mut after, &mut expected] {
        map.retain(|path, _| {
            !path
                .replace('\\', "/")
                .ends_with("runtime/observe/viewers.json")
        });
    }
    assert_eq!(after, expected);

    // Granting read access is a human's: refused in an agent environment and to agents' hooks
    let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
    let refused = command
        .args([
            "observe",
            "viewer",
            "add",
            "agent",
            "--organization",
            "acme",
            "--repository",
            "*",
        ])
        .current_dir(&repository.root)
        .env("CRANE_AGENT", "1")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let hooked = repository.hook(
        "Bash",
        json!({"command": "crane observe viewer add agent --organization acme --repository '*'"}),
    );
    assert_eq!(
        hooked.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&hooked.stdout)
    );
}

/** The dashboard is technically read-only: a dashboard user (the operator or any viewer) cannot
 * modify a policy or a zone, change autonomy or a budget, create an exception, start or stop an
 * agent, execute a command, modify a task or a repository, merge a PR, or change a checkpoint;
 * every attempt is refused by the server and changes no byte of the repository (records, working
 * tree, and Git), and the observability credentials are refused by the control plane */
#[test]
fn the_dashboard_cannot_change_anything() {
    let repository = Repository::new();
    let (session, _) = two_teams(&repository);
    let viewer_token = viewer(
        &repository,
        &["reader", "--organization", "acme", "--repository", "*"],
    );
    let server = Server::start(&repository, "observe", TOKEN);
    let before = snapshot(&repository.root);
    let state = |token: &str| {
        (
            server.json(&format!("/api/v1/sessions/{session}"), token)["autonomy"].clone(),
            server.json(&format!("/api/v1/sessions/{session}"), token)["budget"].clone(),
            server.json("/api/v1/dashboard/tasks/PAY-1", token)["task"]["state"].clone(),
            server.json("/api/v1/dashboard/policies/payments?range=24h", token)["digest"].clone(),
        )
    };
    let known = state(TOKEN);
    assert!(
        !known.0.is_null() && !known.1.is_null() && known.2 == "EXECUTING" && known.3.is_string(),
        "{known:?}"
    );

    let attempts: &[(&str, &[(&str, &str)])] = &[
        (
            "modify a policy",
            &[
                ("PUT", "/api/v1/dashboard/policies/payments"),
                ("PATCH", "/api/v1/dashboard/policies/payments"),
                ("DELETE", "/api/v1/dashboard/policies/payments"),
                ("POST", "/api/v1/policies"),
                ("POST", "/api/v1/dashboard/policies/payments/approve"),
            ],
        ),
        (
            "modify a zone",
            &[
                ("PUT", "/api/v1/dashboard/zones/money"),
                ("DELETE", "/api/v1/dashboard/zones/money"),
                ("POST", "/api/v1/zones/money/approve"),
            ],
        ),
        (
            "change autonomy",
            &[
                ("POST", "/api/v1/sessions/SESSION/autonomy"),
                ("PUT", "/api/v1/sessions/SESSION/autonomy"),
                ("POST", "/api/v1/autonomy/promote"),
            ],
        ),
        (
            "change budget",
            &[
                ("POST", "/api/v1/budget/refill"),
                ("PATCH", "/api/v1/sessions/SESSION"),
                ("POST", "/api/v1/sessions/SESSION/budget"),
            ],
        ),
        (
            "create an exception",
            &[
                ("POST", "/api/v1/exceptions"),
                ("POST", "/api/v1/delivery/SESSION/exception"),
            ],
        ),
        (
            "start an agent",
            &[
                ("POST", "/api/v1/sessions"),
                ("POST", "/api/v1/agents/start"),
                ("PUT", "/api/v1/dashboard/agents/claude.claude-opus-5-5"),
            ],
        ),
        (
            "stop an agent",
            &[
                ("DELETE", "/api/v1/sessions/SESSION"),
                ("POST", "/api/v1/dashboard/runs/SESSION/stop"),
                ("POST", "/api/v1/agents/claude/stop"),
            ],
        ),
        (
            "execute a command",
            &[
                ("POST", "/api/v1/commands"),
                ("POST", "/api/v1/exec"),
                ("POST", "/api/v1/sql"),
            ],
        ),
        (
            "modify a task",
            &[
                ("PUT", "/api/v1/dashboard/tasks/PAY-1"),
                ("PATCH", "/api/v1/tasks/PAY-1"),
                ("DELETE", "/api/v1/tasks/PAY-1"),
            ],
        ),
        (
            "modify a repository",
            &[
                ("PUT", "/api/v1/dashboard/repositories/REPO"),
                ("POST", "/api/v1/repositories"),
                ("DELETE", "/api/v1/repositories/REPO"),
            ],
        ),
        (
            "merge a PR",
            &[
                ("POST", "/api/v1/delivery/SESSION/merge"),
                ("PUT", "/api/v1/pulls/1/merge"),
                ("POST", "/api/v1/dashboard/runs/SESSION/merge"),
            ],
        ),
        (
            "change a checkpoint",
            &[
                ("POST", "/api/v1/checkpoints"),
                ("PUT", "/api/v1/checkpoints/baseline"),
                ("DELETE", "/api/v1/checkpoints/baseline"),
            ],
        ),
    ];
    let repository_id = server.json("/api/v1/dashboard/repositories?range=24h", TOKEN)
        ["repositories"][0]["repository_id"]
        .as_str()
        .unwrap()
        .to_string();
    let body = r#"{"to": "autonomous", "approver": "me", "command": "git push --force", "content": "policy payments {}"}"#;
    for (operation, requests) in attempts {
        for (method, path) in *requests {
            let path = path
                .replace("SESSION", &session)
                .replace("REPO", &repository_id);
            for token in [TOKEN, viewer_token.as_str()] {
                let (status, answer) = server.send(
                    method,
                    &path,
                    token,
                    &[("Content-Type", "application/json")],
                    body,
                );
                assert_eq!(
                    status, 405,
                    "{operation}: {method} {path} answered {status} {answer}"
                );
                // Methods are compared exactly: a lower-case spelling is refused the same way
                assert_eq!(
                    server.send(&method.to_lowercase(), &path, token, &[], "").0,
                    405
                );
            }
        }
    }
    // A GET cannot be turned into a change: action parameters are unknown, bodies are refused,
    // method-override headers are ignored, and no endpoint executes or edits anything
    for path in [
        format!("/api/v1/sessions/{session}/autonomy?to=autonomous"),
        "/api/v1/dashboard/policies/payments?action=edit".to_string(),
        "/api/v1/decisions?sql=select%201".to_string(),
        "/api/v1/dashboard/tasks?range=24h&state=COMPLETED".to_string(),
        format!("/api/v1/dashboard/runs/{session}?merge=true"),
    ] {
        assert_eq!(server.send("GET", &path, TOKEN, &[], "").0, 400, "{path}");
    }
    for path in [
        "/api/v1/exec?command=ls",
        "/api/v1/policies/payments/edit",
        "/api/v1/checkpoints/baseline",
    ] {
        assert_eq!(server.send("GET", path, TOKEN, &[], "").0, 404, "{path}");
    }
    assert_eq!(
        server.send("GET", "/api/v1/overview", TOKEN, &[], body).0,
        400,
        "a GET with a body"
    );
    assert_eq!(
        server
            .send(
                "GET",
                "/api/v1/overview",
                TOKEN,
                &[("X-HTTP-Method-Override", "DELETE")],
                ""
            )
            .0,
        200,
        "an override header changes nothing: the request stays a read"
    );
    for method in ["HEAD", "OPTIONS", "TRACE", "CONNECT", "PROPFIND"] {
        assert_eq!(
            server.send(method, "/api/v1/overview", TOKEN, &[], "").0,
            405,
            "{method}"
        );
    }

    // Nothing changed: not the records, the working tree, Git, or what the dashboard reports
    assert_eq!(snapshot(&repository.root), before);
    assert_eq!(state(TOKEN), known);

    // The control plane refuses observability credentials (read access never implies control)
    let control = Server::start(&repository, "dashboard", "control-plane-token-0123456789");
    for token in [TOKEN, viewer_token.as_str()] {
        for (method, path) in [
            ("POST", "/api/connection"),
            ("GET", "/api/connection"),
            ("POST", "/api/contracts"),
        ] {
            assert_eq!(
                control.send(method, path, token, &[], "{}").0,
                401,
                "{method} {path}"
            );
        }
    }
    drop(control);
    assert_eq!(snapshot(&repository.root), before);
}

/** SHA-256 of bytes as "sha256:hex" (the runtime's digest form), for building journals in tests
 * Input
    - bytes: &[u8] - data
 * Output
    - String
*/
fn sha256(bytes: &[u8]) -> String {
    const ROUND: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = bytes.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((bytes.len() as u64) * 8).to_be_bytes());
    for block in message.chunks(64) {
        let mut words = [0u32; 64];
        for (index, chunk) in block.chunks(4).enumerate() {
            words[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for index in 16..64 {
            let early = words[index - 15];
            let late = words[index - 2];
            words[index] = words[index - 16]
                .wrapping_add(early.rotate_right(7) ^ early.rotate_right(18) ^ (early >> 3))
                .wrapping_add(words[index - 7])
                .wrapping_add(late.rotate_right(17) ^ late.rotate_right(19) ^ (late >> 10));
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let first = h
                .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
                .wrapping_add((e & f) ^ (!e & g))
                .wrapping_add(ROUND[index])
                .wrapping_add(words[index]);
            let second = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                .wrapping_add((a & b) ^ (a & c) ^ (b & c));
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(first);
            d = c;
            c = b;
            b = a;
            a = first.wrapping_add(second);
        }
        for (value, added) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *value = value.wrapping_add(added);
        }
    }
    format!(
        "sha256:{}",
        state
            .iter()
            .map(|word| format!("{word:08x}"))
            .collect::<String>()
    )
}

/** Fill a repository with a large history: one real session recorded through the hooks, then
 * replicated as `sessions` sessions of `events` journal events each (re-sequenced, re-timed, and
 * re-chained exactly as the runtime chains them, so every journal verifies), across three agent
 * profiles, forty models, and four teams, plus `tasks` task records (a share linked to sessions)
 * Input
    - repository: &Repository - repository
    - sessions: usize - sessions
    - events: usize - journal events per session
    - tasks: usize - task records
 * Output
    - Vec<String> the session ids
*/
fn large_history(
    repository: &Repository,
    sessions: usize,
    events: usize,
    tasks: usize,
) -> Vec<String> {
    repository.write(".crane/organization.json", r#"{"organization": "acme"}"#);
    let start =
        json!({"hook_event_name": "SessionStart", "source": "startup", "model": "claude-opus-5-5"});
    hook_as(repository, "tpl-1", "session-start", start);
    let calls = [
        json!({"tool_name": "Read", "tool_input": {"file_path": "payments/service.py"}}),
        json!({"tool_name": "Edit", "tool_input": {"file_path": "catalog/labels.py", "old_string": "return name.strip()", "new_string": "return name"}}),
        json!({"tool_name": "Edit", "tool_input": {"file_path": "payments/service.py", "old_string": "return amount + self.fee(amount)", "new_string": "return amount"}}),
        json!({"tool_name": "Edit", "tool_input": {"file_path": "payments/service.py", "old_string": "return amount // 10", "new_string": "return amount // 20"}}),
        json!({"tool_name": "Grep", "tool_input": {"pattern": "fee", "path": "payments"}}),
    ];
    for call in &calls {
        let mut payload = call.clone();
        payload["hook_event_name"] = json!("PreToolUse");
        hook_as(repository, "tpl-1", "pre-tool-use", payload);
    }
    let template = repository.root.join(".crane/runtime/sessions/claude-tpl-1");
    let document: Value =
        serde_json::from_str(&fs::read_to_string(template.join("session.json")).unwrap()).unwrap();
    let journal = fs::read_to_string(template.join("journal.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let actions = journal[1..].to_vec();
    let others = fs::read_dir(&template)
        .unwrap()
        .flatten()
        .filter(|entry| {
            !["session.json", "journal.jsonl"]
                .contains(&entry.file_name().to_string_lossy().as_ref())
        })
        .map(|entry| (entry.file_name(), fs::read(entry.path()).unwrap()))
        .collect::<Vec<_>>();
    fs::remove_dir_all(&template).unwrap();
    let profiles = ["claude", "codex", "generic"];
    let teams = ["payments", "catalog", "platform", "search"];
    let now = now();
    let mut ids = Vec::new();
    for index in 0..sessions {
        let profile = profiles[index % profiles.len()];
        let id = format!("{profile}-perf-{index:07}");
        let begin = now - 60 - (index as u64 * 7919) % (29 * 86_400);
        let mut binding = document.clone();
        binding["agent"] = json!(profile);
        binding["session_id"] = json!(id);
        binding["provider_session"] = json!(format!("perf-{index:07}"));
        binding["created_at"] = json!(begin);
        binding["expires_at"] = json!(begin + 8 * 3600);
        binding["governance"]["organization"] =
            json!({"organization": "acme", "team": teams[index % teams.len()]});
        binding.as_object_mut().unwrap().remove("binding_digest");
        let digest = sha256(binding.to_string().as_bytes());
        binding["binding_digest"] = json!(digest);
        let mut previous = "genesis".to_string();
        let mut lines = String::new();
        for seq in 1..=events {
            let mut event = if seq == 1 {
                journal[0].clone()
            } else {
                actions[(seq - 2) % actions.len()].clone()
            };
            let object = event.as_object_mut().unwrap();
            object.remove("chain");
            object.insert("session_id".into(), json!(id));
            object.insert("agent".into(), json!(profile));
            object.insert("seq".into(), json!(seq));
            object.insert("at".into(), json!(begin + seq as u64 * 3));
            object.insert("binding".into(), json!(digest));
            if seq == 1 {
                object.insert("model".into(), json!(format!("model-{}", (index / 3) % 40)));
            }
            previous = sha256(format!("{previous}\n{event}").as_bytes());
            event["chain"] = json!(previous);
            lines.push_str(&event.to_string());
            lines.push('\n');
        }
        let directory = repository.root.join(".crane/runtime/sessions").join(&id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("session.json"), binding.to_string()).unwrap();
        fs::write(directory.join("journal.jsonl"), lines).unwrap();
        for (name, bytes) in &others {
            fs::write(directory.join(name), bytes).unwrap();
        }
        ids.push(id);
    }
    let states = [
        "RECEIVED",
        "EXECUTING",
        "REVIEW",
        "MERGED",
        "COMPLETED",
        "FAILED",
        "BLOCKED",
    ];
    for index in 0..tasks {
        let id = format!("PERF-{index:06}");
        let linked = ids
            .get(index)
            .map(|id| vec![id.as_str()])
            .unwrap_or_default();
        let at = now - 60 - (index as u64 * 104_729) % (29 * 86_400);
        task_record(
            repository,
            &id,
            states[index % states.len()],
            &linked,
            &[("RECEIVED", at)],
        );
    }
    ids
}

/** Time a GET: the median of several warm requests after the first
 * Input
    - server: &Server - server
    - path: &str - path and query
    - token: &str - token
 * Output
    - (std::time::Duration, Value) the median time, and the answer
*/
fn timed(server: &Server, path: &str, token: &str) -> (std::time::Duration, Value) {
    let first = server.json(path, token);
    let mut times = (0..3)
        .map(|_| {
            let started = std::time::Instant::now();
            server.json(path, token);
            started.elapsed()
        })
        .collect::<Vec<_>>();
    times.sort();
    (times[1], first)
}

/** Measure every page that spans the history and require each to stay within its budget
 * Input
    - repository: &Repository - repository with a large history
    - ids: &[String] - its session ids
    - budget: std::time::Duration - most a warm page may take
 * Output
    - None
*/
fn pages_stay_within(repository: &Repository, ids: &[String], budget: std::time::Duration) {
    let started = std::time::Instant::now();
    let server = Server::start(repository, "observe", TOKEN);
    eprintln!("server loaded in {:?}", started.elapsed());
    let reader = viewer(
        repository,
        &[
            "perf",
            "--organization",
            "acme",
            "--repository",
            "*",
            "--team",
            "payments",
        ],
    );
    let run = &ids[ids.len() / 2];
    let pages = [
        ("overview aggregation", "/api/v1/dashboard/overview?range=30d".to_string()),
        ("global filtering", "/api/v1/dashboard/overview?range=7d&agent=codex&team=payments&verification=passed,failed".to_string()),
        ("task list", "/api/v1/dashboard/tasks?range=30d&page=1&page_size=50".to_string()),
        ("task list, deep page sorted", "/api/v1/dashboard/tasks?range=30d&sort=duration&page=20&page_size=50".to_string()),
        ("task search", "/api/v1/dashboard/tasks?range=30d&q=PERF-0001".to_string()),
        ("run list", "/api/v1/dashboard/runs?range=30d&page=2&page_size=50&sort=violations".to_string()),
        ("run list, filtered", "/api/v1/dashboard/runs?range=7d&provider=Codex&team=payments".to_string()),
        ("event timeline", format!("/api/v1/dashboard/runs/{run}")),
        ("event timeline, keyset page", format!("/api/v1/dashboard/runs/{run}/events?after=10&limit=50")),
        ("violation explorer", "/api/v1/dashboard/violations?range=30d&page=1&page_size=50".to_string()),
        ("violation explorer, critical", "/api/v1/dashboard/violations?range=30d&severity=critical&page=2".to_string()),
        ("agents", "/api/v1/dashboard/agents?range=30d".to_string()),
        ("autonomy", "/api/v1/dashboard/autonomy?range=30d".to_string()),
        ("policies", "/api/v1/dashboard/policies?range=30d".to_string()),
        ("decisions page", "/api/v1/decisions?decision=deny&limit=100&offset=500".to_string()),
    ];
    for (name, path) in &pages {
        for (who, token) in [("operator", TOKEN), ("viewer", reader.as_str())] {
            let (time, answer) = timed(&server, path, token);
            eprintln!(
                "{name} ({who}): {time:?}, {} bytes",
                answer.to_string().len()
            );
            assert!(
                time < budget,
                "{name} as {who} took {time:?} (budget {budget:?})"
            );
            assert!(
                answer.to_string().len() < 5_000_000,
                "{name} answers more than 5 MB"
            );
        }
    }
    // The answers stay right at volume: counts agree with the records
    let runs = server.json("/api/v1/dashboard/runs?range=30d&page_size=10", TOKEN);
    assert_eq!(runs["total"], ids.len());
    let viewer_runs = server.json("/api/v1/dashboard/runs?range=30d&page_size=10", &reader);
    assert_eq!(
        viewer_runs["total"],
        ids.len().div_ceil(4),
        "a team sees its quarter"
    );
    let audit = server.json("/api/v1/audit", TOKEN);
    assert!(audit["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session["chain"]["status"] == "verified"));
}

/** Query performance: with thousands of sessions, tens of thousands of journal events, and tens of
 * thousands of tasks, every page that spans them answers well within a second or two even in a
 * debug build, for the operator and for a team-scoped viewer, and stays bounded in size */
#[test]
fn pages_stay_fast_on_large_histories() {
    let repository = Repository::new();
    let ids = large_history(&repository, 600, 60, 30_000);
    pages_stay_within(&repository, &ids, std::time::Duration::from_secs(3));
}

/** Load test at production volume (run with: cargo test --release --test observe -- --ignored):
 * 10,000 sessions of 100 journal events (1,000,000 events) and 100,000 tasks, every page within
 * 1.5 seconds; OBSERVE_LOAD="sessions,events,tasks" changes the volume */
#[test]
#[ignore]
fn pages_stay_fast_at_production_volume() {
    let volume = std::env::var("OBSERVE_LOAD").unwrap_or_else(|_| "10000,100,100000".into());
    let numbers = volume
        .split(',')
        .map(|part| part.trim().parse::<usize>().unwrap())
        .collect::<Vec<_>>();
    let repository = Repository::new();
    let started = std::time::Instant::now();
    let ids = large_history(&repository, numbers[0], numbers[1], numbers[2]);
    eprintln!(
        "generated {} sessions, {} events, {} tasks in {:?}",
        numbers[0],
        numbers[0] * numbers[1],
        numbers[2],
        started.elapsed()
    );
    pages_stay_within(&repository, &ids, std::time::Duration::from_millis(1500));
}
