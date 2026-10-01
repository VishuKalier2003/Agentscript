use super::engine::{domains, region_of, symbol_confidence, Confidence, Signal};

/** Build signals from their names
 * Input
    - names: &[&str] - signal names
 * Output
    - Vec<Signal>
*/
fn signals(names: &[&str]) -> Vec<Signal> {
    names
        .iter()
        .map(|name| Signal {
            signal: name.to_string(),
            detail: String::new(),
        })
        .collect()
}

/** Test the vocabulary heuristics: whole words (and plurals) only, so words such as display or
 * signal do not match, and key or token word pairs count as secrets
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn domain_words_match_whole_words() {
    let names = |text: &str| {
        domains(text)
            .into_iter()
            .map(|(domain, word)| format!("{domain}:{word}"))
            .collect::<Vec<_>>()
    };
    assert_eq!(names("PaymentService"), ["payment:payment"]);
    assert_eq!(names("payments.api"), ["payment:payment"]);
    assert_eq!(names("authenticateUser"), ["auth:authenticate"]);
    assert_eq!(names("API_KEY"), ["secrets:api key"]);
    assert_eq!(names("refreshAccessToken"), ["secrets:access token"]);
    assert_eq!(names("hashPassword"), ["secrets:password"]);
    assert_eq!(
        names("loginWithPassword"),
        ["auth:login", "secrets:password"]
    );
    for text in [
        "display",
        "signal",
        "author",
        "keyboard",
        "tokenize",
        "billboard",
    ] {
        assert!(domains(text).is_empty(), "{text}");
    }
}

/** Test the fixed confidence rules for symbols
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn confidence_follows_fixed_rules() {
    let cases: [(&[&str], Option<Confidence>); 11] = [
        (&["zone_critical"], Some(Confidence::High)),
        (&["zone_restricted"], Some(Confidence::High)),
        (&["payment_name", "payment_context"], Some(Confidence::High)),
        (&["auth_name", "tested"], Some(Confidence::High)),
        (&["secrets_name", "owned"], Some(Confidence::High)),
        (&["payment_name", "high_centrality"], Some(Confidence::High)),
        (&["payment_name"], Some(Confidence::Medium)),
        (&["zone_sensitive"], Some(Confidence::Medium)),
        (
            &["auth_context", "high_centrality"],
            Some(Confidence::Medium),
        ),
        (&["auth_context"], Some(Confidence::Low)),
        (&["tested", "owned"], None),
    ];
    for (names, expected) in cases {
        assert_eq!(symbol_confidence(&signals(names)), expected, "{names:?}");
    }
    assert_eq!(
        symbol_confidence(&signals(&["high_centrality"])),
        Some(Confidence::Low)
    );
    assert!(Confidence::High > Confidence::Medium && Confidence::Medium > Confidence::Low);
    assert_eq!(Confidence::parse("medium"), Ok(Confidence::Medium));
    assert!(Confidence::parse("certain").is_err());
}

/** Test the path conventions for regions no rule can target
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn regions_follow_path_conventions() {
    let region = |path: &str| region_of(path);
    assert_eq!(
        region("db/migrations/001_init.sql"),
        Some(("migration", "db/migrations".into(), true))
    );
    assert_eq!(
        region("app/alembic/versions/x.py"),
        Some(("migration", "app/alembic".into(), true))
    );
    assert_eq!(
        region("sql/V2__add_users.sql"),
        Some(("migration", "sql".into(), true))
    );
    assert_eq!(
        region(".github/workflows/ci.yml"),
        Some(("infrastructure", ".github/workflows".into(), true))
    );
    assert_eq!(
        region("ops/terraform/main.tf"),
        Some(("infrastructure", "ops/terraform".into(), true))
    );
    assert_eq!(
        region("main.tf"),
        Some(("infrastructure", "main.tf".into(), false))
    );
    assert_eq!(
        region("svc/Dockerfile"),
        Some(("infrastructure", "svc/Dockerfile".into(), false))
    );
    assert_eq!(
        region("config/application-prod.yml"),
        Some((
            "production_config",
            "config/application-prod.yml".into(),
            false
        ))
    );
    assert_eq!(
        region(".env.production"),
        Some(("production_config", ".env.production".into(), false))
    );
    assert_eq!(
        region("config/secrets.yml"),
        Some(("secrets_config", "config/secrets.yml".into(), false))
    );
    for path in [
        "src/product.py",
        "config/application-dev.yml",
        "docs/production.md",
        "src/migrate_helper.rs",
    ] {
        assert_eq!(region(path), None, "{path}");
    }
}
