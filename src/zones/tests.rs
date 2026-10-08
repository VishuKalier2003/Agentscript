use super::model::{parse, set_version, Autonomy, Criticality, SafetyState, SelectorKind};

/** A zone file with the three zones of the examples: payments, authentication, and tests */
const ZONES: &str = "# Organization zones\nzone payments {\n    criticality critical;\n    autonomy assisted;\n    policy payments;\n    select subsystem payments;\n    select symbol java:com.acme.payments.PaymentService.charge;\n}\n\nzone authentication {\n    criticality restricted;\n    autonomy observe;\n    state quarantined;\n    select subsystem auth*;\n}\n\n// Tests are routine\nzone tests {\n    criticality routine;\n    autonomy autonomous;\n    select tests;\n}\n";

/** Test that zone files parse into zones with their declared values, sorted selectors, and the
 * default active state
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn parses_zone_definitions() {
    let zones = parse(ZONES, "zones/org.zone").unwrap();
    assert_eq!(
        zones
            .iter()
            .map(|zone| zone.zone_id.as_str())
            .collect::<Vec<_>>(),
        ["payments", "authentication", "tests"]
    );
    let payments = &zones[0];
    assert_eq!(payments.criticality, Criticality::Critical);
    assert_eq!(payments.default_autonomy, Autonomy::Assisted);
    assert_eq!(payments.safety_state, SafetyState::Active);
    assert_eq!(payments.policy_reference.as_deref(), Some("payments"));
    assert_eq!(
        payments
            .selectors
            .iter()
            .map(|selector| selector.text())
            .collect::<Vec<_>>(),
        [
            "symbol java:com.acme.payments.PaymentService.charge",
            "subsystem payments"
        ]
    );
    assert_eq!(zones[1].safety_state, SafetyState::Quarantined);
    assert_eq!(zones[2].selectors[0].kind, SelectorKind::Tests);
    assert!(payments.version.starts_with("sha256:"));
    assert!(!SelectorKind::Folder.semantic() && SelectorKind::Subsystem.semantic());
}

/** Test that a zone's version depends only on its definition: reformatting, reordering
 * selectors, comments, and its file name keep it, while any change of value changes it
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn zone_versions_ignore_layout_but_not_meaning() {
    let original = parse(ZONES, "zones/org.zone").unwrap();
    let reformatted = parse(
        "zone payments {\n  select symbol java:com.acme.payments.PaymentService.charge;\n  policy payments;\n  autonomy ASSISTED;\n  select subsystem payments;\n  select subsystem payments;\n  criticality critical;\n}\n",
        "zones/payments.zone",
    )
    .unwrap();
    assert_eq!(reformatted[0].version, original[0].version);
    assert_eq!(
        set_version(&original),
        set_version(&original.iter().rev().cloned().collect::<Vec<_>>())
    );
    for changed in [
        ZONES.replace("criticality critical", "criticality sensitive"),
        ZONES.replace("autonomy assisted", "autonomy delegated"),
        ZONES.replace("policy payments;", "policy billing;"),
        ZONES.replace("subsystem payments", "subsystem billing"),
        ZONES.replace(
            "    policy payments;\n",
            "    state degraded;\n    policy payments;\n",
        ),
    ] {
        let zones = parse(&changed, "zones/org.zone").unwrap();
        assert_ne!(zones[0].version, original[0].version, "{changed}");
        assert_ne!(set_version(&zones), set_version(&original));
    }
}

/** Test that malformed zone files are rejected with the line at fault
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn rejects_malformed_zones() {
    let block = |body: &str| format!("zone z {{\n{body}\n}}\n");
    for (content, expected) in [
        ("zone z\n".to_string(), "expected 'zone NAME {'"),
        ("zone bad name {\n}\n".to_string(), "invalid identifier"),
        (
            block("criticality critical;\nautonomy observe;"),
            "has no select statement",
        ),
        (
            block("autonomy observe;\nselect tests;"),
            "has no criticality",
        ),
        (
            block("criticality critical;\nselect tests;"),
            "has no autonomy",
        ),
        (
            block("criticality red;\nautonomy observe;\nselect tests;"),
            "invalid criticality 'red'",
        ),
        (
            block("criticality critical;\nautonomy green;\nselect tests;"),
            "invalid autonomy",
        ),
        (
            block("criticality critical\nautonomy observe;\nselect tests;"),
            "must end with ';'",
        ),
        (
            block("criticality critical;\ncriticality routine;\nautonomy observe;\nselect tests;"),
            "given twice",
        ),
        (
            block("criticality critical;\nautonomy observe;\nselect file a.rs;"),
            "invalid selectorkind",
        ),
        (
            block("criticality critical;\nautonomy observe;\nselect symbol;"),
            "needs a value",
        ),
        (
            block("criticality critical;\nautonomy observe;\nselect tests x;"),
            "takes no value",
        ),
        (
            block("criticality critical;\nautonomy observe;\nallow everything;"),
            "unknown statement",
        ),
        (
            "zone z {\ncriticality critical;\n".to_string(),
            "is not closed",
        ),
    ] {
        let error = parse(&content, "zones/x.zone").unwrap_err();
        assert!(error.contains(expected), "{content}: {error}");
        assert!(error.starts_with("zones/x.zone: line"), "{error}");
    }
}

/** Test that criticality and state cap autonomy, so a zone can only lower what an agent may do
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn criticality_and_state_cap_autonomy() {
    assert_eq!(Criticality::Restricted.autonomy_cap(), Autonomy::Observe);
    assert_eq!(Criticality::Critical.autonomy_cap(), Autonomy::Assisted);
    assert_eq!(Criticality::Sensitive.autonomy_cap(), Autonomy::Delegated);
    assert_eq!(Criticality::Routine.autonomy_cap(), Autonomy::Autonomous);
    assert_eq!(SafetyState::Quarantined.autonomy_cap(), Autonomy::Observe);
    assert_eq!(SafetyState::Degraded.autonomy_cap(), Autonomy::Assisted);
    assert!(Autonomy::Observe < Autonomy::Autonomous);
    assert!(Criticality::Restricted > Criticality::Critical);
    assert!(SafetyState::Quarantined > SafetyState::Degraded);
}

/** Test that the example in LANGUAGE.md, with trailing comments, parses
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn documented_example_parses() {
    let language = include_str!("../../LANGUAGE.md").replace("\r\n", "\n");
    let start = language.find("zone payments {").expect("example present");
    let end = start + language[start..].find("\n}\n").expect("example closed") + 3;
    let zones = parse(&language[start..end], "LANGUAGE.md").unwrap();
    assert_eq!(zones[0].selectors.len(), 2);
    assert_eq!(zones[0].policy_reference.as_deref(), Some("payments"));
    assert_eq!(zones[0].safety_state, SafetyState::Active);
}

/** Test that a file whose zones select some of its symbols restricts only changes to those
 * symbols, keeps its whole-file zones for every change, and falls back to everything when the
 * changed symbols are unknown or the file is new
 * Input
    - None
 * Output
    - None (panics on assertion failure)
*/
#[test]
fn symbol_constraints_fail_closed() {
    use crate::agent_session::{FileConstraint, Governance};
    use std::collections::{BTreeMap, BTreeSet};
    let constraint = |zone: &str, criticality, autonomy| FileConstraint {
        zones: vec![zone.into()],
        criticality,
        autonomy,
        state: SafetyState::Active,
        grant: None,
        symbols: BTreeMap::new(),
        whole: None,
    };
    let whole = constraint("folder", Criticality::Sensitive, Autonomy::Delegated);
    let charge = constraint("money", Criticality::Critical, Autonomy::Assisted);
    let mut file = whole.clone().merge(charge.clone());
    file.symbols.insert("Pay.charge".into(), charge);
    file.whole = Some(Box::new(whole));
    let mut governance = Governance::plain();
    governance.files.insert("pay/service.py".into(), file);
    let names = |list: &[&str]| {
        list.iter()
            .map(|name| name.to_string())
            .collect::<BTreeSet<_>>()
    };
    let (other, on) = governance
        .constraint_for("pay/service.py", Some(&names(&["Pay.fee"])))
        .unwrap();
    assert_eq!(
        (other.autonomy, on.as_str()),
        (Autonomy::Delegated, "pay/service.py")
    );
    let (touched, on) = governance
        .constraint_for("pay/service.py", Some(&names(&["Pay", "Pay.charge"])))
        .unwrap();
    assert_eq!(touched.autonomy, Autonomy::Assisted);
    assert_eq!(on, "pay/service.py::Pay.charge");
    assert_eq!(touched.zones, ["folder", "money"]);
    let (unknown, _) = governance.constraint_for("pay/service.py", None).unwrap();
    assert_eq!(unknown.autonomy, Autonomy::Assisted);
    let (new_file, _) = governance
        .constraint_for("pay/new.py", Some(&BTreeSet::new()))
        .unwrap();
    assert_eq!(new_file.autonomy, Autonomy::Assisted);
    assert!(governance.constraint_for("docs/readme.md", None).is_none());
}
