use super::providers::{
    named, parse, sanitize, select, GitHost, GitHub, Local, Remote, RepositoryProvider,
};

/** Parse a remote and require it to parse
 * Input
    - url: &str - remote URL
 * Output
    - Remote
*/
fn remote(url: &str) -> Remote {
    parse(url).unwrap_or_else(|| panic!("{url} did not parse"))
}

/** Every common remote form yields host, owner, and name */
#[test]
fn remotes_parse_in_every_form() {
    for url in [
        "https://github.com/acme/shop.git",
        "https://github.com/acme/shop",
        "git@github.com:acme/shop.git",
        "ssh://git@github.com/acme/shop.git",
        "ssh://git@github.com:22/acme/shop",
        "https://github.com/acme/shop/",
    ] {
        let parsed = remote(url);
        assert_eq!(
            (
                parsed.host.as_deref(),
                parsed.owner.as_deref(),
                parsed.name.as_str()
            ),
            (Some("github.com"), Some("acme"), "shop"),
            "{url}"
        );
    }
    let nested = remote("https://gitlab.example.com/platform/payments/shop.git");
    assert_eq!(nested.owner.as_deref(), Some("platform/payments"));
    assert_eq!(nested.name, "shop");
    for local in [
        "/srv/git/shop.git",
        "C:\\repos\\shop",
        "file:///srv/git/shop.git",
        "../shop",
    ] {
        let parsed = remote(local);
        assert_eq!(
            (
                parsed.host.as_deref(),
                parsed.owner.as_deref(),
                parsed.name.as_str()
            ),
            (None, None, "shop"),
            "{local}"
        );
    }
    assert!(parse("").is_none());
}

/** Credentials in a remote URL are never kept */
#[test]
fn credentials_are_removed() {
    assert_eq!(
        sanitize("https://bot:ghp_secret123@github.com/acme/shop.git"),
        "https://github.com/acme/shop.git"
    );
    assert_eq!(
        sanitize("https://ghp_secret123@github.com/acme/shop.git"),
        "https://github.com/acme/shop.git"
    );
    assert_eq!(
        remote("https://bot:ghp_secret123@github.com/acme/shop.git").url,
        "https://github.com/acme/shop.git"
    );
    assert_eq!(
        sanitize("ssh://git@github.com/acme/shop.git"),
        "ssh://git@github.com/acme/shop.git",
        "an SSH user is not a secret"
    );
}

/** Providers are chosen behind one trait: GitHub for github.com and github.* hosts, the generic
 * Git provider for other hosts, local for everything else; a named provider is honoured when it
 * can work, and refused when it cannot */
#[test]
fn providers_are_selected_through_the_abstraction() {
    let chosen = |url: &str| select(&remote(url), None).unwrap().name();
    assert_eq!(chosen("git@github.com:acme/shop.git"), "github");
    assert_eq!(
        chosen("https://github.acme.internal/payments/shop"),
        "github",
        "GitHub Enterprise"
    );
    assert_eq!(chosen("https://gitlab.com/acme/shop.git"), "git");
    assert_eq!(chosen("/srv/git/shop.git"), "local");
    let enterprise = remote("https://code.acme.internal/payments/shop.git");
    assert_eq!(
        select(&enterprise, Some("github")).unwrap().name(),
        "github",
        "forced for a custom host"
    );
    assert!(select(&remote("/srv/git/shop.git"), Some("github")).is_err());
    assert!(select(&enterprise, Some("bitbucket"))
        .err()
        .unwrap()
        .contains("unknown provider"));

    let github = remote("https://github.com/acme/shop.git");
    let providers: Vec<Box<dyn RepositoryProvider>> =
        vec![Box::new(GitHub), Box::new(GitHost), Box::new(Local)];
    let urls = providers
        .iter()
        .map(|provider| provider.web_url(&github))
        .collect::<Vec<_>>();
    assert_eq!(
        urls,
        [
            Some("https://github.com/acme/shop".to_string()),
            Some("https://github.com/acme/shop".to_string()),
            None
        ]
    );
    assert_eq!(
        GitHub.pull_request_url(&github).unwrap(),
        "https://github.com/acme/shop/pull/{number}"
    );
    assert!(GitHost.pull_request_url(&github).is_none());
    assert_eq!(
        GitHub.capabilities()["network_checked"],
        false,
        "providers never call the network"
    );
    assert_eq!(named("git").unwrap().name(), "git");
    assert!(named("svn").is_none());
}
