// Security metrics, computed on demand from the event store and the autonomy ledger at every
// execution level: tool call, hook, task, session, agent, and repository. Every metric has a
// definition (type, unit, aggregation, security domain, collection method, versioned formula,
// purpose), and every value carries how it is known: a metric without data is UNOBSERVED (never
// zero), a metric Crane cannot collect is UNSUPPORTED, and parsed rather than measured facts are
// INFERRED. Positive outcomes are prevented prohibited actions and verified compliant actions, not
// merely successful tool calls; attempts, prevented actions, violations, and confirmed bypasses are
// counted separately. No metric grants authority or overrides a decision.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{json, Value};

use super::events::Event;
use super::ledger::Entry;
use super::model::{Assessment, Decision, Domain, Enforcement, Telemetry};

/** Version of the metric formulas */
pub(crate) const FORMULA_VERSION: u32 = 1;

/** A metric definition
 * Fields
    - name: &'static str - metric name
    - kind: &'static str - counter, rate, distribution, gauge, list, or assessment
    - unit: &'static str - unit
    - aggregation: &'static str - how values combine across scopes
    - domain: Domain - security domain
    - method: &'static str - collection method
    - formula: &'static str - how it is computed
    - purpose: &'static str - the security question it answers
*/
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Definition {
    pub(crate) name: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) unit: &'static str,
    pub(crate) aggregation: &'static str,
    pub(crate) domain: Domain,
    pub(crate) method: &'static str,
    pub(crate) formula: &'static str,
    pub(crate) purpose: &'static str,
}

/** Build a definition
 * Input
    - name, kind, unit, aggregation: &'static str - identity and shape
    - domain: Domain - security domain
    - method, formula, purpose: &'static str - semantics
 * Output
    - Definition
*/
#[allow(clippy::too_many_arguments)]
const fn define(
    name: &'static str,
    kind: &'static str,
    unit: &'static str,
    aggregation: &'static str,
    domain: Domain,
    method: &'static str,
    formula: &'static str,
    purpose: &'static str,
) -> Definition {
    Definition {
        name,
        kind,
        unit,
        aggregation,
        domain,
        method,
        formula,
        purpose,
    }
}

/** Every metric Crane computes */
pub(crate) const DEFINITIONS: &[Definition] = &[
    define(
        "tool_calls_decided",
        "counter",
        "count",
        "sum",
        Domain::Authorization,
        "pre-tool hook decisions",
        "count(tool_call.decided)",
        "how many agent actions passed the authorization boundary",
    ),
    define(
        "actions_allowed",
        "counter",
        "count",
        "sum",
        Domain::Authorization,
        "pre-tool hook decisions",
        "count(decision = ALLOW)",
        "actions the agent was authorized to take",
    ),
    define(
        "actions_denied",
        "counter",
        "count",
        "sum",
        Domain::Authorization,
        "pre-tool hook decisions",
        "count(decision in DENY, QUARANTINE, ERROR)",
        "prohibited actions stopped before they ran",
    ),
    define(
        "actions_requiring_approval",
        "counter",
        "count",
        "sum",
        Domain::RecoveryOversight,
        "pre-tool hook decisions",
        "count(decision = REQUIRE_APPROVAL)",
        "actions escalated to a human",
    ),
    define(
        "denial_rate",
        "rate",
        "ratio",
        "recompute from counters",
        Domain::Authorization,
        "pre-tool hook decisions",
        "actions_denied / tool_calls_decided",
        "how often the agent attempts what it may not do",
    ),
    define(
        "positive_security_outcomes",
        "counter",
        "count",
        "sum",
        Domain::PolicyIntegrity,
        "decisions and post-action verification",
        "count(enforcement = PREVENTED) + count(tool_call.verified with COMPLIANT)",
        "prohibited actions prevented plus actions verified compliant",
    ),
    define(
        "negative_security_outcomes",
        "counter",
        "count",
        "sum",
        Domain::PolicyIntegrity,
        "decisions and post-action verification",
        "bypass_attempts + violations_after_effect + bypasses_confirmed",
        "attempted and actual breaches of policy",
    ),
    define(
        "bypass_attempts",
        "counter",
        "count",
        "sum",
        Domain::PolicyIntegrity,
        "pre-tool hook decisions",
        "count(bypass_attempt = 1)",
        "attempts to change Crane metadata, markers, trust material, or run governance commands",
    ),
    define(
        "bypasses_confirmed",
        "counter",
        "count",
        "sum",
        Domain::PolicyIntegrity,
        "post-action verification",
        "count(enforcement = BYPASS_CONFIRMED)",
        "protected code changed without passing the write check",
    ),
    define(
        "violations_after_effect",
        "counter",
        "count",
        "sum",
        Domain::PolicyIntegrity,
        "post-action verification",
        "count(enforcement = DETECTED_AFTER_EFFECT)",
        "violations found only after the change happened",
    ),
    define(
        "prevented_actions",
        "counter",
        "count",
        "sum",
        Domain::Authorization,
        "pre-tool hook decisions",
        "count(enforcement = PREVENTED)",
        "enforcement that worked before the effect",
    ),
    define(
        "coverage_gaps",
        "counter",
        "count",
        "sum",
        Domain::AuditCompliance,
        "hook correlation",
        "count(enforcement = COVERAGE_GAP)",
        "actions that were not decided or whose effect was not reported",
    ),
    define(
        "evidence_completeness",
        "rate",
        "ratio",
        "recompute from counters",
        Domain::AuditCompliance,
        "hook correlation",
        "tool calls with a pre decision and a post verification / allowed tool calls",
        "share of actions with complete before-and-after evidence",
    ),
    define(
        "hook_latency_ms",
        "distribution",
        "ms",
        "p50, p95, max",
        Domain::AuditCompliance,
        "hook process wall clock",
        "percentiles of hook_latency_ms",
        "cost of enforcement and timeout risk (a timed-out hook fails open in some hosts)",
    ),
    define(
        "tool_duration_ms",
        "distribution",
        "ms",
        "p50, p95, max",
        Domain::ResourceAbuse,
        "pre-tool to post-tool interval",
        "percentiles of tool_duration_ms",
        "long-running or hanging actions",
    ),
    define(
        "hook_failures",
        "counter",
        "count",
        "sum",
        Domain::AuditCompliance,
        "hook runtime",
        "count(hook.failed)",
        "fail-closed outcomes caused by Crane errors",
    ),
    define(
        "missing_decisions",
        "counter",
        "count",
        "sum",
        Domain::AuditCompliance,
        "hook correlation",
        "count(hook.missing_decision)",
        "actions that ran without a recorded authorization",
    ),
    define(
        "hook_invocations",
        "list",
        "count by event",
        "sum per key",
        Domain::AuditCompliance,
        "hook runtime",
        "count by hook event",
        "which lifecycle events Crane observed",
    ),
    define(
        "resources_touched",
        "list",
        "resources",
        "union",
        Domain::DataAccessPrivacy,
        "tool arguments",
        "distinct resource ids by access",
        "which files, commands, and tools the agent used",
    ),
    define(
        "protected_resource_attempts",
        "counter",
        "count",
        "sum",
        Domain::PolicyIntegrity,
        "pre-tool hook decisions",
        "count(tool calls touching selections)",
        "how often the agent works on protected or targeted code",
    ),
    define(
        "network_destinations",
        "list",
        "hosts",
        "union",
        Domain::NetworkExfiltration,
        "parsed from commands and tool arguments",
        "distinct inferred destinations",
        "possible egress (inferred, not observed)",
    ),
    define(
        "network_bytes_sent",
        "gauge",
        "bytes",
        "sum",
        Domain::NetworkExfiltration,
        "not instrumented",
        "unsupported",
        "data volume leaving the machine",
    ),
    define(
        "secrets_redacted",
        "counter",
        "count",
        "sum",
        Domain::SecretsCredentials,
        "redaction before persistence",
        "sum(secrets_redacted)",
        "credentials appearing in agent actions",
    ),
    define(
        "suspicious_prompts",
        "counter",
        "count",
        "sum",
        Domain::PromptInjection,
        "prompt phrase screening",
        "count(prompt.suspicious)",
        "instruction-integrity attacks (heuristic)",
    ),
    define(
        "credits_granted",
        "gauge",
        "credits",
        "sum",
        Domain::ResourceAbuse,
        "autonomy ledger",
        "ledger balance.granted",
        "authority issued",
    ),
    define(
        "credits_consumed",
        "gauge",
        "credits",
        "sum",
        Domain::ResourceAbuse,
        "autonomy ledger",
        "ledger balance.consumed",
        "authority used by executed actions",
    ),
    define(
        "credits_reserved",
        "gauge",
        "credits",
        "sum",
        Domain::ResourceAbuse,
        "autonomy ledger",
        "ledger balance.reserved",
        "authority held by unsettled actions",
    ),
    define(
        "credits_available",
        "gauge",
        "credits",
        "sum",
        Domain::ResourceAbuse,
        "autonomy ledger",
        "ledger balance.available",
        "authority remaining",
    ),
    define(
        "credits_expired",
        "gauge",
        "credits",
        "sum",
        Domain::ResourceAbuse,
        "autonomy ledger",
        "ledger balance.expired",
        "authority that lapsed unused",
    ),
    define(
        "credits_revoked",
        "gauge",
        "credits",
        "sum",
        Domain::RecoveryOversight,
        "autonomy ledger",
        "ledger balance.revoked",
        "authority removed after violations",
    ),
    define(
        "credit_burn_rate",
        "rate",
        "credits per hour",
        "recompute",
        Domain::ResourceAbuse,
        "autonomy ledger",
        "credits_consumed / active hours",
        "pace of authority consumption",
    ),
    define(
        "safety_state",
        "assessment",
        "state",
        "worst",
        Domain::RecoveryOversight,
        "session record",
        "ACTIVE, DEGRADED, or QUARANTINED",
        "whether the agent is trusted to continue",
    ),
    define(
        "identity_provenance",
        "assessment",
        "provenance",
        "list",
        Domain::IdentityProvenance,
        "session record",
        "provider, environment, or generated",
        "whether identities came from the provider or were generated by Crane",
    ),
    define(
        "identity_conflicts",
        "counter",
        "count",
        "sum",
        Domain::IdentityProvenance,
        "session record",
        "count(conflicts)",
        "inconsistent identity reports",
    ),
    define(
        "models_seen",
        "list",
        "models",
        "union",
        Domain::IdentityProvenance,
        "provider payloads",
        "distinct reported models",
        "which models acted",
    ),
    define(
        "policy_versions_seen",
        "list",
        "generations",
        "union",
        Domain::PolicyIntegrity,
        "event bindings",
        "distinct registry generations",
        "policy drift during the work",
    ),
    define(
        "tasks_verified",
        "counter",
        "count",
        "sum",
        Domain::AuditCompliance,
        "stop reconciliation",
        "count(task.verified)",
        "tasks verified compliant at completion",
    ),
    define(
        "tasks_failed_verification",
        "counter",
        "count",
        "sum",
        Domain::AuditCompliance,
        "stop reconciliation",
        "count(task.verification_failed)",
        "tasks that did not satisfy their policies",
    ),
    define(
        "attestations_signed",
        "counter",
        "count",
        "sum",
        Domain::AuditCompliance,
        "session end",
        "count(session.attested signed)",
        "sessions with signed evidence",
    ),
    define(
        "cpu_ms",
        "gauge",
        "ms",
        "sum",
        Domain::ResourceAbuse,
        "not instrumented",
        "unsupported",
        "compute consumed by agent processes",
    ),
    define(
        "llm_tokens",
        "gauge",
        "tokens",
        "sum",
        Domain::ResourceAbuse,
        "not reported by hook payloads",
        "unsupported",
        "model usage (kept separate from autonomy credits)",
    ),
];

/** A computed metric
 * Fields
    - name: &'static str - metric name
    - value: Value - number, list, map, or null
    - unit: &'static str - unit
    - status: Telemetry - how the value is known
    - domain: Domain - security domain
    - formula_version: u32 - FORMULA_VERSION
*/
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Metric {
    pub(crate) name: &'static str,
    pub(crate) value: Value,
    pub(crate) unit: &'static str,
    pub(crate) status: Telemetry,
    pub(crate) domain: Domain,
    pub(crate) formula_version: u32,
}

/** Compute percentiles of values
 * Input
    - values: &mut Vec<f64> - values
 * Output
    - Value {p50, p95, max, count} or null when empty
*/
fn percentiles(values: &mut [f64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let at = |fraction: f64| values[((values.len() - 1) as f64 * fraction).round() as usize];
    json!({"p50": at(0.5), "p95": at(0.95), "max": values[values.len() - 1], "count": values.len()})
}

/** Compute every metric for a set of events and ledger entries
 * Input
    - events: &[&Event] - events in scope
    - ledger: &[&Entry] - ledger entries in scope
    - sessions: &[Value] - session records in scope (as JSON)
 * Output
    - Vec<Metric> in definition order
*/
pub(crate) fn compute(events: &[&Event], ledger: &[&Entry], sessions: &[Value]) -> Vec<Metric> {
    let of_type = |name: &str| {
        events
            .iter()
            .filter(|event| event.event_type == name)
            .count()
    };
    let decided = events
        .iter()
        .filter(|event| event.event_type == "tool_call.decided")
        .collect::<Vec<_>>();
    let count_decision = |decisions: &[Decision]| {
        decided
            .iter()
            .filter(|event| {
                event
                    .decision
                    .is_some_and(|decision| decisions.contains(&decision))
            })
            .count()
    };
    let allowed = count_decision(&[Decision::Allow]);
    let denied = count_decision(&[Decision::Deny, Decision::Quarantine, Decision::Error]);
    let approval = count_decision(&[Decision::RequireApproval]);
    let enforcement = |kind: Enforcement| {
        events
            .iter()
            .filter(|event| event.enforcement == kind)
            .count()
    };
    let bypass_attempts = decided
        .iter()
        .filter(|event| event.measurement("bypass_attempt") == Some(1.0))
        .count();
    let verified = events
        .iter()
        .filter(|event| event.event_type == "tool_call.verified")
        .collect::<Vec<_>>();
    let compliant = verified
        .iter()
        .filter(|event| event.assessment == Some(Assessment::Compliant))
        .count();
    let allowed_calls = decided
        .iter()
        .filter(|event| event.decision == Some(Decision::Allow))
        .filter_map(|event| event.scope.tool_call_id.clone())
        .collect::<BTreeSet<_>>();
    let verified_calls = verified
        .iter()
        .filter_map(|event| event.scope.tool_call_id.clone())
        .collect::<BTreeSet<_>>();
    let mut latency = events
        .iter()
        .filter_map(|event| event.measurement("hook_latency_ms"))
        .collect::<Vec<_>>();
    let mut duration = events
        .iter()
        .filter_map(|event| event.measurement("tool_duration_ms"))
        .collect::<Vec<_>>();
    let mut invocations: BTreeMap<String, u64> = BTreeMap::new();
    for event in events {
        if let Some(hook) = event.payload.get("hook_event").and_then(Value::as_str) {
            if event.scope.hook_event_id.is_some()
                && matches!(
                    event.event_type.as_str(),
                    "tool_call.decided"
                        | "tool_call.verified"
                        | "session.started"
                        | "task.started"
                        | "task.verified"
                        | "task.verification_failed"
                        | "session.attested"
                        | "hook.failed"
                )
            {
                *invocations.entry(hook.to_string()).or_default() += 1;
            }
        }
    }
    let mut resources: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut network = BTreeSet::new();
    for event in &decided {
        for resource in &event.resources {
            resources
                .entry(resource.access.clone())
                .or_default()
                .insert(format!("{}:{}", resource.kind, resource.id));
            if resource.access == "network" {
                network.insert(resource.id.clone());
            }
        }
    }
    let protected = decided
        .iter()
        .filter(|event| !event.bindings.selections.is_empty())
        .count();
    let secrets = events
        .iter()
        .filter_map(|event| event.measurement("secrets_redacted"))
        .sum::<f64>();
    let generations = events
        .iter()
        .map(|event| event.bindings.registry_generation)
        .collect::<BTreeSet<_>>();
    let mut balance = super::ledger::Balance::default();
    for entry in ledger {
        balance = balance.apply(entry.kind, entry.requested).0;
    }
    let first = ledger.iter().map(|entry| entry.at).min();
    let last = ledger.iter().map(|entry| entry.at).max();
    let hours = match (first, last) {
        (Some(first), Some(last)) if last > first => Some((last - first) as f64 / 3_600_000.0),
        _ => None,
    };
    let safety = sessions
        .iter()
        .filter_map(|session| session["safety"].as_str())
        .max_by_key(|state| match *state {
            "QUARANTINED" => 2,
            "DEGRADED" => 1,
            _ => 0,
        });
    let provenance = sessions
        .iter()
        .filter_map(|session| session["provenance"].as_str())
        .collect::<BTreeSet<_>>();
    let conflicts = sessions
        .iter()
        .map(|session| session["conflicts"].as_array().map_or(0, Vec::len))
        .sum::<usize>();
    let models = sessions
        .iter()
        .flat_map(|session| {
            session["models_seen"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|model| model.as_str().map(String::from))
        .collect::<BTreeSet<_>>();
    let signed = events
        .iter()
        .filter(|event| {
            event.event_type == "session.attested" && event.payload["attestation"]["signed"] == true
        })
        .count();
    let has_events = !events.is_empty();
    let has_decisions = !decided.is_empty();
    let has_ledger = !ledger.is_empty();
    let mut values: BTreeMap<&str, (Value, Telemetry)> = BTreeMap::new();
    let mut put = |name: &'static str, value: Value, status: Telemetry| {
        values.insert(name, (value, status));
    };
    let counted = |present: bool| {
        if present {
            Telemetry::Observed
        } else {
            Telemetry::Unobserved
        }
    };
    put(
        "tool_calls_decided",
        json!(decided.len()),
        counted(has_events),
    );
    put("actions_allowed", json!(allowed), counted(has_decisions));
    put("actions_denied", json!(denied), counted(has_decisions));
    put(
        "actions_requiring_approval",
        json!(approval),
        counted(has_decisions),
    );
    put(
        "denial_rate",
        if has_decisions {
            json!(denied as f64 / decided.len() as f64)
        } else {
            Value::Null
        },
        counted(has_decisions),
    );
    put(
        "positive_security_outcomes",
        json!(enforcement(Enforcement::Prevented) + compliant),
        counted(has_events),
    );
    put(
        "negative_security_outcomes",
        json!(
            bypass_attempts
                + enforcement(Enforcement::DetectedAfterEffect)
                + enforcement(Enforcement::BypassConfirmed)
        ),
        counted(has_events),
    );
    put(
        "bypass_attempts",
        json!(bypass_attempts),
        counted(has_decisions),
    );
    put(
        "bypasses_confirmed",
        json!(enforcement(Enforcement::BypassConfirmed)),
        counted(has_events),
    );
    put(
        "violations_after_effect",
        json!(enforcement(Enforcement::DetectedAfterEffect)),
        counted(has_events),
    );
    put(
        "prevented_actions",
        json!(enforcement(Enforcement::Prevented)),
        counted(has_decisions),
    );
    put(
        "coverage_gaps",
        json!(enforcement(Enforcement::CoverageGap)),
        counted(has_events),
    );
    put(
        "evidence_completeness",
        if allowed_calls.is_empty() {
            Value::Null
        } else {
            json!(
                allowed_calls.intersection(&verified_calls).count() as f64
                    / allowed_calls.len() as f64
            )
        },
        counted(!allowed_calls.is_empty()),
    );
    put(
        "hook_latency_ms",
        percentiles(&mut latency),
        counted(!latency.is_empty()),
    );
    put(
        "tool_duration_ms",
        percentiles(&mut duration),
        counted(!duration.is_empty()),
    );
    put(
        "hook_failures",
        json!(of_type("hook.failed")),
        counted(has_events),
    );
    put(
        "missing_decisions",
        json!(of_type("hook.missing_decision")),
        counted(has_events),
    );
    put("hook_invocations", json!(invocations), counted(has_events));
    put(
        "resources_touched",
        json!(resources),
        counted(has_decisions),
    );
    put(
        "protected_resource_attempts",
        json!(protected),
        counted(has_decisions),
    );
    put(
        "network_destinations",
        json!(network),
        if has_decisions {
            Telemetry::Inferred
        } else {
            Telemetry::Unobserved
        },
    );
    put("network_bytes_sent", Value::Null, Telemetry::Unsupported);
    put("secrets_redacted", json!(secrets), counted(has_events));
    put(
        "suspicious_prompts",
        json!(of_type("prompt.suspicious")),
        counted(has_events),
    );
    put(
        "credits_granted",
        json!(balance.granted),
        counted(has_ledger),
    );
    put(
        "credits_consumed",
        json!(balance.consumed),
        counted(has_ledger),
    );
    put(
        "credits_reserved",
        json!(balance.reserved),
        counted(has_ledger),
    );
    put(
        "credits_available",
        json!(balance.available),
        counted(has_ledger),
    );
    put(
        "credits_expired",
        json!(balance.expired),
        counted(has_ledger),
    );
    put(
        "credits_revoked",
        json!(balance.revoked),
        counted(has_ledger),
    );
    put(
        "credit_burn_rate",
        hours.map_or(Value::Null, |hours| json!(balance.consumed as f64 / hours)),
        counted(hours.is_some()),
    );
    put(
        "safety_state",
        safety.map_or(Value::Null, |state| json!(state)),
        counted(safety.is_some()),
    );
    put(
        "identity_provenance",
        json!(provenance),
        counted(!sessions.is_empty()),
    );
    put(
        "identity_conflicts",
        json!(conflicts),
        counted(!sessions.is_empty()),
    );
    put("models_seen", json!(models), counted(!models.is_empty()));
    put(
        "policy_versions_seen",
        json!(generations),
        counted(has_events),
    );
    put(
        "tasks_verified",
        json!(of_type("task.verified")),
        counted(has_events),
    );
    put(
        "tasks_failed_verification",
        json!(of_type("task.verification_failed")),
        counted(has_events),
    );
    put("attestations_signed", json!(signed), counted(has_events));
    put("cpu_ms", Value::Null, Telemetry::Unsupported);
    put("llm_tokens", Value::Null, Telemetry::Unsupported);
    DEFINITIONS
        .iter()
        .map(|definition| {
            let (value, status) = values
                .remove(definition.name)
                .unwrap_or((Value::Null, Telemetry::Unsupported));
            let value = if status == Telemetry::Unobserved {
                Value::Null
            } else {
                value
            };
            Metric {
                name: definition.name,
                value,
                unit: definition.unit,
                status,
                domain: definition.domain,
                formula_version: FORMULA_VERSION,
            }
        })
        .collect()
}

/** Select the ledger entries of some sessions
 * Input
    - entries: &'a [Entry] - every entry
    - sessions: &BTreeSet<String> - canonical sessions
 * Output
    - Vec<&'a Entry>
*/
pub(crate) fn ledger_of<'a>(entries: &'a [Entry], sessions: &BTreeSet<String>) -> Vec<&'a Entry> {
    entries
        .iter()
        .filter(|entry| sessions.contains(&entry.session))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::events::Measurement;

    /** Check that missing data is null and unobserved, and that counts separate outcomes
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn metrics_distinguish_missing_from_zero() {
        let empty = compute(&[], &[], &[]);
        let denied = empty
            .iter()
            .find(|metric| metric.name == "actions_denied")
            .unwrap();
        assert!(denied.value.is_null());
        assert_eq!(denied.status, Telemetry::Unobserved);
        assert_eq!(
            empty
                .iter()
                .find(|metric| metric.name == "network_bytes_sent")
                .unwrap()
                .status,
            Telemetry::Unsupported
        );
        let mut deny = Event::new("tool_call.decided", Domain::PolicyIntegrity, "r", "1");
        deny.decision = Some(Decision::Deny);
        deny.enforcement = Enforcement::Prevented;
        deny.measurements.push(Measurement::observed(
            "bypass_attempt",
            1.0,
            "count",
            "test",
        ));
        let mut allow = Event::new("tool_call.decided", Domain::Authorization, "r", "2");
        allow.decision = Some(Decision::Allow);
        let metrics = compute(&[&deny, &allow], &[], &[]);
        let value = |name: &str| {
            metrics
                .iter()
                .find(|metric| metric.name == name)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(value("actions_denied"), json!(1));
        assert_eq!(value("bypass_attempts"), json!(1));
        assert_eq!(value("bypasses_confirmed"), json!(0));
        assert_eq!(value("denial_rate"), json!(0.5));
        assert_eq!(DEFINITIONS.len(), metrics.len());
    }
}
