// The read-only observability server: the dashboard's hardened HTTP handling, serving the Society
// Overview page at / and only the GET-only observability router under /api/, with its own token
// (CRANE_OBSERVE_TOKEN), so read access never implies control-plane access.

use serde_json::{json, Value};

use crate::dashboard::server::{run, Request, Site};

/** Paths that open the page directly at a view (deep links) */
const ROUTES: &[&str] = &[
    "/overview",
    "/runs",
    "/tasks",
    "/agents",
    "/policies",
    "/repositories",
    "/zones",
    "/violations",
    "/autonomy",
];

/** How often the background revalidation runs */
const REFRESH: std::time::Duration = std::time::Duration::from_secs(2);

/** The read-only Overview page (it carries no data; it reads the API with the token) */
const PAGE: &str = include_str!("app.html");

/** Route a request to the observability API under the scope its credential grants: the server's
 * own token reads everything, a registered viewer's token reads its organizations, repositories,
 * and teams, and anything else is refused (the request takes no body; the server refuses every
 * method but GET before routing)
 * Input
    - request: &Request - the request
 * Output
    - (u16, Value)
*/
fn observe_route(request: &Request) -> (u16, Value) {
    match super::access::authenticate(request.credential, request.operator) {
        Ok(Some(scope)) => super::route_as(scope, request.method, request.target),
        Ok(None) => (
            401,
            json!({"error": "missing or wrong X-Crane-Token (the operator token printed at start, or a registered viewer's token)"}),
        ),
        Err(error) => (500, json!({"error": error})),
    }
}

/** Serve the read-only observability API until stopped (or for one request)
 * Input
    - address: &str - address to bind
    - once: bool - stop after one request
 * Output
    - Result<(), String>
*/
pub(crate) fn serve(address: &str, once: bool) -> Result<(), String> {
    // Load everything before listening, then keep revalidating in the background, so a request
    // reads memory rather than the file system (the records stay the source of truth: whatever
    // changed is recomputed from them within a few seconds)
    let started = std::time::Instant::now();
    let cores = std::thread::available_parallelism().map_or(4, |count| count.get());
    let (sessions, tasks) = super::refresh(cores)?;
    eprintln!(
        "loaded {sessions} sessions and {tasks} task records in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    let root = std::env::current_dir().map_err(|error| error.to_string())?;
    // The background pass uses a quarter of the cores and waits at least twice as long as the
    // last pass took, so it never takes more than a third of the time of the threads it uses
    std::thread::spawn(move || {
        let mut pause = REFRESH;
        loop {
            std::thread::sleep(pause);
            if std::env::current_dir().is_ok_and(|current| current == root) {
                let pass = std::time::Instant::now();
                let _ = super::refresh((cores / 4).max(1));
                pause = REFRESH.max(pass.elapsed() * 2);
            }
        }
    });
    run(
        &Site {
            name: "Society Overview (read-only)",
            token_variable: "CRANE_OBSERVE_TOKEN",
            page: Some(PAGE),
            router: observe_route,
            read_only: true,
            routes: ROUTES,
            viewers: true,
        },
        address,
        once,
    )
}
