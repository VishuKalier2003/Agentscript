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

/** The payment service of the fixture repository */
const SERVICE: &str = "class PaymentService:\n    def charge(self, amount):\n        return amount + 1\n\n    def refund(self, amount):\n        return -amount\n";

/** A temporary directory, optionally a Git repository, removed on drop
 * Fields
    - root: PathBuf - directory
*/
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /** Create an empty directory
     * Input
        - None
     * Output
        - Fixture
    */
    fn empty() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "crane-repo-{suffix}-{}",
            REPOSITORIES.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    /** Create a Git repository with one commit (a Python payment service) and an origin remote
     * Input
        - origin: Option<&str> - origin remote URL
     * Output
        - Fixture
    */
    fn repository(origin: Option<&str>) -> Self {
        let fixture = Self::empty();
        fixture.write("pay/service.py", SERVICE);
        fixture.write("README.md", "# Shop\n");
        // Each fixture is its own repository: identical content committed in the same second
        // would give the same root commit, which is how Crane identifies a repository
        let message = format!(
            "baseline {}",
            fixture.root.file_name().unwrap().to_string_lossy()
        );
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "lead@example.com"],
            vec!["config", "user.name", "Crane Repo Test"],
            vec!["config", "core.autocrlf", "false"],
            vec!["add", "."],
            vec!["commit", "-qm", message.as_str()],
        ] {
            fixture.git(&args);
        }
        if let Some(origin) = origin {
            fixture.git(&["remote", "add", "origin", origin]);
        }
        fixture
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
        - String trimmed stdout
    */
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

    /** Run crane with agent markers removed, extra variables, and a working directory
     * Input
        - args: &[&str] - arguments
        - environment: &[(&str, &str)] - variables
        - directory: Option<&str> - subdirectory to run in
     * Output
        - Output
    */
    fn run(&self, args: &[&str], environment: &[(&str, &str)], directory: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
        command
            .args(args)
            .current_dir(directory.map_or(self.root.clone(), |directory| self.root.join(directory)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for marker in AGENT_MARKERS {
            command.env_remove(marker);
        }
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"").unwrap();
        child.wait_with_output().unwrap()
    }

    /** Run crane as a human and require success
     * Input
        - args: &[&str] - arguments
     * Output
        - String stdout
    */
    fn crane(&self, args: &[&str]) -> String {
        let output = self.run(args, &[], None);
        assert!(
            output.status.success(),
            "crane {args:?}: {}{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /** Run crane with --json and parse the answer
     * Input
        - args: &[&str] - arguments
     * Output
        - Value
    */
    fn json(&self, args: &[&str]) -> Value {
        let mut full = args.to_vec();
        full.push("--json");
        serde_json::from_str(&self.crane(&full)).unwrap()
    }

    /** Read the stored connection record
     * Input
        - None
     * Output
        - (String, Value) the file's text and value
    */
    fn record(&self) -> (String, Value) {
        let text = fs::read_to_string(self.root.join(".crane/connection.json")).unwrap();
        let value = serde_json::from_str(&text).unwrap();
        (text, value)
    }
}

impl Drop for Fixture {
    /** Remove the directory
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

/** A fresh repository connects in one step (Crane initialized, trusted checkpoint created at
 * HEAD, discovery run) and flows straight into zones, policies, task planning, and an agent session
 */
#[test]
fn a_connected_repository_flows_into_execution() {
    let fixture = Fixture::repository(Some("https://github.com/acme/shop.git"));
    assert!(!fixture.root.join(".crane").exists());
    let connected = fixture.json(&["repo", "connect"]);
    assert_eq!(connected["already_connected"], false);
    assert_eq!(connected["provider"], "github");
    assert_eq!(
        (
            connected["owner"].as_str(),
            connected["name"].as_str(),
            connected["full_name"].as_str()
        ),
        (Some("acme"), Some("shop"), Some("acme/shop"))
    );
    assert_eq!(connected["web_url"], "https://github.com/acme/shop");
    assert_eq!(connected["default_branch"], "main");
    assert_eq!(connected["trusted_checkpoint"], "baseline");
    assert_eq!(
        connected["last_discovery"]["head"],
        fixture.git(&["rev-parse", "HEAD"])
    );
    assert!(connected["repository_id"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    let checkpoint: Value = serde_json::from_str(
        &fs::read_to_string(fixture.root.join(".crane/checkpoints/baseline.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(checkpoint["commit"], fixture.git(&["rev-parse", "HEAD"]));

    let status = fixture.json(&["repo", "status"]);
    assert_eq!(status["connection_status"], "connected");
    assert_eq!(status["trusted_checkpoint"]["status"], "current");
    assert_eq!(status["discovery"]["state"], "fresh");

    // Zones, policies, task planning, and an agent session work immediately
    fixture.write(".crane/zones/org.zone", "zone payments {\n    criticality critical;\n    autonomy assisted;\n    select subsystem pay;\n}\n");
    assert!(fixture.crane(&["zones"]).contains("zone payments"));
    fixture.crane(&[
        "protect",
        "--function",
        "PaymentService.charge",
        "--policy",
        "core",
    ]);
    fixture.write(".crane/tasks/PAY-1.json", "{\"task_format\": 1, \"task_id\": \"PAY-1\", \"title\": \"Reject negative refunds\", \"description\": \"Make `PaymentService.refund` reject negative amounts without changing `PaymentService.charge`.\", \"acceptance_criteria\": [\"a negative refund raises\"], \"repositories\": [\"acme/shop\"]}");
    let plan = fixture.json(&["task", "plan", "PAY-1"]);
    assert_eq!(
        plan["status"], "planned",
        "the connected acme/shop matches the task's repository: {plan}"
    );
    let started = fixture.crane(&[
        "agent",
        "session",
        "start",
        "--profile",
        "claude",
        "--session",
        "s1",
        "--task",
        "PAY-1",
    ]);
    assert!(started.contains("task: PAY-1"), "{started}");
    let inspected = fixture.json(&["repo", "inspect"]);
    assert_eq!(inspected["capabilities"]["ready"], true, "{inspected}");
    assert_eq!(
        inspected["policy_version"]["changed"], true,
        "the new policy is noticed"
    );
}

/** Connecting again is idempotent: the same record, untouched on disk; refresh updates the
 * metadata but keeps who connected and when, and never moves the trusted checkpoint */
#[test]
fn duplicate_connection_is_idempotent() {
    let fixture = Fixture::repository(Some("git@github.com:acme/shop.git"));
    let first = fixture.json(&["repo", "connect"]);
    let (stored, _) = fixture.record();
    let again = fixture.json(&["repo", "connect"]);
    assert_eq!(again["already_connected"], true);
    assert_eq!(
        fixture.record().0,
        stored,
        "connecting again writes nothing"
    );
    let alias = fixture.json(&["connect"]);
    assert_eq!(
        alias["already_connected"], true,
        "crane connect is the same command"
    );

    fixture.write("pay/extra.py", "def extra():\n    return 1\n");
    fixture.git(&["add", "."]);
    fixture.git(&["commit", "-qm", "more"]);
    let refreshed = fixture.json(&["repo", "connect", "--refresh"]);
    assert_eq!(refreshed["connected_at"], first["connected_at"]);
    assert!(refreshed["refreshed_at"].as_u64().is_some());
    assert_eq!(
        refreshed["last_discovery"]["head"],
        fixture.git(&["rev-parse", "HEAD"])
    );
    let checkpoint: Value = serde_json::from_str(
        &fs::read_to_string(fixture.root.join(".crane/checkpoints/baseline.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        checkpoint["commit"],
        fixture.git(&["rev-parse", "HEAD~1"]),
        "refresh never moves the trusted checkpoint"
    );
    let actions = fixture.record().1["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["action"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(actions, ["checkpoint_created", "connected", "refreshed"]);
}

/** Invalid repositories are refused with a reason: not a Git repository, a session worktree, an
 * agent environment, and a .crane connected to another repository */
#[test]
fn invalid_repositories_are_refused() {
    let plain = Fixture::empty();
    let refused = plain.run(&["repo", "connect"], &[], None);
    assert!(!refused.status.success());
    assert!(
        text(&refused.stderr)
            .contains("invalid repository: the current directory is not inside a Git repository"),
        "{}",
        text(&refused.stderr)
    );
    assert!(
        !plain.root.join(".crane").exists(),
        "nothing is created for an invalid repository"
    );

    let fixture = Fixture::repository(None);
    let agent = fixture.run(&["repo", "connect"], &[("CLAUDECODE", "1")], None);
    assert!(text(&agent.stderr).contains("agent environment"));
    assert!(
        !fixture.root.join(".crane").exists(),
        "an agent connects nothing"
    );
    fixture.crane(&["repo", "connect"]);

    // The same .crane in another repository is refused
    let other = Fixture::repository(None);
    let copy = |from: &PathBuf, to: &PathBuf| {
        for entry in walk(from) {
            let target = to.join(entry.strip_prefix(from).unwrap());
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::copy(&entry, &target).unwrap();
        }
    };
    copy(&fixture.root.join(".crane"), &other.root.join(".crane"));
    let foreign = other.run(&["repo", "connect"], &[], None);
    assert!(
        text(&foreign.stderr).contains("connected to another repository"),
        "{}",
        text(&foreign.stderr)
    );
    other.crane(&["repo", "disconnect", "--forget"]);
    assert_eq!(
        other.json(&["repo", "connect"])["already_connected"],
        false,
        "after forgetting, it connects"
    );
}

/** List every file below a directory
 * Input
    - directory: &PathBuf - directory
 * Output
    - Vec<PathBuf>
*/
fn walk(directory: &PathBuf) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/** Missing Git metadata: a repository without commits is refused; one without a remote connects
 * as a local repository; a detached HEAD still connects */
#[test]
fn missing_git_metadata() {
    let unborn = Fixture::empty();
    unborn.git(&["init", "-q"]);
    let refused = unborn.run(&["repo", "connect"], &[], None);
    assert!(
        text(&refused.stderr).contains("missing Git metadata: the repository has no commits yet"),
        "{}",
        text(&refused.stderr)
    );

    let local = Fixture::repository(None);
    let connected = local.json(&["repo", "connect"]);
    assert_eq!(connected["provider"], "local");
    assert_eq!(connected["owner"], Value::Null);
    assert_eq!(connected["remote"], Value::Null);
    assert_eq!(
        connected["name"],
        local.root.file_name().unwrap().to_string_lossy().as_ref()
    );

    let detached = Fixture::repository(Some("https://gitlab.com/acme/shop.git"));
    let head = detached.git(&["rev-parse", "HEAD"]);
    detached.git(&["checkout", "-q", &head]);
    let connected = detached.json(&["repo", "connect", "--default-branch", "main"]);
    assert_eq!(connected["provider"], "git");
    assert_eq!(connected["default_branch"], "main");
    assert_eq!(
        detached.json(&["repo", "status"])["current_branch"],
        Value::Null
    );

    // Connecting from a subdirectory connects the repository's top level
    let nested = Fixture::repository(None);
    let output = nested.run(&["repo", "connect", "--json"], &[], Some("pay"));
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(nested.root.join(".crane/connection.json").exists());
    assert!(!nested.root.join("pay/.crane").exists());
}

/** A stale checkpoint is reported, not silently moved: new commits make it stale with the count,
 * a checkpoint whose commit vanished is missing, and an invalid one is reported as invalid */
#[test]
fn stale_checkpoint() {
    let fixture = Fixture::repository(None);
    fixture.crane(&["repo", "connect"]);
    for round in 1..=2 {
        fixture.write(&format!("pay/new{round}.py"), "def new():\n    return 1\n");
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-qm", "more"]);
    }
    let status = fixture.json(&["repo", "status"]);
    assert_eq!(status["trusted_checkpoint"]["status"], "stale");
    assert_eq!(status["trusted_checkpoint"]["commits_since"], 2);
    assert_eq!(status["discovery"]["state"], "stale");
    let human = fixture.crane(&["repo", "status"]);
    assert!(human.contains("(stale)"), "{human}");
    fixture.crane(&["discover"]);
    assert_eq!(
        fixture.json(&["repo", "status"])["discovery"]["state"],
        "fresh",
        "crane discover updates the connection"
    );

    fixture.write(".crane/checkpoints/baseline.json", "{\"name\":\"baseline\",\"commit\":\"1111111111111111111111111111111111111111\",\"branch\":\"main\",\"created_at_unix\":1}");
    assert_eq!(
        fixture.json(&["repo", "status"])["trusted_checkpoint"]["status"],
        "missing"
    );
    fixture.write(
        ".crane/checkpoints/baseline.json",
        "{\"name\":\"baseline\",\"commit\":\"abc\",\"branch\":\"main\",\"created_at_unix\":1}",
    );
    assert_eq!(
        fixture.json(&["repo", "status"])["trusted_checkpoint"]["status"],
        "invalid"
    );
    let inspected = fixture.json(&["repo", "inspect"]);
    assert_eq!(inspected["capabilities"]["ready"], false);
    assert!(inspected["capabilities"]["missing"]
        .to_string()
        .contains("trusted checkpoint"));
}

/** Disconnecting keeps everything; the control plane stops answering; reconnecting restores the
 * same connection with its history */
#[test]
fn disconnect_and_reconnect() {
    let fixture = Fixture::repository(Some("https://github.com/acme/shop.git"));
    let first = fixture.json(&["repo", "connect"]);
    fixture.crane(&[
        "protect",
        "--function",
        "PaymentService.charge",
        "--policy",
        "core",
    ]);
    let disconnected = fixture.json(&["repo", "disconnect", "--reason", "migrating"]);
    assert_eq!(disconnected["connection_status"], "disconnected");
    assert_eq!(disconnected["disconnect_reason"], "migrating");
    assert!(
        fixture.root.join(".crane/policies/core.crane").exists(),
        "configuration is kept"
    );
    let screen = fixture.run(&["dashboard", "api", "GET", "/api/repository"], &[], None);
    assert!(
        text(&screen.stdout).contains("the repository is disconnected"),
        "{}",
        text(&screen.stdout)
    );
    assert_eq!(
        fixture.json(&["repo", "disconnect"])["connection_status"],
        "disconnected",
        "disconnecting twice is harmless"
    );

    let again = fixture.json(&["repo", "connect"]);
    assert_eq!(again["already_connected"], false);
    assert_eq!(again["connection_status"], "connected");
    assert_eq!(again["connected_at"], first["connected_at"]);
    assert_eq!(again["disconnected_at"], Value::Null);
    let actions = again["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["action"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        actions,
        [
            "checkpoint_created",
            "connected",
            "disconnected",
            "reconnected"
        ]
    );
    let screen = fixture.run(&["dashboard", "api", "GET", "/api/repository"], &[], None);
    assert!(screen.status.success());
}

/** State persistence: one format 2 record written atomically (no temporary files left, no
 * credentials stored) that a later process reads back; a format 1 record from the first control
 * plane is read and upgraded */
#[test]
fn state_persistence() {
    let fixture = Fixture::repository(Some("https://bot:ghp_secret123@github.com/acme/shop.git"));
    fixture.crane(&["repo", "connect"]);
    let (text_stored, record) = fixture.record();
    assert_eq!(record["connection_format"], 2);
    assert!(
        !text_stored.contains("ghp_secret123"),
        "credentials are never stored"
    );
    assert_eq!(record["remote"], "https://github.com/acme/shop.git");
    let leftovers = fs::read_dir(fixture.root.join(".crane"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
        .count();
    assert_eq!(leftovers, 0);
    let read_back = fixture.json(&["repo", "inspect"]);
    assert_eq!(read_back["repository_id"], record["repository_id"]);
    assert_eq!(read_back["history"], record["history"]);
    assert!(read_back["branches"]
        .as_array()
        .unwrap()
        .iter()
        .any(|branch| branch["name"] == "main"));
    assert!(!read_back["worktrees"].as_array().unwrap().is_empty());

    let legacy = Fixture::repository(Some("https://github.com/acme/legacy.git"));
    legacy.crane(&["init"]);
    legacy.crane(&["checkpoint", "--name", "baseline"]);
    let identity = {
        let mut commits = legacy
            .git(&["rev-list", "--max-parents=0", "HEAD"])
            .split_whitespace()
            .map(String::from)
            .collect::<Vec<_>>();
        commits.sort();
        commits
    };
    assert_eq!(identity.len(), 1);
    // A record exactly as the first control plane wrote it (format 1); its id is recomputed by connect
    let first = json!({"connection_format": 1, "repository_id": "placeholder", "root": legacy.root.to_string_lossy(), "remote": "https://github.com/acme/legacy.git", "default_branch": "main", "connected_at": 100, "connected_by": "lead@example.com", "languages": []});
    legacy.write(".crane/connection.json", &first.to_string());
    let status = legacy.json(&["repo", "status"]);
    assert_eq!(status["connection_status"], "connected");
    assert_eq!(status["provider"], "github");
    assert_eq!(status["owner"], "acme");
    assert_eq!(status["name"], "legacy");
}

/** The control plane shows repository, provider, branch, checkpoint, discovery, and connection */
#[test]
fn dashboard_shows_the_connection() {
    let fixture = Fixture::repository(Some("https://github.com/acme/shop.git"));
    fixture.crane(&["init"]);
    let before = fixture.run(&["dashboard", "api", "GET", "/api/repo"], &[], None);
    assert!(text(&before.stdout).contains("connect the repository first"));
    fixture.crane(&["repo", "connect"]);
    let repo: Value =
        serde_json::from_str(&fixture.crane(&["dashboard", "api", "GET", "/api/repo"])).unwrap();
    assert_eq!(repo["full_name"], "acme/shop");
    assert_eq!(repo["provider"], "github");
    assert_eq!(repo["current_branch"], "main");
    assert_eq!(repo["trusted_checkpoint"]["status"], "current");
    assert_eq!(repo["discovery"]["state"], "fresh");
    assert_eq!(repo["connection_status"], "connected");
    assert_eq!(
        repo,
        fixture.json(&["repo", "status"]),
        "the dashboard and the CLI agree"
    );
    let page = include_str!("../src/dashboard/app.html");
    for label in [
        "\"provider\"",
        "trusted checkpoint",
        "discovery",
        "connection",
        "/api/repo",
    ] {
        assert!(page.contains(label), "{label}");
    }
    // Only a human connects or disconnects: an agent's shell running it is denied
    let payload = json!({"session_id": "x1", "tool_name": "Bash", "tool_input": {"command": "crane repo disconnect"}}).to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_crane"));
    command
        .args([
            "agent",
            "hook",
            "--event",
            "pre-tool-use",
            "--profile",
            "claude",
        ])
        .current_dir(&fixture.root)
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
        .write_all(payload.as_bytes())
        .unwrap();
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(2));
    assert_eq!(
        fixture.json(&["repo", "status"])["connection_status"],
        "connected"
    );
}
