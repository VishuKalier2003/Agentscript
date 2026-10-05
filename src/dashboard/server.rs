// The dashboard's local web server: GET / serves the single-page dashboard, and /api/* answers
// through the same route the CLI uses. API requests need the token printed at start (the page
// itself carries no data), one request is served at a time, and only on the given address.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use serde_json::{json, Value};

use super::route;
use crate::proposals::store::require_human;
use crate::util::{now_unix, sha256};

/** The dashboard page */
const PAGE: &str = include_str!("app.html");

/** Largest request body accepted */
const MAX_BODY: usize = 1_000_000;

/** Compare two secrets in time independent of where they differ
 * Input
    - left: &str - secret
    - right: &str - secret
 * Output
    - bool
*/
fn same(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/** Serve the dashboard until stopped (or for one request)
 * Input
    - address: &str - address to bind
    - once: bool - stop after one request
 * Output
    - Result<(), String>
*/
pub(crate) fn serve(address: &str, once: bool) -> Result<(), String> {
    require_human("serve the dashboard")?;
    let token = std::env::var("CRANE_DASHBOARD_TOKEN")
        .ok()
        .filter(|token| token.len() >= 16)
        .unwrap_or_else(|| {
            sha256(
                format!(
                    "{}:{}:{:?}",
                    now_unix(),
                    std::process::id(),
                    std::time::Instant::now()
                )
                .as_bytes(),
            )
            .trim_start_matches("sha256:")
            .chars()
            .take(32)
            .collect()
        });
    let listener = TcpListener::bind(address)
        .map_err(|error| format!("cannot listen on {address}: {error}"))?;
    let bound = listener.local_addr().map_err(|error| error.to_string())?;
    println!("Crane dashboard: http://{bound}/?token={token}");
    std::io::stdout().flush().ok();
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        handle(stream, &token);
        if once {
            break;
        }
    }
    Ok(())
}

/** Answer one connection
 * Input
    - stream: TcpStream - connection
    - token: &str - API token
 * Output
    - None
*/
fn handle(mut stream: TcpStream, token: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let (status, content_type, body) = match read(&mut stream, token) {
        Ok(answer) => answer,
        Err((status, message)) => (
            status,
            "application/json",
            json!({"error": message}).to_string(),
        ),
    };
    let reason = match status {
        200 => "OK",
        201 => "Created",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        _ => "Bad Request",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/** Read one request and produce the response
 * Input
    - stream: &mut TcpStream - connection
    - token: &str - API token
 * Output
    - Result<(u16, &'static str, String), (u16, String)> status, content type, and body
*/
fn read(stream: &mut TcpStream, token: &str) -> Result<(u16, &'static str, String), (u16, String)> {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|error| (400, error.to_string()))?,
    );
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| (400, error.to_string()))?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let path = target.split('?').next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    loop {
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .map_err(|error| (400, error.to_string()))?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
        if headers.len() > 100 {
            return Err((400, "too many headers".into()));
        }
    }
    if method == "GET" && (path == "/" || path == "/index.html") {
        return Ok((200, "text/html", PAGE.to_string()));
    }
    if !path.starts_with("/api/") {
        return Err((404, "use / for the dashboard and /api/ for its API".into()));
    }
    if !headers
        .get("x-crane-token")
        .is_some_and(|given| same(given, token))
    {
        return Err((
            401,
            "missing or wrong X-Crane-Token (it is printed when the dashboard starts)".into(),
        ));
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err((413, "request body is larger than 1 MB".into()));
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| (400, error.to_string()))?;
    let value: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).map_err(|error| (400, format!("invalid JSON: {error}")))?
    };
    let (status, answer) = route(&method, &path, &value);
    Ok((status, "application/json", answer.to_string()))
}
