use std::collections::HashMap;
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use serde_json::{json, Value};

use super::ingest;
use crate::proposals::store::require_human;

/** Largest webhook body accepted */
const MAX_BODY: usize = 1_000_000;

/** Serve webhook deliveries over HTTP, one request at a time: POST /webhooks/jira and
 * /webhooks/asana with the shared token in X-Crane-Token (from CRANE_WEBHOOK_TOKEN); Asana's
 * handshake (X-Hook-Secret) is echoed; every delivery goes through the same ingestion as replay
 * Input
    - address: &str - address to bind, 127.0.0.1:8787 by default
    - once: bool - stop after one request (for tests and manual checks)
 * Output
    - Result<(), String>
    - Error if no token is configured, the address cannot be bound, or in an agent environment
*/
pub(crate) fn serve(address: &str, once: bool) -> Result<(), String> {
    require_human("serve")?;
    let token = env::var("CRANE_WEBHOOK_TOKEN")
        .ok()
        .filter(|token| token.len() >= 16)
        .ok_or(
            "crane task serve needs CRANE_WEBHOOK_TOKEN set to a secret of at least 16 characters",
        )?;
    let listener = TcpListener::bind(address)
        .map_err(|error| format!("cannot listen on {address}: {error}"))?;
    let bound = listener.local_addr().map_err(|error| error.to_string())?;
    println!("Crane task webhooks listening on http://{bound}/webhooks/jira and /webhooks/asana");
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => handle(stream, &token),
            Err(error) => eprintln!("crane: connection failed: {error}"),
        }
        if once {
            break;
        }
    }
    Ok(())
}

/** Compare two secrets in time independent of where they differ
 * Input
    - left: &str - first secret
    - right: &str - second secret
 * Output
    - bool
*/
fn same_secret(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/** Handle one HTTP request and write the response; errors are reported to the client
 * Input
    - stream: TcpStream - connection
    - token: &str - shared secret
 * Output
    - None
*/
fn handle(mut stream: TcpStream, token: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let (status, body, extra) = match respond(&mut stream, token) {
        Ok(response) => response,
        Err((status, message)) => (status, json!({"error": message}), String::new()),
    };
    let text = body.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Unprocessable Entity",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{text}",
        text.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/** Read and route one request
 * Input
    - stream: &mut TcpStream - connection
    - token: &str - shared secret
 * Output
    - Result<(u16, Value, String), (u16, String)> status, body, and extra headers, or an error
      status and message
*/
fn respond(stream: &mut TcpStream, token: &str) -> Result<(u16, Value, String), (u16, String)> {
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
    let (method, path) = (
        parts.next().unwrap_or_default().to_string(),
        parts.next().unwrap_or_default().to_string(),
    );
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
    let source = match (method.as_str(), path.as_str()) {
        ("POST", "/webhooks/jira") => "jira",
        ("POST", "/webhooks/asana") => "asana",
        _ => return Err((404, "use POST /webhooks/jira or /webhooks/asana".into())),
    };
    if !headers
        .get("x-crane-token")
        .is_some_and(|given| same_secret(given, token))
    {
        return Err((401, "missing or wrong X-Crane-Token".into()));
    }
    let length = headers
        .get("content-length")
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| (400, "invalid Content-Length".to_string()))
        })
        .transpose()?
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err((413, "webhook body is larger than 1 MB".into()));
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| (400, error.to_string()))?;
    if let Some(secret) = headers.get("x-hook-secret").filter(|_| source == "asana") {
        // Asana's handshake: echo the secret to confirm the webhook
        return Ok((
            200,
            json!({"handshake": true}),
            format!("X-Hook-Secret: {secret}\r\n"),
        ));
    }
    let value: Value =
        serde_json::from_slice(&body).map_err(|error| (400, format!("invalid JSON: {error}")))?;
    let delivery = [
        "x-atlassian-webhook-identifier",
        "x-crane-delivery",
        "x-request-id",
    ]
    .iter()
    .find_map(|name| headers.get(*name))
    .cloned();
    match ingest(source, &value, delivery.as_deref()) {
        Ok(results) => Ok((200, json!({"results": results}), String::new())),
        Err(error) => Err((422, error)),
    }
}
