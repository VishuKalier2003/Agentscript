// The Foxx dashboard in local mode: loopback binding, token pairing, read-only API, Host and Origin
// validation, bounded pagination, and the content of its views, reports, and evidence bundles.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Stdio};

use common::Fixture;
use serde_json::{json, Value};

/** A running dashboard
 * Fields
    - child: Child - the crane dashboard process
    - port: u16 - bound port
    - token: String - pairing token
*/
struct Dashboard {
    child: Child,
    port: u16,
    token: String,
}

impl Dashboard {
    /** Start the dashboard for a number of requests and read its URL
     * Input
        - fixture: &Fixture - repository
        - requests: usize - requests to serve before exiting
     * Output
        - Dashboard
    */
    fn start(fixture: &Fixture, requests: usize) -> Self {
        let mut child = fixture
            .command(&["dashboard"])
            .env("CRANE_DASHBOARD_MAX_REQUESTS", requests.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let url = &line[line.find("http://").expect("the dashboard prints its URL")..].trim();
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        let port = url["http://127.0.0.1:".len()..]
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let token = url.split("#token=").nth(1).unwrap().to_string();
        Self { child, port, token }
    }

    /** Send a raw request and return status and body
     * Input
        - method: &str - HTTP method
        - path: &str - path
        - headers: &[(&str, String)] - extra headers
     * Output
        - (u16, String)
    */
    fn request(&self, method: &str, path: &str, headers: &[(&str, String)]) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("host"))
        {
            request.push_str(&format!("Host: 127.0.0.1:{}\r\n", self.port));
        }
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        (status, body)
    }

    /** GET an API path with the token and parse the JSON
     * Input
        - path: &str - API path
     * Output
        - (u16, Value)
    */
    fn get(&self, path: &str) -> (u16, Value) {
        let (status, body) = self.request(
            "GET",
            path,
            &[("Authorization", format!("Bearer {}", self.token))],
        );
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }
}

impl Drop for Dashboard {
    /** Stop the dashboard
     * Input
        - None (uses self)
     * Output
        - None
    */
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/** The dashboard binds to loopback, refuses unauthenticated, non-GET, rebinding, and cross-origin
 * requests, and serves the page without data
 * Input
    - None
 * Output
    - None (panics on failure)
*/
#[test]
fn local_service_is_locked_down() {
    let fixture = Fixture::ready();
    let dashboard = Dashboard::start(&fixture, 8);
    let (status, page) = dashboard.request("GET", "/", &[]);
    assert_eq!(status, 200);
    assert!(page.contains("<title>Foxx Dashboard</title>"));
    assert!(
        !page.contains(&dashboard.token),
        "the page carries no secret"
    );
    assert_eq!(dashboard.request("GET", "/api/overview", &[]).0, 401);
    assert_eq!(
        dashboard
            .request(
                "GET",
                "/api/overview",
                &[("Authorization", "Bearer wrong".into())]
            )
            .0,
        401
    );
    assert_eq!(
        dashboard
            .request(
                "POST",
                "/api/overview",
                &[("Authorization", format!("Bearer {}", dashboard.token))]
            )
            .0,
        405
    );
    assert_eq!(
        dashboard
            .request(
                "GET",
                "/api/overview",
                &[
                    ("Host", "evil.example:80".into()),
                    ("Authorization", format!("Bearer {}", dashboard.token))
                ]
            )
            .0,
        421
    );
    assert_eq!(
        dashboard
            .request(
                "GET",
                "/api/overview",
                &[
                    ("Origin", "https://evil.example".into()),
                    ("Authorization", format!("Bearer {}", dashboard.token))
                ]
            )
            .0,
        403
    );
    assert_eq!(dashboard.request("GET", "/../../etc/passwd", &[]).0, 404);
    let (status, overview) = dashboard.get("/api/overview");
    assert_eq!(status, 200);
    assert_eq!(overview["integrity"]["ok"], true);
}

/** The API exposes governance, events with filters and pagination, sessions, graphs, metrics,
 * compliance, reports, and evidence bundles, and never exposes secrets
 * Input
    - None
 * Output
    - None (panics on failure)
*/
#[test]
fn api_views_and_evidence() {
    let fixture = Fixture::ready();
    let keep = fixture.protect(&["app/payments.py", "start-line", "1", "end-line", "4"]);
    let slack = fixture.with_input(
        &["integrate", "slack"],
        "1\nhttps://hooks.slack.com/services/T0/B0/TOPSECRETVALUE\n\n2\n\ny\n",
    );
    assert!(slack.status.success(), "{}", common::text(&slack));
    fixture.hook(
        "claude",
        "SessionStart",
        &json!({"session_id": "dash-1", "model": "m"}),
    );
    let call = |id: &str, tool: &str, input: Value| json!({"session_id": "dash-1", "tool_use_id": id, "cwd": fixture.work.to_string_lossy(), "tool_name": tool, "tool_input": input});
    fixture.hook("claude", "pre-tool-use", &call("d1", "Edit", json!({"file_path": fixture.work.join("app/payments.py").to_string_lossy(), "old_string": "0.03", "new_string": "0.3"})));
    for index in 0..3 {
        let payload = call(&format!("ok{index}"), "Bash", json!({"command": "echo ok"}));
        fixture.hook("claude", "pre-tool-use", &payload);
        fixture.hook("claude", "post-tool-use", &payload);
    }
    let dashboard = Dashboard::start(&fixture, 16);
    let (_, governance) = dashboard.get("/api/governance");
    assert_eq!(governance["selections"][0]["id"], keep.as_str());
    assert!(governance["selections"][0]["resolved"].is_object());
    assert_eq!(governance["policies"][0]["name"], "default");
    let (_, page) = dashboard.get("/api/events?limit=2");
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    let cursor = page["next_cursor"].as_u64().expect("more events");
    let (_, next) = dashboard.get(&format!("/api/events?limit=2&cursor={cursor}"));
    assert!(next["events"][0]["sequence"].as_u64().unwrap() > cursor);
    let (_, denied) = dashboard.get("/api/events?decision=DENY");
    assert_eq!(denied["events"].as_array().unwrap().len(), 1);
    assert_eq!(denied["events"][0]["enforcement"], "PREVENTED");
    let (_, sessions) = dashboard.get("/api/sessions");
    let session = sessions["sessions"][0]["session"]["foxx_session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, graph) = dashboard.get(&format!("/api/graph/{session}"));
    let edges = graph["edges"].as_array().unwrap();
    assert!(
        edges.iter().any(|edge| edge["kind"] == "verified_by"),
        "decisions link to their verification"
    );
    assert!(graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["flags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|flag| flag == "denied")));
    let (_, metrics) = dashboard.get(&format!("/api/metrics?scope=session&id={session}"));
    assert!(metrics["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|metric| metric["name"] == "prevented_actions" && metric["value"] == 1));
    let (_, compliance) = dashboard.get("/api/compliance");
    assert!(compliance["disclaimer"]
        .as_str()
        .unwrap()
        .contains("not a certification"));
    let outcome = |id: &str| {
        compliance["controls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|control| control["control"]["id"] == id)
            .unwrap()["outcome"]
            .clone()
    };
    assert_eq!(outcome("CR-01"), "SATISFIED");
    assert_eq!(outcome("CR-10"), "UNSUPPORTED");
    let (_, report) = dashboard.get(&format!("/api/reports/session/{session}"));
    assert!(report["limitations"].as_array().unwrap().len() >= 3);
    assert_eq!(
        report["integrity"]["event_chain"]["status"], "verified",
        "{}",
        report["integrity"]["event_chain"]
    );
    let (status, bundle) = dashboard.request(
        "GET",
        &format!("/api/evidence/session/{session}"),
        &[("Authorization", format!("Bearer {}", dashboard.token))],
    );
    assert_eq!(status, 200);
    let bundle: Value = serde_json::from_str(&bundle).unwrap();
    assert_eq!(bundle["manifest"]["parts"].as_array().unwrap().len(), 3);
    assert_eq!(bundle["bundle_digest"].as_str().unwrap().len(), 128);
    let (_, integrations) = dashboard.get("/api/integrations");
    assert!(
        !integrations.to_string().contains("TOPSECRETVALUE"),
        "secrets never leave the trust directory"
    );
    assert_eq!(integrations["integrations"][0]["name"], "slack");
    let (status, _) = dashboard.get("/api/nothing");
    assert_eq!(status, 404);
}
