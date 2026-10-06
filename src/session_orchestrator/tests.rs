use std::collections::{BTreeSet, VecDeque};

use super::Phase::{self, *};

/** Every transition the lifecycle allows, written out independently of Phase::allows */
const ALLOWED: &[(Phase, Phase)] = &[
    (TaskReady, SessionCreated),
    (TaskReady, Failed),
    (SessionCreated, AgentConnected),
    (SessionCreated, Stopping),
    (SessionCreated, Failed),
    (AgentConnected, Running),
    (AgentConnected, Degraded),
    (AgentConnected, Quarantined),
    (AgentConnected, Stopping),
    (AgentConnected, Failed),
    (Running, Degraded),
    (Running, Quarantined),
    (Running, Stopping),
    (Running, Failed),
    (Degraded, Running),
    (Degraded, Quarantined),
    (Degraded, Stopping),
    (Degraded, Failed),
    (Quarantined, Running),
    (Quarantined, Degraded),
    (Quarantined, Stopping),
    (Quarantined, Failed),
    (Stopping, Reconciling),
    (Stopping, Failed),
    (Reconciling, Verified),
    (Reconciling, Failed),
    (Verified, DeliveryReady),
];

/** Return every phase reachable from one, following allowed transitions
 * Input
    - from: Phase - start
 * Output
    - BTreeSet<Phase> reachable phases (excluding the start unless a cycle returns to it)
*/
fn reachable(from: Phase) -> BTreeSet<Phase> {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([from]);
    while let Some(phase) = queue.pop_front() {
        for next in Phase::ALL {
            if phase.allows(next) && seen.insert(next) {
                queue.push_back(next);
            }
        }
    }
    seen
}

/** All 121 ordered pairs of phases are checked against the table: exactly the listed
 * transitions are allowed */
#[test]
fn every_transition_matches_the_table() {
    let mut allowed = 0;
    for from in Phase::ALL {
        for to in Phase::ALL {
            let expected = ALLOWED.contains(&(from, to));
            assert_eq!(
                from.allows(to),
                expected,
                "{} -> {}",
                from.name(),
                to.name()
            );
            allowed += usize::from(expected);
        }
    }
    assert_eq!(allowed, ALLOWED.len());
    assert_eq!(Phase::ALL.len() * Phase::ALL.len(), 121);
}

/** No phase moves to itself (a repeated phase is a no-op, never a transition) */
#[test]
fn no_self_transitions() {
    for phase in Phase::ALL {
        assert!(!phase.allows(phase), "{}", phase.name());
    }
}

/** Names round-trip, and unknown names are refused */
#[test]
fn names_round_trip() {
    for phase in Phase::ALL {
        assert_eq!(Phase::parse(phase.name()), Ok(phase));
    }
    assert!(Phase::parse("DONE").is_err());
}

/** The lifecycle can reach every phase from TASK_READY, and the happy path is a path */
#[test]
fn every_phase_is_reachable_and_the_happy_path_holds() {
    let from_start = reachable(TaskReady);
    for phase in Phase::ALL.into_iter().filter(|phase| *phase != TaskReady) {
        assert!(from_start.contains(&phase), "{}", phase.name());
    }
    let happy = [
        TaskReady,
        SessionCreated,
        AgentConnected,
        Running,
        Stopping,
        Reconciling,
        Verified,
        DeliveryReady,
    ];
    for pair in happy.windows(2) {
        assert!(
            pair[0].allows(pair[1]),
            "{} -> {}",
            pair[0].name(),
            pair[1].name()
        );
    }
    for safety in [Degraded, Quarantined] {
        assert!(Running.allows(safety) && safety.allows(Running) && safety.allows(Stopping));
    }
}

/** FAILED and DELIVERY_READY are terminal; VERIFIED only moves on to DELIVERY_READY */
#[test]
fn terminal_phases_never_move() {
    assert!(reachable(Failed).is_empty());
    assert!(reachable(DeliveryReady).is_empty());
    assert_eq!(reachable(Verified), BTreeSet::from([DeliveryReady]));
    for phase in Phase::ALL {
        assert_eq!(
            phase.terminal(),
            reachable(phase).is_empty(),
            "{}",
            phase.name()
        );
    }
}

/** A failed verification can never become success: FAILED reaches nothing, and VERIFIED is
 * entered only from RECONCILING */
#[test]
fn verified_only_from_reconciliation() {
    for from in Phase::ALL {
        assert_eq!(
            from.allows(Verified),
            from == Reconciling,
            "{}",
            from.name()
        );
        assert_eq!(
            from.allows(DeliveryReady),
            from == Verified,
            "{}",
            from.name()
        );
    }
    assert!(!reachable(Failed).contains(&Verified));
}

/** Freezing is irreversible: once STOPPING, the session never acts again */
#[test]
fn freezing_is_irreversible() {
    let after = reachable(Stopping);
    for acting in [
        SessionCreated,
        AgentConnected,
        Running,
        Degraded,
        Quarantined,
    ] {
        assert!(!after.contains(&acting), "{}", acting.name());
    }
    for phase in Phase::ALL {
        if phase.frozen() {
            for acting in [AgentConnected, Running, Degraded, Quarantined] {
                assert!(
                    !reachable(phase).contains(&acting),
                    "{} -> {}",
                    phase.name(),
                    acting.name()
                );
            }
        }
    }
    assert_eq!(
        Phase::ALL.iter().filter(|phase| phase.frozen()).count(),
        5,
        "STOPPING, RECONCILING, VERIFIED, DELIVERY_READY, FAILED"
    );
}

/** Every phase before VERIFIED can fail, and nothing after it can */
#[test]
fn failure_is_possible_until_verified() {
    for phase in Phase::ALL {
        let before = !matches!(phase, Verified | DeliveryReady | Failed);
        assert_eq!(phase.allows(Failed), before, "{}", phase.name());
    }
}

/** Verification is reached only through STOPPING and RECONCILING: every path from an acting phase
 * to VERIFIED passes the freeze */
#[test]
fn verification_requires_the_freeze() {
    for acting in [
        SessionCreated,
        AgentConnected,
        Running,
        Degraded,
        Quarantined,
    ] {
        assert!(!acting.allows(Reconciling) && !acting.allows(Verified));
        assert!(reachable(acting).contains(&Verified));
    }
    assert!(!reachable(Reconciling).contains(&Stopping));
}
