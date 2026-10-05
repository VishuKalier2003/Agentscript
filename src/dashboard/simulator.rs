// The policy simulator: evaluate a draft policy in shadow mode against everything agents actually
// did (the session journals), without enforcing anything. It reports which historical actions the
// policy would have denied or flagged, which sessions a target would have failed, how its targets
// resolve in the repository today, and a false-positive analysis: a blocked action from a session
// that passed (or merged) was probably legitimate work; one from a session that failed, was
// quarantined, or was rejected in review probably deserved blocking.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::model::{Policy, Rule, Scope};
use crate::session::{session_ids, ContractSession};
use crate::zones::model::SafetyState;

/** One action from a session journal, reduced to what the simulator needs
 * Fields
    - seq: u64 - journal sequence
    - event: String - pre_tool_use, permission_request, or post_tool_use
    - tool: String - tool name
    - operation: String - read, write, execute, or other
    - digest: String - arguments digest (pairs authorization with execution)
    - decision: Option<String> - decision of an authorization
    - files: Vec<String> - files touched
    - symbols: Vec<String> - qualified names of symbols changed
    - violations: Vec<String> - violations found after it ran
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Action {
    pub(crate) seq: u64,
    pub(crate) event: String,
    pub(crate) tool: String,
    pub(crate) operation: String,
    pub(crate) digest: String,
    pub(crate) decision: Option<String>,
    pub(crate) files: Vec<String>,
    pub(crate) symbols: Vec<String>,
    pub(crate) violations: Vec<String>,
}

/** One session's history
 * Fields
    - id: String - session id
    - outcome: String - pass, fail, cancelled, merged, or open
    - troubled: bool - quarantined, or rejected or sent back in review
    - actions: Vec<Action> - its mutating actions in order
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct History {
    pub(crate) id: String,
    pub(crate) outcome: String,
    pub(crate) troubled: bool,
    pub(crate) actions: Vec<Action>,
}

/** Reduce a session journal to its history
 * Input
    - id: &str - session id
    - events: &[Value] - journal
    - quarantined: bool - the session ended quarantined
 * Output
    - History
*/
pub(crate) fn history(id: &str, events: &[Value], quarantined: bool) -> History {
    let strings = |value: &Value| {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect::<Vec<_>>()
    };
    let mut actions = Vec::new();
    for event in events {
        let name = event["event"].as_str().unwrap_or_default();
        if !matches!(
            name,
            "pre_tool_use" | "permission_request" | "post_tool_use"
        ) || event["operation"] == "read"
        {
            continue;
        }
        let (files, symbols) = if name == "post_tool_use" {
            let effect = &event["effect"]["files"];
            let mut files = Vec::new();
            for key in ["added", "modified", "deleted"] {
                files.extend(strings(&effect[key]));
            }
            files.extend(
                effect["renamed"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|pair| pair["to"].as_str())
                    .map(String::from),
            );
            let symbols = event["effect"]["symbols"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|symbol| symbol["symbol"].as_str())
                .filter_map(|symbol| {
                    symbol.split_once('#').map(|(_, qualified)| {
                        qualified.split('~').next().unwrap_or(qualified).to_string()
                    })
                })
                .collect();
            (files, symbols)
        } else {
            let resources = strings(&event["resources"]);
            (
                resources
                    .iter()
                    .filter_map(|resource| resource.strip_prefix("file:"))
                    .map(String::from)
                    .collect(),
                resources
                    .iter()
                    .filter_map(|resource| resource.strip_prefix("symbol:"))
                    .filter_map(|rest| {
                        rest.split_once(':')
                            .map(|(_, qualified)| qualified.to_string())
                    })
                    .collect(),
            )
        };
        actions.push(Action {
            seq: event["seq"].as_u64().unwrap_or(0),
            event: name.to_string(),
            tool: event["tool"].as_str().unwrap_or_default().to_string(),
            operation: event["operation"].as_str().unwrap_or_default().to_string(),
            digest: event["arguments_digest"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            decision: event["decision"].as_str().map(String::from),
            files,
            symbols,
            violations: strings(&event["effect"]["violations"]),
        });
    }
    let finalized = events
        .iter()
        .rev()
        .find(|event| event["event"] == "session_finalized");
    let merged = events
        .iter()
        .any(|event| event["event"] == "delivery_merged");
    let outcome = if merged {
        "merged"
    } else if let Some(event) = finalized {
        if event["final_status"] == "PASS" {
            "pass"
        } else {
            "fail"
        }
    } else if events
        .iter()
        .any(|event| event["event"] == "session_cancelled")
    {
        "cancelled"
    } else {
        "open"
    };
    let reviewed_badly = events.iter().any(|event| {
        matches!(
            event["event"].as_str(),
            Some("delivery_rejection" | "delivery_changes_requested")
        )
    });
    History {
        id: id.to_string(),
        outcome: outcome.to_string(),
        troubled: quarantined || reviewed_badly,
        actions,
    }
}

/** Read every session's history from its journal
 * Input
    - None
 * Output
    - Result<Vec<History>, String>
*/
pub(crate) fn histories() -> Result<Vec<History>, String> {
    let mut out = Vec::new();
    for id in session_ids()? {
        if let Ok(Some(session)) = ContractSession::load(&id) {
            out.push(history(
                &id,
                &session.events(),
                session.activity().state.safety == SafetyState::Quarantined,
            ));
        }
    }
    Ok(out)
}

/** Check whether a qualified symbol name is a rule's target
 * Input
    - qualified: &str - symbol
    - target: &str - rule target
 * Output
    - bool
*/
fn names(qualified: &str, target: &str) -> bool {
    qualified == target || qualified.ends_with(&format!(".{target}"))
}

/** The folder of a path
 * Input
    - path: &str - repository-relative path
 * Output
    - &str
*/
fn folder(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(folder, _)| folder)
}

/** Check whether an action touches what a rule covers: the target itself at block and flow scope
 * (flow is approximated by the target), its file, its folder, or anything at all
 * Input
    - action: &Action - action
    - target: &str - rule target
    - scope: Scope - rule scope
    - located: &BTreeSet<String> - files the target resolves to today
 * Output
    - bool
*/
fn touches(action: &Action, target: &str, scope: Scope, located: &BTreeSet<String>) -> bool {
    let by_symbol = action.symbols.iter().any(|symbol| names(symbol, target));
    match scope {
        Scope::Block | Scope::Flow => by_symbol,
        Scope::File => by_symbol || action.files.iter().any(|file| located.contains(file)),
        Scope::Folder => {
            by_symbol
                || action
                    .files
                    .iter()
                    .any(|file| located.iter().any(|home| folder(home) == folder(file)))
        }
        Scope::All => !action.files.is_empty() || !action.symbols.is_empty(),
    }
}

/** Classify a blocked historical action by how its session turned out
 * Input
    - session: &History - session
    - action: &Action - action
 * Output
    - &'static str likely_true_positive, likely_false_positive, or undetermined
*/
fn verdict(session: &History, action: &Action) -> &'static str {
    if !action.violations.is_empty() || session.troubled || session.outcome == "fail" {
        "likely_true_positive"
    } else if matches!(session.outcome.as_str(), "pass" | "merged") {
        "likely_false_positive"
    } else {
        "undetermined"
    }
}

/** Evaluate a policy in shadow mode against session histories and today's repository
 * Input
    - policy: &Policy - draft policy
    - sessions: &[History] - session histories
    - entities: &[(String, String, String)] - (qualified name, item kind, file) of every
      targetable non-test symbol in the repository today
 * Output
    - Value {policy, rules, impact, false_positives, summary}
*/
pub(crate) fn simulate(
    policy: &Policy,
    sessions: &[History],
    entities: &[(String, String, String)],
) -> Value {
    let mut rules = Vec::new();
    let mut impact = Vec::new();
    let (mut true_positive, mut false_positive, mut undetermined) = (0, 0, 0);
    for rule in &policy.rules {
        let (keyword, kind, target, scope) = match rule {
            Rule::Preserve {
                kind,
                target,
                scope,
            } => ("preserve", kind, target, *scope),
            Rule::Target {
                kind,
                target,
                scope,
                ..
            } => ("target", kind, target, *scope),
        };
        let matches = entities
            .iter()
            .filter(|(qualified, item, _)| names(qualified, target) && item == kind.noun())
            .collect::<Vec<_>>();
        let located = matches
            .iter()
            .map(|(_, _, file)| file.clone())
            .collect::<BTreeSet<_>>();
        let resolution = match matches.len() {
            0 => "missing",
            1 => "resolved",
            _ => "ambiguous",
        };
        let mut affected = BTreeSet::new();
        let mut actions = 0;
        let mut unmet = Vec::new();
        for session in sessions {
            let authorized = session
                .actions
                .iter()
                .filter(|action| action.event != "post_tool_use")
                .map(|action| action.digest.clone())
                .collect::<BTreeSet<_>>();
            let touching = session
                .actions
                .iter()
                .filter(|action| touches(action, target, scope, &located))
                .collect::<Vec<_>>();
            if keyword == "target" {
                // A target changes the outcome of every session that never changed it
                if touching.is_empty()
                    && session
                        .actions
                        .iter()
                        .any(|action| action.event == "post_tool_use")
                {
                    unmet.push(session.id.clone());
                }
                continue;
            }
            for action in touching {
                // An execution already judged at authorization is not counted twice
                if action.event == "post_tool_use" && authorized.contains(&action.digest) {
                    continue;
                }
                let shadow = match (action.event.as_str(), action.decision.as_deref()) {
                    (_, Some("deny")) => "already_denied",
                    ("post_tool_use", _) => "would_flag",
                    _ => "would_deny",
                };
                let class = if shadow == "already_denied" {
                    "unchanged"
                } else {
                    verdict(session, action)
                };
                match class {
                    "likely_true_positive" => true_positive += 1,
                    "likely_false_positive" => false_positive += 1,
                    "undetermined" => undetermined += 1,
                    _ => {}
                }
                if shadow != "already_denied" {
                    actions += 1;
                    affected.insert(session.id.clone());
                }
                impact.push(json!({
                    "session": session.id,
                    "seq": action.seq,
                    "event": action.event,
                    "tool": action.tool,
                    "operation": action.operation,
                    "historical_decision": action.decision.clone().unwrap_or_else(|| "executed".into()),
                    "shadow_decision": shadow,
                    "rule": format!("{keyword} {} {target}", kind.flag()),
                    "session_outcome": session.outcome,
                    "classification": class,
                }));
            }
        }
        rules.push(json!({
            "rule": format!("{keyword} {} {target}", kind.flag()),
            "scope": scope.name(),
            "scope_approximate": scope == Scope::Flow,
            "resolution": resolution,
            "resolves_to": located,
            "actions_changed": actions,
            "sessions_affected": affected,
            "sessions_left_unmet": unmet,
        }));
    }
    let judged = true_positive + false_positive;
    json!({
        "mode": "shadow",
        "enforced": false,
        "policy": {"name": policy.name, "checkpoint": policy.checkpoint},
        "sessions_evaluated": sessions.len(),
        "actions_evaluated": sessions.iter().map(|session| session.actions.len()).sum::<usize>(),
        "rules": rules,
        "impact": impact,
        "false_positive_analysis": {
            "likely_true_positives": true_positive,
            "likely_false_positives": false_positive,
            "undetermined": undetermined,
            "false_positive_rate": (judged > 0).then(|| ((false_positive as f64 / judged as f64) * 1000.0).round() / 1000.0),
            "method": "a blocked action from a session that passed or merged is a likely false positive; from one that failed, was quarantined, was rejected in review, or whose action caused a violation, a likely true positive",
        },
    })
}
