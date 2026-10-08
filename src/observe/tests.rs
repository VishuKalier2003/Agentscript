use serde_json::json;

use super::{decode, identifier, route, Query};

/** Every method but GET is refused before anything is read */
#[test]
fn only_get_is_answered() {
    for method in ["POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"] {
        let (status, body) = route(method, "/api/v1/overview");
        assert_eq!(status, 405, "{method}");
        assert_eq!(body["allowed"], json!(["GET"]));
    }
}

/** Unknown endpoints and path traversal are refused */
#[test]
fn only_the_endpoint_table_is_reachable() {
    for path in [
        "/api/v1/sql",
        "/api/v1/sessions/a/b/c",
        "/api/v2/overview",
        "/api/v1/policies/edit",
    ] {
        assert_eq!(route("GET", path).0, 404, "{path}");
    }
    assert_eq!(route("GET", "/api/v1/sessions/..").0, 400);
    assert_eq!(route("GET", "/api/v1/sessions/.crane").0, 400);
    assert!(identifier("claude-task-PAY-1830-v1").is_ok());
    assert!(identifier("a/b").is_err());
    assert!(identifier(&"x".repeat(129)).is_err());
}

/** Parameters are a typed allow-list per endpoint */
#[test]
fn parameters_are_typed() {
    let query = Query::parse(
        "decision=deny&policy=payments&limit=5",
        &["decision", "policy", "limit"],
    )
    .unwrap();
    assert_eq!(query.get("policy"), Some("payments"));
    assert_eq!(query.limit().unwrap(), 5);
    for (text, expected) in [
        (
            "decision=maybe",
            "decision must be one of allow, deny, approval_required",
        ),
        ("limit=0", "limit must be a number from 1 to 1000"),
        ("limit=5000", "limit must be a number from 1 to 1000"),
        ("sql=select", "unknown parameter 'sql'"),
        ("decision=deny&decision=allow", "given twice"),
        ("decision=", "is empty"),
        ("decision", "has no value"),
    ] {
        let error = Query::parse(text, &["decision", "limit"])
            .err()
            .unwrap_or_default();
        assert!(error.contains(expected), "{text}: {error}");
    }
    assert!(Query::parse("x=1", &[])
        .unwrap_err()
        .contains("takes no parameters"));
    assert_eq!(route("GET", "/api/v1/decisions?severity=critical").0, 400);
    assert_eq!(decode("a%20b+c").unwrap(), "a b c");
    assert!(decode("%zz").is_err());
}

/** The Overview page offers no action: no control is labelled with an action verb, and it only
 * ever sends GET requests */
#[test]
fn the_page_has_no_mutation_controls() {
    let page = include_str!("app.html");
    for line in page.lines().filter(|line| line.contains("h(\"button\"")) {
        for verb in [
            "Run",
            "Execute",
            "Approve",
            "Edit",
            "Delete",
            "Modify",
            "Apply",
            "Merge",
            "Start",
            "Stop",
            "Restart",
            "Change",
            "Set",
            "Promote",
            "Demote",
            "Refill",
            "Credit",
            "Grant",
            "Revoke",
            "Resume",
            "Cancel",
            "Kill",
            "Except",
            "Create",
            "Update",
            "Save",
            "Submit",
            "Assign",
            "Reset",
            "Deploy",
            "Connect",
            "Disconnect",
            "Rollback",
            "Override",
        ] {
            assert!(
                !line.contains(&format!("\"{verb}")),
                "a button says {verb}: {line}"
            );
        }
    }
    for method in ["\"POST\"", "\"PUT\"", "\"PATCH\"", "\"DELETE\""] {
        assert!(!page.contains(method), "the page sends {method}");
    }
    assert!(
        !page.contains("innerHTML"),
        "record data is only ever rendered as text"
    );
    assert!(page.contains("method: \"GET\""));
    // No input surface for changes: no forms, editable regions, free-text areas, or uploads, and no
    // console of any kind (the inputs that exist search and filter)
    for marker in [
        "<form",
        "h(\"form\"",
        "contenteditable",
        "<textarea",
        "h(\"textarea\"",
        "type: \"file\"",
        "type=\"file\"",
        "eval(",
        "new Function",
        "console",
    ] {
        assert!(!page.contains(marker), "the page has {marker}");
    }
}

/** The observability module cannot change anything: none of its code writes, renames, or removes
 * a file, creates a directory, changes the working directory or the environment, or starts a
 * process (git plumbing reads run only through the repository module's checkpoint reader) */
#[test]
fn the_module_has_no_write_path() {
    let sources = [
        ("mod.rs", include_str!("mod.rs")),
        ("access.rs", include_str!("access.rs")),
        ("agents.rs", include_str!("agents.rs")),
        ("autonomy.rs", include_str!("autonomy.rs")),
        ("cache.rs", include_str!("cache.rs")),
        ("catalog.rs", include_str!("catalog.rs")),
        ("compact.rs", include_str!("compact.rs")),
        ("dashboard.rs", include_str!("dashboard.rs")),
        ("detail.rs", include_str!("detail.rs")),
        ("repos.rs", include_str!("repos.rs")),
        ("runs.rs", include_str!("runs.rs")),
        ("server.rs", include_str!("server.rs")),
    ];
    for (name, text) in sources {
        for call in [
            "fs::write",
            "File::create",
            "OpenOptions",
            "create_dir",
            "remove_file",
            "remove_dir",
            "fs::rename",
            "fs::copy",
            "set_permissions",
            "set_current_dir",
            "set_var",
            "remove_var",
            "Command::new",
            "process::Command",
            "::append(",
            "::save(",
            "::record(",
            "::write_",
        ] {
            assert!(!text.contains(call), "{name} calls {call}");
        }
    }
}

/** Grants admit exactly what they name: "*" admits any value (even none), a list admits only its
 * names (a record without the key is not admitted), and a scope admits a record only when every
 * key does */
#[test]
fn grants_admit_exactly_what_they_name() {
    use super::access::{Grant, Scope};
    let names = |list: &[&str]| Grant::Only(list.iter().map(|name| name.to_string()).collect());
    assert!(Grant::Any.admits(None) && Grant::Any.admits(Some("x")));
    assert!(names(&["a"]).admits(Some("a")));
    assert!(!names(&["a"]).admits(Some("b")) && !names(&["a"]).admits(None));
    let scope = Scope {
        viewer: "pay".into(),
        operator: false,
        organizations: names(&["acme"]),
        repositories: names(&["acme/shop"]),
        teams: names(&["payments"]),
    };
    assert!(scope.admits_repository(Some("acme"), "acme/shop"));
    assert!(!scope.admits_repository(Some("acme"), "acme/other"));
    assert!(!scope.admits_repository(Some("globex"), "acme/shop"));
    assert!(!scope.admits_repository(None, "acme/shop"));
    assert!(scope.admits_record(Some("acme"), Some("payments")));
    assert!(!scope.admits_record(Some("acme"), Some("catalog")));
    assert!(
        !scope.admits_record(Some("acme"), None),
        "a session without a team is not a team's"
    );
    assert!(!scope.admits_record(Some("globex"), Some("payments")));
    assert!(scope.restricts_teams());
    assert!(!Scope::operator().restricts_teams());
    assert!(Scope::operator().admits_record(None, None));
    assert!(!scope.to_json().to_string().contains("token"));
    // Outside a request nothing is admitted (fail closed); a request's scope ends with it
    assert!(!super::access::current().admits_record(Some("acme"), Some("payments")));
    super::access::within(scope, || {
        assert_eq!(super::access::current().viewer, "pay");
    });
    assert_eq!(super::access::current().viewer, "nobody");
}

/** Every list and timeline is bounded: list limits and offsets are capped, custom windows span at
 * most a year and a day, and a window cannot reach far into the future */
#[test]
fn queries_are_bounded() {
    for path in [
        "/api/v1/decisions?limit=1001",
        "/api/v1/decisions?limit=0",
        "/api/v1/decisions?offset=99999999999",
        "/api/v1/decisions?offset=-1",
    ] {
        assert_eq!(route("GET", path).0, 400, "{path}");
    }
    let now = crate::util::now_unix();
    let query = |text: &str| Query::parse(text, &["range", "from", "to"]).unwrap();
    let window = |text: &str| super::dashboard::Window::parse(&query(text), now);
    assert!(window(&format!(
        "range=custom&from={}&to={now}",
        now - 366 * 86_400
    ))
    .is_ok());
    assert!(window(&format!(
        "range=custom&from={}&to={now}",
        now - 367 * 86_400
    ))
    .is_err());
    assert!(window(&format!(
        "range=custom&from={}&to={}",
        now,
        now + 3 * 86_400
    ))
    .is_err());
    assert!(window("range=90d").is_err());
}
