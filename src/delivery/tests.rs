use serde_json::json;

use super::policy::{applies, evaluate, DeliveryConfig, Exception, Facts};
use super::slack::{parse, url_decode, verify};
use crate::util::hmac_sha256;
use crate::zones::model::{Autonomy, Criticality};

/** Facts of a clean round: PASS, safe, all checks passing, branch unmoved, nothing decided
 * Input
    - autonomy: Autonomy - session autonomy
    - criticality: Criticality - change criticality
 * Output
    - Facts
*/
fn clean(autonomy: Autonomy, criticality: Criticality) -> Facts {
    Facts {
        session: "claude-s1".into(),
        decision: "PASS".into(),
        autonomy,
        safety: "active".into(),
        criticality,
        contract_failed: 0,
        checks: vec![
            ("repository_tests".into(), "passed".into()),
            ("lint".into(), "passed".into()),
        ],
        head: "abc".into(),
        branch_head: Some("abc".into()),
        approvals: Vec::new(),
        blocks: Vec::new(),
        exceptions: Vec::new(),
        merged: false,
        now: 1_000,
    }
}

/** HMAC-SHA256 matches RFC 4231 test cases 1, 2, and 6 (a key longer than a block) */
#[test]
fn hmac_matches_rfc_4231() {
    assert_eq!(
        hmac_sha256(&[0x0b; 20], b"Hi There"),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    assert_eq!(
        hmac_sha256(b"Jefe", b"what do ya want for nothing?"),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    assert_eq!(
        hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        ),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}

/** Slack signatures: a correct one verifies; a wrong secret, an edited body, and a stale
 * timestamp do not */
#[test]
fn slack_signatures() {
    let body = "payload=%7B%22type%22%3A%22block_actions%22%7D";
    let signature = format!(
        "v0={}",
        hmac_sha256(b"secret", format!("v0:1000:{body}").as_bytes())
    );
    assert!(verify("secret", "1000", body, &signature, 1_100).is_ok());
    assert!(verify("other", "1000", body, &signature, 1_100)
        .unwrap_err()
        .contains("does not match"));
    assert!(verify("secret", "1000", &format!("{body}x"), &signature, 1_100).is_err());
    assert!(verify("secret", "1000", body, &signature, 1_000 + 301)
        .unwrap_err()
        .contains("older than five minutes"));
    assert!(verify("secret", "soon", body, &signature, 1_000).is_err());
}

/** Slack payloads: form decoding, the click, and rejection of foreign buttons */
#[test]
fn slack_payloads() {
    assert_eq!(url_decode("a+b%20c%2Fd%zz"), "a b c/d%zz");
    let payload = json!({"type": "block_actions", "user": {"id": "U1"}, "actions": [{"action_id": "approve_exception", "value": json!({"delivery": "claude-s1", "head": "abc", "check": "lint"}).to_string()}]});
    let encoded = payload
        .to_string()
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    let click = parse(&format!("payload={encoded}")).unwrap();
    assert_eq!(
        (
            click.action.as_str(),
            click.user.as_str(),
            click.delivery.as_str(),
            click.head.as_str(),
            click.check.as_deref()
        ),
        ("approve_exception", "U1", "claude-s1", "abc", Some("lint"))
    );
    let foreign = json!({"type": "block_actions", "user": {"id": "U1"}, "actions": [{"action_id": "merge_now", "value": "{}"}]});
    assert!(parse(&format!("payload={}", foreign))
        .unwrap_err()
        .contains("unknown Slack action"));
    assert!(parse("token=x").unwrap_err().contains("no payload"));
}

/** Merge eligibility by autonomy and zone criticality: autonomous + routine + all checks passing
 * merges automatically; a delegated routine change needs one approval; critical changes need two
 */
#[test]
fn eligibility_follows_autonomy_and_criticality() {
    let config = DeliveryConfig::default();
    let auto = evaluate(&config, &clean(Autonomy::Autonomous, Criticality::Routine));
    assert_eq!(auto["eligible"], true);
    assert_eq!(auto["auto_merge"], true);
    assert_eq!(auto["rule"]["name"], "autonomous-routine");

    let delegated = clean(Autonomy::Delegated, Criticality::Routine);
    let waiting = evaluate(&config, &delegated);
    assert_eq!(waiting["eligible"], false);
    assert_eq!(waiting["missing"], json!(["0 of 1 required approvals"]));
    let mut approved = delegated;
    approved.approvals.push(("lead".into(), "abc".into()));
    let approved = evaluate(&config, &approved);
    assert_eq!(approved["eligible"], true);
    assert_eq!(
        approved["auto_merge"], false,
        "the default rule does not merge by itself"
    );

    let mut critical = clean(Autonomy::Autonomous, Criticality::Critical);
    critical.approvals = vec![("lead".into(), "abc".into()), ("lead".into(), "abc".into())];
    assert_eq!(
        evaluate(&config, &critical)["missing"],
        json!(["1 of 2 required approvals"]),
        "one person counts once"
    );
    critical.approvals.push(("security".into(), "old".into()));
    assert_eq!(
        evaluate(&config, &critical)["eligible"],
        false,
        "approvals of another commit do not count"
    );
    critical.approvals.push(("security".into(), "abc".into()));
    let ready = evaluate(&config, &critical);
    assert_eq!(
        (ready["eligible"].clone(), ready["auto_merge"].clone()),
        (json!(true), json!(false))
    );
}

/** Everything else that blocks a merge: a failing session, an unsafe session, contract tests, a
 * failing check, a moved branch, a rejection, requested changes, a previous merge */
#[test]
fn eligibility_requires_every_condition() {
    let config = DeliveryConfig::default();
    let blocked = |change: &dyn Fn(&mut Facts)| {
        let mut facts = clean(Autonomy::Autonomous, Criticality::Routine);
        change(&mut facts);
        let result = evaluate(&config, &facts);
        assert_eq!(result["eligible"], false);
        result["missing"][0].as_str().unwrap().to_string()
    };
    assert!(blocked(&|facts| facts.decision = "FAIL".into()).contains("FAIL"));
    assert!(blocked(&|facts| facts.safety = "degraded".into()).contains("degraded"));
    assert!(blocked(&|facts| facts.contract_failed = 1).contains("never be excepted"));
    assert!(blocked(&|facts| facts.checks[1].1 = "failed".into()).contains("check lint is failed"));
    assert!(blocked(&|facts| facts.branch_head = Some("def".into())).contains("branch moved"));
    assert!(blocked(&|facts| facts
        .blocks
        .push(("rejection".into(), "lead".into(), "abc".into())))
    .contains("lead rejected"));
    assert!(blocked(&|facts| facts.blocks.push((
        "changes_requested".into(),
        "lead".into(),
        "abc".into()
    )))
    .contains("requested changes"));
    assert!(blocked(&|facts| facts.merged = true).contains("already merged"));
}

/** Exceptions are scoped to one check, one session, and one commit, and expire */
#[test]
fn exceptions_are_scoped_and_temporary() {
    let config = DeliveryConfig::default();
    let mut facts = clean(Autonomy::Autonomous, Criticality::Routine);
    facts.checks[1].1 = "failed".into();
    let exception = Exception {
        id: "x1".into(),
        check: "lint".into(),
        session: "claude-s1".into(),
        head: "abc".into(),
        approver: "lead".into(),
        reason: "flaky".into(),
        expires_at: 2_000,
    };
    assert!(applies(&exception, "lint", &facts));
    assert!(
        !applies(&exception, "repository_tests", &facts),
        "another check"
    );
    assert!(
        !applies(
            &Exception {
                session: "claude-s2".into(),
                ..exception.clone()
            },
            "lint",
            &facts
        ),
        "another session"
    );
    assert!(
        !applies(
            &Exception {
                head: "old".into(),
                ..exception.clone()
            },
            "lint",
            &facts
        ),
        "another commit"
    );
    facts.exceptions.push(exception.clone());
    let excepted = evaluate(&config, &facts);
    assert_eq!(excepted["eligible"], true);
    assert_eq!(excepted["excepted"][0]["exception"], "x1");
    facts.now = 2_000;
    assert_eq!(
        evaluate(&config, &facts)["eligible"],
        false,
        "an expired exception no longer applies"
    );
    facts.now = 1_000;
    facts.contract_failed = 1;
    facts.exceptions.push(Exception {
        check: "contract_tests".into(),
        ..exception
    });
    assert_eq!(
        evaluate(&config, &facts)["eligible"],
        false,
        "contract tests are never excepted"
    );
}

/** The configuration is validated: unknown settings, rules that would merge critical changes
 * without approval, impossible approver lists, excepting contract tests, reserved check names */
#[test]
fn configuration_is_validated() {
    for (config, expected) in [
        (json!({"merge": {}}), "unknown setting 'merge'"),
        (json!({"provider": "gitlab"}), "unknown provider"),
        (
            json!({"merge_policy": {"rules": [{"name": "yolo", "approvals": 0}]}}),
            "critical or restricted changes without approval",
        ),
        (
            json!({"merge_policy": {"default": {"approvals": 0, "max_criticality": "critical"}}}),
            "without approval",
        ),
        (
            json!({"merge_policy": {"rules": [{"approvals": 2, "approvers": ["lead"], "min_criticality": "critical"}]}}),
            "names only 1 approvers",
        ),
        (
            json!({"exceptions": {"allowed_checks": ["contract_tests"]}}),
            "never be excepted",
        ),
        (
            json!({"checks": [{"name": "repository_tests", "command": ["x"]}]}),
            "reserved",
        ),
        (
            json!({"checks": [{"name": "lint", "command": []}]}),
            "needs a program",
        ),
    ] {
        let error = DeliveryConfig::from_json(&config).unwrap_err();
        assert!(error.contains(expected), "{config}: {error}");
    }
    let config = DeliveryConfig::from_json(&json!({
        "merge_policy": {"rules": [{"name": "docs", "max_criticality": "routine", "approvals": 0, "auto_merge": true}]},
        "exceptions": {"allowed_checks": ["lint"], "max_duration_seconds": 3600, "default_duration_seconds": 7200},
    }))
    .unwrap();
    assert_eq!(
        config
            .rule_for(Autonomy::Delegated, Criticality::Routine)
            .name,
        "docs"
    );
    assert_eq!(
        config
            .rule_for(Autonomy::Delegated, Criticality::Sensitive)
            .name,
        "default"
    );
    assert_eq!(
        config.default_exception, 3600,
        "the default never exceeds the maximum"
    );
}
