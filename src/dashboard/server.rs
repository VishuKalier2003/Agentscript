// The local web server of the dashboard and of the read-only observability API: GET / serves the
// single-page dashboard (when the site has one), and /api/* answers through the same route the CLI
// uses. API requests need the site's token printed at start (the page itself carries no data), one
// request is served at a time, and only on the given address. A read-only site refuses every
// method but GET before reading anything else of the request.

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

/** One API request as the router sees it
 * Fields
    - method: &'a str - HTTP method
    - target: &'a str - path and query
    - body: &'a Value - JSON body (empty on a read-only site)
    - credential: &'a str - the X-Crane-Token presented
    - operator: bool - whether the credential is the site's own token
*/
pub(crate) struct Request<'a> {
    pub(crate) method: &'a str,
    pub(crate) target: &'a str,
    pub(crate) body: &'a Value,
    pub(crate) credential: &'a str,
    pub(crate) operator: bool,
}

/** A site the server can serve
 * Fields
    - name: &'static str - name printed at start
    - token_variable: &'static str - environment variable that may set the API token
    - page: Option<&'static str> - the page served at /, None for an API-only site
    - router: fn(&Request) -> (u16, Value) - answers a request
    - read_only: bool - refuse every method but GET, and any request body
    - routes: &'static [&'static str] - path prefixes that also serve the page, so deep links such
      as /runs/ID open it (the page routes them itself)
    - viewers: bool - credentials other than the site's token reach the router, which authorizes
      them itself (scoped read access); otherwise only the site's token is accepted
*/
pub(crate) struct Site {
    pub(crate) name: &'static str,
    pub(crate) token_variable: &'static str,
    pub(crate) page: Option<&'static str>,
    pub(crate) router: fn(&Request) -> (u16, Value),
    pub(crate) read_only: bool,
    pub(crate) routes: &'static [&'static str],
    pub(crate) viewers: bool,
}

/** Route a dashboard request (the control plane's router ignores the query string; the server
 * has already required the site's own token)
 * Input
    - request: &Request - the request
 * Output
    - (u16, Value)
*/
fn dashboard_route(request: &Request) -> (u16, Value) {
    route(
        request.method,
        request.target.split('?').next().unwrap_or_default(),
        request.body,
    )
}

/** Serve the dashboard until stopped (or for one request)
 * Input
    - address: &str - address to bind
    - once: bool - stop after one request
 * Output
    - Result<(), String>
*/
pub(crate) fn serve(address: &str, once: bool) -> Result<(), String> {
    run(
        &Site {
            name: "Crane dashboard",
            token_variable: "CRANE_DASHBOARD_TOKEN",
            page: Some(PAGE),
            router: dashboard_route,
            read_only: false,
            routes: &[],
            viewers: false,
        },
        address,
        once,
    )
}

/** Serve a site until stopped (or for one request): human-only, with the site's token from its
 * environment variable (at least 16 characters) or a random one
 * Input
    - site: &Site - what to serve
    - address: &str - address to bind
    - once: bool - stop after one request
 * Output
    - Result<(), String>
*/
pub(crate) fn run(site: &Site, address: &str, once: bool) -> Result<(), String> {
    require_human(&format!("serve the {}", site.name))?;
    let token = std::env::var(site.token_variable)
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
    println!("{}: http://{bound}/?token={token}", site.name);
    std::io::stdout().flush().ok();
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        handle(stream, &token, site);
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
    - site: &Site - what is served
 * Output
    - None
*/
fn handle(mut stream: TcpStream, token: &str, site: &Site) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let (status, content_type, body) = match read(&mut stream, token, site) {
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
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
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
    - site: &Site - what is served
 * Output
    - Result<(u16, &'static str, String), (u16, String)> status, content type, and body
*/
fn read(
    stream: &mut TcpStream,
    token: &str,
    site: &Site,
) -> Result<(u16, &'static str, String), (u16, String)> {
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
    if site.read_only && method != "GET" {
        return Err((
            405,
            "this API is read-only; changes are made with the Crane CLI and its control-plane API"
                .into(),
        ));
    }
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
    let deep_link = site.routes.iter().any(|route| {
        path == *route
            || path
                .strip_prefix(route)
                .is_some_and(|rest| rest.starts_with('/') && !rest.contains(".."))
    });
    if let Some(page) = site
        .page
        .filter(|_| method == "GET" && (path == "/" || path == "/index.html" || deep_link))
    {
        return Ok((200, "text/html", page.to_string()));
    }
    if !path.starts_with("/api/") {
        return Err((404, "use / for the dashboard and /api/ for its API".into()));
    }
    let credential = headers.get("x-crane-token").cloned().unwrap_or_default();
    let operator = same(&credential, token);
    if !operator && !(site.viewers && !credential.is_empty()) {
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
    if site.read_only && length > 0 {
        return Err((400, "a read-only request takes no body".into()));
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
    let (status, answer) = (site.router)(&Request {
        method: &method,
        target: &target,
        body: &value,
        credential: &credential,
        operator,
    });
    Ok((status, "application/json", answer.to_string()))
}
