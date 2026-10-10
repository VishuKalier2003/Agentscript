// MongoDB outage behavior (built with --features mongodb): an unreachable database never weakens
// enforcement, never loses local evidence, and never stops the dashboard.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;

use common::{text, Fixture};
use serde_json::json;

/** A MongoDB URI that refuses connections quickly */
const UNREACHABLE: &str =
    "mongodb://127.0.0.1:9/?serverSelectionTimeoutMS=500&connectTimeoutMS=500";

/** Run a hook with the unreachable database configured
 * Input
    - fixture: &Fixture - repository
    - event: &str - hook event
    - payload: serde_json::Value - payload
 * Output
    - std::process::Output
*/
fn hook(fixture: &Fixture, event: &str, payload: serde_json::Value) -> std::process::Output {
    let mut child = fixture
        .command(&["agent", "hook", "--event", event, "--profile", "claude"])
        .env("CLAUDECODE", "1")
        .env("CRANE_MONGODB_URI", UNREACHABLE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/** Enforcement and local evidence are unaffected by a database outage
 * Input
    - None
 * Output
    - None (panics on failure)
*/
#[test]
fn database_outage_does_not_weaken_enforcement() {
    let fixture = Fixture::ready();
    fixture.protect(&["app/payments.py", "start-line", "1", "end-line", "4"]);
    assert!(hook(&fixture, "SessionStart", json!({"session_id": "m1"}))
        .status
        .success());
    let denied = hook(
        &fixture,
        "pre-tool-use",
        json!({"session_id": "m1", "tool_use_id": "t1", "cwd": fixture.work.to_string_lossy(), "tool_name": "Edit", "tool_input": {"file_path": fixture.work.join("app/payments.py").to_string_lossy(), "old_string": "0.03", "new_string": "0.3"}}),
    );
    assert_eq!(denied.status.code(), Some(2), "{}", text(&denied));
    let end = hook(&fixture, "SessionEnd", json!({"session_id": "m1"}));
    assert!(
        end.status.success(),
        "the session ends even though MongoDB is down: {}",
        text(&end)
    );
    let session = fixture.ok(&["session", "current"]);
    assert!(
        session.contains("status: ENDED") && session.contains("actions_denied"),
        "{session}"
    );
    let mut dashboard = fixture
        .command(&["dashboard"])
        .env("CRANE_MONGODB_URI", UNREACHABLE)
        .env("CRANE_DASHBOARD_MAX_REQUESTS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(dashboard.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(
        line.contains("http://127.0.0.1:"),
        "the dashboard serves local evidence: {line}"
    );
    let port = line[line.find("127.0.0.1:").unwrap() + 10..]
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    write!(stream, "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
    let mut response = String::new();
    std::io::Read::read_to_string(&mut stream, &mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    let output = dashboard.wait_with_output().unwrap();
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("MongoDB synchronization failed"),
        "the outage is reported"
    );
}

/** The live pipeline (needs CRANE_TEST_MONGODB_URI, skipped otherwise): crane integrate mongodb
 * verifies and migrates, hooks and commands reach MongoDB through the detached sync, rollups and
 * alerts are copied, records missing from MongoDB are re-sent, and the dashboard reads MongoDB
 * Input
    - None
 * Output
    - None (panics on failure)
*/
#[test]
fn live_pipeline_reaches_mongodb() {
    use mongodb::bson::doc;
    let Ok(uri) = std::env::var("CRANE_TEST_MONGODB_URI") else {
        eprintln!("skipped: set CRANE_TEST_MONGODB_URI to run against a live MongoDB");
        return;
    };
    let name = format!("crane_test_{}", std::process::id());
    let fixture = Fixture::ready();
    let configured = fixture.with_input(
        &["integrate", "mongodb"],
        &format!("2\n{uri}\n{name}\n30\n5\ny\ny\n"),
    );
    assert!(
        text(&configured).contains("Connection verified"),
        "{}",
        text(&configured)
    );
    assert!(!fixture.read(".crane/config.json").contains(&uri));
    fixture.protect(&["app/payments.py", "start-line", "1", "end-line", "4"]);
    let call = |id: &str, command: &str| json!({"session_id": "live-1", "tool_use_id": id, "cwd": fixture.work.to_string_lossy(), "tool_name": "Bash", "tool_input": {"command": command}});
    fixture.hook("claude", "SessionStart", &json!({"session_id": "live-1"}));
    fixture.hook("claude", "pre-tool-use", &call("l1", "echo ok"));
    fixture.hook("claude", "post-tool-use", &call("l1", "echo ok"));
    fixture.hook("claude", "pre-tool-use", &call("l2", "cat .crane/map"));
    fixture.hook("claude", "SessionEnd", &json!({"session_id": "live-1"}));
    let synced = fixture.ok(&["agent", "sync"]);
    assert!(synced.contains("Synchronized"), "{synced}");
    let client = mongodb::sync::Client::with_uri_str(&uri).unwrap();
    let database = client.database(&name);
    let count = |collection: &str, filter| {
        database
            .collection::<mongodb::bson::Document>(collection)
            .count_documents(filter)
            .run()
            .unwrap()
    };
    let local_events = std::fs::read_to_string(
        std::fs::read_dir(fixture.home.join("repos"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("runtime/events.jsonl"),
    )
    .unwrap()
    .lines()
    .count() as u64;
    assert_eq!(count("events", doc! {}), local_events);
    assert!(count("events", doc! {"event_type": "command.executed"}) >= 2);
    assert!(count("events", doc! {"event_type": "tool_call.verified"}) >= 1);
    assert!(count("alerts", doc! {"kind": "bypass_attempt"}) >= 1);
    assert!(count("metric_rollups", doc! {"scope": "session"}) >= 1);
    assert!(count("metric_rollups", doc! {"scope": "tool_call"}) >= 1);
    assert_eq!(count("sessions", doc! {}), 1);
    assert!(count("governance_changes", doc! {}) >= 3);
    // Let the detached syncs started by the hooks finish before simulating data loss
    std::thread::sleep(std::time::Duration::from_secs(5));
    database
        .collection::<mongodb::bson::Document>("events")
        .delete_many(doc! {"sequence": {"$gt": 3}})
        .run()
        .unwrap();
    assert!(fixture.ok(&["agent", "sync"]).contains("re-sent"));
    assert_eq!(count("events", doc! {}), local_events);
    let mut dashboard = fixture
        .command(&["dashboard"])
        .env("CRANE_DASHBOARD_MAX_REQUESTS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(dashboard.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let token = line.split("#token=").nth(1).unwrap().trim().to_string();
    let port = line[line.find("127.0.0.1:").unwrap() + 10..]
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    write!(
        stream,
        "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    std::io::Read::read_to_string(&mut stream, &mut response).unwrap();
    assert!(response.contains("\"source\":\"mongodb\""), "{response}");
    assert!(response.contains("\"status\":\"verified\""), "{response}");
    let _ = dashboard.wait();
    database.drop().run().unwrap();
}
