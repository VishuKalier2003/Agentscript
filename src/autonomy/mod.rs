// Autonomy state machine: a session's autonomy (how much the agent may do on its own) and its
// safety state (whether it is behaving) are two separate dimensions with their own legal
// transitions. Autonomy changes only by a human promotion or demotion; safety degrades on
// violations, quarantines on critical ones, and recovers only when an explicit set of recovery
// conditions has been met. Transitions are a pure function of the organization's autonomy policy,
// the current state, the trigger, and who caused it, so replaying a session journal always gives
// the same state. Agents can never cause a transition.

pub(crate) mod manage;

#[cfg(test)] // Compile the module only when running tests, not in production builds
mod tests;

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use crate::util::{sha256, validate_identifier};
use crate::zones::model::{Autonomy, SafetyState};

/** File in .crane holding the organization's autonomy policy (defaults apply without it) */
pub(crate) const POLICY_FILE: &str = "autonomy.json";

/** Prefix of the error for any attempt by an agent to change its own state, and the violation
 * kind it is recorded as */
pub(crate) const SELF_ESCALATION: &str = "self_escalation";

/** Verification findings that say nothing about the agent's behaviour, so they never move safety:
 * an unmet target is work still to do, and the rest are setup problems */
pub(crate) const NOT_VIOLATIONS: &[&str] = &[
    "target_unchanged",
    "change_type_mismatch",
    "checkpoint_error",
    "verification_error",
    "malformed_policy",
];

/** Evidence that can satisfy a recovery from a degraded or quarantined safety state
 * Variants
    - VerifiedRepair - a full validation by Crane found no violation after the incident
    - HumanApproval - a human approved the session again (resume or autonomy approve)
    - NewSession - the state was inherited by a new session of the same agent and task
    - NewRiskBudget - a human granted a new autonomy budget (agent session extend)
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Condition {
    VerifiedRepair,
    HumanApproval,
    NewSession,
    NewRiskBudget,
}

impl Condition {
    /** Every condition, in order */
    pub(crate) const ALL: &'static [Condition] = &[
        Condition::VerifiedRepair,
        Condition::HumanApproval,
        Condition::NewSession,
        Condition::NewRiskBudget,
    ];

    /** Return the keyword used in the policy, the journal, and output
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::VerifiedRepair => "verified_repair",
            Self::HumanApproval => "human_approval",
            Self::NewSession => "new_session",
            Self::NewRiskBudget => "new_risk_budget",
        }
    }

    /** Parse a keyword
     * Input
        - value: &str - keyword
     * Output
        - Result<Condition, String>
        - Error naming the accepted keywords
    */
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        Self::ALL
            .iter()
            .copied()
            .find(|condition| condition.name() == value)
            .ok_or_else(|| {
                format!(
                    "invalid recovery condition '{value}'; expected {}",
                    Self::ALL
                        .iter()
                        .map(|condition| condition.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }

    /** Return who alone may supply this evidence: a human approves and grants budgets; Crane
     * verifies repairs and creates sessions
     * Input
        - None (uses self)
     * Output
        - Actor
    */
    pub(crate) fn source(self) -> Actor {
        match self {
            Self::HumanApproval | Self::NewRiskBudget => Actor::Human,
            Self::VerifiedRepair | Self::NewSession => Actor::Crane,
        }
    }
}

/** Who caused a trigger
 * Variants
    - Human - a person at the CLI, outside any agent environment
    - Crane - Crane itself, from what it verified or observed
    - Agent - the agent (a command run in an agent environment); never allowed to cause anything
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Actor {
    Human,
    Crane,
    Agent,
}

impl Actor {
    /** Return the keyword used in the journal
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Crane => "crane",
            Self::Agent => "agent",
        }
    }

    /** Parse a keyword
     * Input
        - value: &str - keyword
     * Output
        - Result<Actor, String>
    */
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "human" => Ok(Self::Human),
            "crane" => Ok(Self::Crane),
            "agent" => Ok(Self::Agent),
            other => Err(format!("invalid actor '{other}'")),
        }
    }
}

/** What can move the state machine
 * Variants
    - Promote(Autonomy) - raise autonomy (human only, safety active, within the policy maximum)
    - Demote(Autonomy) - lower autonomy (human only)
    - Violation(String) - a violation of the given kind was found (Crane or a human); critical
      kinds quarantine, others degrade
    - Quarantine(String) - quarantine for the given reason (Crane or a human)
    - Evidence(Condition) - recovery evidence, from the condition's only source; recovers once a
      configured set of conditions is complete
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Trigger {
    Promote(Autonomy),
    Demote(Autonomy),
    Violation(String),
    Quarantine(String),
    Evidence(Condition),
}

impl Trigger {
    /** Serialize for the journal
     * Input
        - None (uses self)
     * Output
        - Value such as {"trigger": "promote", "to": "delegated"}
    */
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Promote(to) => json!({"trigger": "promote", "to": to.name()}),
            Self::Demote(to) => json!({"trigger": "demote", "to": to.name()}),
            Self::Violation(kind) => json!({"trigger": "violation", "kind": kind}),
            Self::Quarantine(reason) => json!({"trigger": "quarantine", "reason": reason}),
            Self::Evidence(condition) => {
                json!({"trigger": "evidence", "condition": condition.name()})
            }
        }
    }

    /** Read back from a journal event
     * Input
        - value: &Value - event written with to_json's fields
     * Output
        - Result<Trigger, String>
    */
    pub(crate) fn from_json(value: &Value) -> Result<Self, String> {
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
        match value["trigger"].as_str() {
            Some("promote") => Ok(Self::Promote(Autonomy::parse(&text("to"))?)),
            Some("demote") => Ok(Self::Demote(Autonomy::parse(&text("to"))?)),
            Some("violation") => Ok(Self::Violation(text("kind"))),
            Some("quarantine") => Ok(Self::Quarantine(text("reason"))),
            Some("evidence") => Ok(Self::Evidence(Condition::parse(&text("condition"))?)),
            other => Err(format!("unknown autonomy trigger {other:?}")),
        }
    }
}

/** How a safety state recovers
 * Fields
    - to: SafetyState - the state recovered to
    - any_of: Vec<BTreeSet<Condition>> - condition sets; recovery happens once every condition of
      any one set has been met since the latest violation
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recovery {
    pub(crate) to: SafetyState,
    pub(crate) any_of: Vec<BTreeSet<Condition>>,
}

/** Explicit organizational authority over a Restricted zone, which no autonomy mode has by itself
 * Fields
    - zone: String - restricted zone id
    - autonomy: Autonomy - the most the grant allows there (still capped by the zone's safety state)
    - task: Option<String> - task the grant is limited to, None for every task
    - approved_by: String - who in the organization approved it
    - reason: String - why
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthorityGrant {
    pub(crate) zone: String,
    pub(crate) autonomy: Autonomy,
    pub(crate) task: Option<String>,
    pub(crate) approved_by: String,
    pub(crate) reason: String,
}

/** The organization's autonomy policy, bound to each session when it is created
 * Fields
    - max_autonomy: Autonomy - the highest autonomy any session may hold or be promoted to
    - single_step: bool - promotions go one level at a time
    - violations_to_quarantine: u64 - non-critical violations in one incident that quarantine a
      degraded session (0 never)
    - critical: BTreeSet<String> - violation kinds that quarantine from any state
    - degraded: Recovery - how a degraded session recovers
    - quarantined: Recovery - how a quarantined session recovers
    - grants: Vec<AuthorityGrant> - authority over restricted zones
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutonomyPolicy {
    pub(crate) max_autonomy: Autonomy,
    pub(crate) single_step: bool,
    pub(crate) violations_to_quarantine: u64,
    pub(crate) critical: BTreeSet<String>,
    pub(crate) degraded: Recovery,
    pub(crate) quarantined: Recovery,
    pub(crate) grants: Vec<AuthorityGrant>,
}

impl Default for AutonomyPolicy {
    /** Build the default policy: autonomy up to autonomous one step at a time; three violations
     * quarantine; unauthorized effects, exhausted budgets, and self-escalation are critical; a
     * degraded session recovers by a verified repair, a human approval, or a human budget refill;
     * a quarantined one only
     * with a human approval plus a verified repair, a new risk budget, or a new session
     * Input
        - None
     * Output
        - AutonomyPolicy
    */
    fn default() -> Self {
        let set = |conditions: &[Condition]| conditions.iter().copied().collect::<BTreeSet<_>>();
        Self {
            max_autonomy: Autonomy::Autonomous,
            single_step: true,
            violations_to_quarantine: 3,
            critical: ["unauthorized_effect", "budget_exhausted", SELF_ESCALATION]
                .iter()
                .map(|kind| kind.to_string())
                .collect(),
            degraded: Recovery {
                to: SafetyState::Active,
                any_of: vec![
                    set(&[Condition::VerifiedRepair]),
                    set(&[Condition::HumanApproval]),
                    set(&[Condition::NewRiskBudget]),
                ],
            },
            quarantined: Recovery {
                to: SafetyState::Active,
                any_of: vec![
                    set(&[Condition::HumanApproval, Condition::VerifiedRepair]),
                    set(&[Condition::HumanApproval, Condition::NewRiskBudget]),
                    set(&[Condition::HumanApproval, Condition::NewSession]),
                ],
            },
            grants: Vec::new(),
        }
    }
}

/** Serialize a recovery rule
 * Input
    - recovery: &Recovery - rule
 * Output
    - Value {to, any_of}
*/
fn recovery_json(recovery: &Recovery) -> Value {
    json!({
        "to": recovery.to.name(),
        "any_of": recovery.any_of.iter().map(|set| set.iter().map(|condition| condition.name()).collect::<Vec<_>>()).collect::<Vec<_>>(),
    })
}

/** Reject keys a policy object does not know, so a misspelt setting never silently takes its default
 * Input
    - value: &Value - object
    - known: &[&str] - accepted keys
    - what: &str - object name for errors
 * Output
    - Result<&Map<String, Value>, String>
*/
fn object<'a>(
    value: &'a Value,
    known: &[&str],
    what: &str,
) -> Result<&'a Map<String, Value>, String> {
    let map = value
        .as_object()
        .ok_or_else(|| format!("{what} must be a JSON object"))?;
    if let Some(unknown) = map.keys().find(|key| !known.contains(&key.as_str())) {
        return Err(format!(
            "{what} has unknown setting '{unknown}'; expected {}",
            known.join(", ")
        ));
    }
    Ok(map)
}

/** Read a recovery rule, requiring explicit conditions in every set and, for quarantine, a human
 * condition in every set (so no quarantine is ever lifted without a person)
 * Input
    - value: Option<&Value> - JSON, None for the default
    - default: &Recovery - default rule
    - quarantine: bool - whether this is the quarantine rule
 * Output
    - Result<Recovery, String>
*/
fn parse_recovery(
    value: Option<&Value>,
    default: &Recovery,
    quarantine: bool,
) -> Result<Recovery, String> {
    let what = if quarantine {
        "recovery.quarantined"
    } else {
        "recovery.degraded"
    };
    let Some(value) = value else {
        return Ok(default.clone());
    };
    let map = object(value, &["to", "any_of"], what)?;
    let to = match map.get("to") {
        Some(to) => SafetyState::parse(
            to.as_str()
                .ok_or_else(|| format!("{what}.to must be a string"))?,
        )?,
        None => default.to,
    };
    let allowed: &[SafetyState] = if quarantine {
        &[SafetyState::Active, SafetyState::Degraded]
    } else {
        &[SafetyState::Active]
    };
    if !allowed.contains(&to) {
        return Err(format!("{what}.to cannot be {}", to.name()));
    }
    let any_of = match map.get("any_of") {
        None => default.any_of.clone(),
        Some(sets) => {
            let mut parsed = Vec::new();
            for set in sets
                .as_array()
                .ok_or_else(|| format!("{what}.any_of must be a list of condition lists"))?
            {
                let set = set
                    .as_array()
                    .ok_or_else(|| format!("{what}.any_of must be a list of condition lists"))?
                    .iter()
                    .map(|condition| Condition::parse(condition.as_str().unwrap_or_default()))
                    .collect::<Result<BTreeSet<_>, _>>()?;
                parsed.push(set);
            }
            parsed
        }
    };
    if any_of.is_empty() || any_of.iter().any(BTreeSet::is_empty) {
        return Err(format!(
            "{what}.any_of needs at least one non-empty condition set; recovery must require explicit conditions"
        ));
    }
    if quarantine
        && any_of.iter().any(|set| {
            !set.iter()
                .any(|condition| condition.source() == Actor::Human)
        })
    {
        return Err(format!(
            "every {what}.any_of set must include human_approval or new_risk_budget; a quarantine is never lifted without a human"
        ));
    }
    Ok(Recovery { to, any_of })
}

impl AutonomyPolicy {
    /** Read a policy from .crane/autonomy.json text: every setting is optional and defaults as in
     * AutonomyPolicy::default; unknown settings and invalid values are errors
     * Input
        - value: &Value - parsed JSON
     * Output
        - Result<AutonomyPolicy, String>
    */
    pub(crate) fn from_json(value: &Value) -> Result<Self, String> {
        let default = Self::default();
        let map = object(
            value,
            &[
                "max_autonomy",
                "single_step_promotion",
                "violations_to_quarantine",
                "critical_violations",
                "recovery",
                "grants",
            ],
            "the autonomy policy",
        )?;
        let max_autonomy = match map.get("max_autonomy") {
            Some(value) => Autonomy::parse(value.as_str().ok_or("max_autonomy must be a string")?)?,
            None => default.max_autonomy,
        };
        let single_step = match map.get("single_step_promotion") {
            Some(value) => value
                .as_bool()
                .ok_or("single_step_promotion must be true or false")?,
            None => default.single_step,
        };
        let violations_to_quarantine = match map.get("violations_to_quarantine") {
            Some(value) => value
                .as_u64()
                .ok_or("violations_to_quarantine must be a whole number")?,
            None => default.violations_to_quarantine,
        };
        if violations_to_quarantine == 1 {
            return Err("violations_to_quarantine must be 0 (never) or at least 2; the first violation only degrades".into());
        }
        let critical = match map.get("critical_violations") {
            Some(value) => value
                .as_array()
                .ok_or("critical_violations must be a list of violation kinds")?
                .iter()
                .map(|kind| {
                    kind.as_str()
                        .filter(|kind| !kind.is_empty())
                        .map(String::from)
                        .ok_or_else(|| {
                            "critical_violations must be a list of violation kinds".to_string()
                        })
                })
                .collect::<Result<BTreeSet<_>, _>>()?,
            None => default.critical.clone(),
        };
        if !critical.contains(SELF_ESCALATION) {
            return Err(format!(
                "critical_violations must include {SELF_ESCALATION}: an agent trying to change its own autonomy is always quarantined"
            ));
        }
        let recovery = match map.get("recovery") {
            Some(value) => object(value, &["degraded", "quarantined"], "recovery")?.clone(),
            None => Map::new(),
        };
        let degraded = parse_recovery(recovery.get("degraded"), &default.degraded, false)?;
        let quarantined = parse_recovery(recovery.get("quarantined"), &default.quarantined, true)?;
        let mut grants = Vec::new();
        for (index, grant) in map
            .get("grants")
            .map(|grants| grants.as_array().ok_or("grants must be a list"))
            .transpose()?
            .into_iter()
            .flatten()
            .enumerate()
        {
            let what = format!("grants[{index}]");
            let entry = object(
                grant,
                &["zone", "autonomy", "task", "approved_by", "reason"],
                &what,
            )?;
            let text = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(String::from)
                    .ok_or_else(|| format!("{what}.{key} is required"))
            };
            let zone = text("zone")?;
            validate_identifier(&zone).map_err(|error| format!("{what}.zone: {error}"))?;
            let autonomy = Autonomy::parse(&text("autonomy")?)?;
            if autonomy == Autonomy::Observe {
                return Err(format!(
                    "{what}.autonomy must be above observe; a grant of observe grants nothing"
                ));
            }
            grants.push(AuthorityGrant {
                zone,
                autonomy,
                task: entry.get("task").and_then(Value::as_str).map(String::from),
                approved_by: text("approved_by")?,
                reason: text("reason")?,
            });
        }
        Ok(Self {
            max_autonomy,
            single_step,
            violations_to_quarantine,
            critical,
            degraded,
            quarantined,
            grants,
        })
    }

    /** Serialize the policy in the same form from_json reads
     * Input
        - None (uses self)
     * Output
        - Value
    */
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "max_autonomy": self.max_autonomy.name(),
            "single_step_promotion": self.single_step,
            "violations_to_quarantine": self.violations_to_quarantine,
            "critical_violations": self.critical,
            "recovery": {
                "degraded": recovery_json(&self.degraded),
                "quarantined": recovery_json(&self.quarantined),
            },
            "grants": self.grants.iter().map(|grant| json!({
                "zone": grant.zone,
                "autonomy": grant.autonomy.name(),
                "task": grant.task,
                "approved_by": grant.approved_by,
                "reason": grant.reason,
            })).collect::<Vec<_>>(),
        })
    }

    /** Return the policy's version, a digest of its canonical form
     * Input
        - None (uses self)
     * Output
        - String
    */
    pub(crate) fn version(&self) -> String {
        sha256(self.to_json().to_string().as_bytes())
    }

    /** Find the grant over a zone for a task: one limited to that task wins over a general one
     * Input
        - zone: &str - zone id
        - task: Option<&str> - the session's task
     * Output
        - Option<&AuthorityGrant>
    */
    pub(crate) fn grant_for(&self, zone: &str, task: Option<&str>) -> Option<&AuthorityGrant> {
        let mut matching = self.grants.iter().filter(|grant| {
            grant.zone == zone && grant.task.as_deref().is_none_or(|only| Some(only) == task)
        });
        let first = matching.next()?;
        Some(
            std::iter::once(first)
                .chain(matching)
                .find(|grant| grant.task.is_some())
                .unwrap_or(first),
        )
    }

    /** Return the recovery rule of a safety state
     * Input
        - safety: SafetyState - degraded or quarantined
     * Output
        - Option<&Recovery>, None for active
    */
    pub(crate) fn recovery(&self, safety: SafetyState) -> Option<&Recovery> {
        match safety {
            SafetyState::Active => None,
            SafetyState::Degraded => Some(&self.degraded),
            SafetyState::Quarantined => Some(&self.quarantined),
        }
    }

    /** List every legal transition under this policy, for status output and documentation
     * Input
        - None (uses self)
     * Output
        - Vec<Value> {dimension, from, to, trigger, by, requires}
    */
    pub(crate) fn transitions(&self) -> Vec<Value> {
        let mut rows = Vec::new();
        for pair in Autonomy::ALL.windows(2) {
            if pair[1] <= self.max_autonomy {
                rows.push(json!({"dimension": "autonomy", "from": pair[0].name(), "to": pair[1].name(), "trigger": "promote", "by": "human", "requires": "safety active"}));
            }
        }
        if !self.single_step {
            rows.push(json!({"dimension": "autonomy", "from": "any", "to": "any higher", "trigger": "promote", "by": "human", "requires": format!("safety active, at most {}", self.max_autonomy.name())}));
        }
        rows.push(json!({"dimension": "autonomy", "from": "any", "to": "any lower", "trigger": "demote", "by": "human", "requires": "nothing"}));
        rows.push(json!({"dimension": "safety", "from": "active", "to": "degraded", "trigger": "violation", "by": "crane or human", "requires": "a non-critical violation"}));
        if self.violations_to_quarantine > 0 {
            rows.push(json!({"dimension": "safety", "from": "degraded", "to": "quarantined", "trigger": "violation", "by": "crane or human", "requires": format!("{} violations in one incident", self.violations_to_quarantine)}));
        }
        rows.push(json!({"dimension": "safety", "from": "any", "to": "quarantined", "trigger": "critical violation or quarantine", "by": "crane or human", "requires": format!("a critical violation ({})", self.critical.iter().cloned().collect::<Vec<_>>().join(", "))}));
        for (from, recovery) in [
            ("degraded", &self.degraded),
            ("quarantined", &self.quarantined),
        ] {
            rows.push(json!({"dimension": "safety", "from": from, "to": recovery.to.name(), "trigger": "evidence", "by": "human or crane, per condition", "requires": describe_sets(&recovery.any_of)}));
        }
        rows
    }
}

/** Describe condition sets for people, such as "human_approval + verified_repair, or ..."
 * Input
    - sets: &[BTreeSet<Condition>] - sets
 * Output
    - String
*/
pub(crate) fn describe_sets(sets: &[BTreeSet<Condition>]) -> String {
    sets.iter()
        .map(|set| {
            set.iter()
                .map(|condition| condition.name())
                .collect::<Vec<_>>()
                .join(" + ")
        })
        .collect::<Vec<_>>()
        .join(", or ")
}

/** The state of one session in both dimensions, plus what the current incident has accumulated
 * Fields
    - autonomy: Autonomy - current autonomy mode
    - safety: SafetyState - current safety state
    - reason: Option<String> - why the safety state is not active
    - violations: u64 - violations in the current incident (since the session was last active)
    - evidence: BTreeSet<Condition> - recovery evidence since the latest violation
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct State {
    pub(crate) autonomy: Autonomy,
    pub(crate) safety: SafetyState,
    pub(crate) reason: Option<String>,
    pub(crate) violations: u64,
    pub(crate) evidence: BTreeSet<Condition>,
}

impl State {
    /** Build the state a session starts in: its bound autonomy, safety active
     * Input
        - autonomy: Autonomy - initial autonomy
     * Output
        - State
    */
    pub(crate) fn initial(autonomy: Autonomy) -> Self {
        Self {
            autonomy,
            safety: SafetyState::Active,
            reason: None,
            violations: 0,
            evidence: BTreeSet::new(),
        }
    }

    /** Serialize for status output
     * Input
        - None (uses self)
     * Output
        - Value
    */
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "autonomy": self.autonomy.name(),
            "safety": self.safety.name(),
            "reason": self.reason,
            "violations_in_incident": self.violations,
            "evidence": self.evidence.iter().map(|condition| condition.name()).collect::<Vec<_>>(),
        })
    }
}

/** One change of one dimension
 * Fields
    - dimension: &'static str - "autonomy" or "safety"
    - from: &'static str - previous value
    - to: &'static str - new value
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Change {
    pub(crate) dimension: &'static str,
    pub(crate) from: &'static str,
    pub(crate) to: &'static str,
}

impl Change {
    /** Serialize for the journal
     * Input
        - None (uses self)
     * Output
        - Value
    */
    pub(crate) fn to_json(self) -> Value {
        json!({"dimension": self.dimension, "from": self.from, "to": self.to})
    }
}

/** Apply one trigger: the only way either dimension ever changes. Promotions and demotions change
 * only autonomy; violations, quarantines, and evidence change only safety. Agents can cause
 * nothing; a rejected trigger leaves the state unchanged
 * Input
    - policy: &AutonomyPolicy - the session's bound policy
    - state: &State - current state
    - trigger: &Trigger - what happened
    - actor: Actor - who caused it
 * Output
    - Result<(State, Vec<Change>), String> the new state and the dimension changes (empty when only
      the incident's counters or evidence changed)
    - Error explaining why the transition is illegal (starting with SELF_ESCALATION for agents)
*/
pub(crate) fn step(
    policy: &AutonomyPolicy,
    state: &State,
    trigger: &Trigger,
    actor: Actor,
) -> Result<(State, Vec<Change>), String> {
    if actor == Actor::Agent {
        return Err(format!(
            "{SELF_ESCALATION}: an agent can never change its own autonomy or safety state"
        ));
    }
    let mut next = state.clone();
    match trigger {
        Trigger::Promote(to) => {
            if actor != Actor::Human {
                return Err("only a human can promote a session's autonomy".into());
            }
            if state.safety != SafetyState::Active {
                return Err(format!(
                    "autonomy can only be promoted while safety is active; the session is {}",
                    state.safety.name()
                ));
            }
            if *to <= state.autonomy {
                return Err(format!(
                    "promotion must raise autonomy above {}",
                    state.autonomy.name()
                ));
            }
            if *to > policy.max_autonomy {
                return Err(format!(
                    "the autonomy policy allows at most {}",
                    policy.max_autonomy.name()
                ));
            }
            let following = Autonomy::ALL
                .iter()
                .copied()
                .find(|level| *level > state.autonomy)
                .unwrap_or(state.autonomy);
            if policy.single_step && *to != following {
                return Err(format!(
                    "promotions go one step at a time: {} can only become {}",
                    state.autonomy.name(),
                    following.name()
                ));
            }
            next.autonomy = *to;
        }
        Trigger::Demote(to) => {
            if actor != Actor::Human {
                return Err("only a human can demote a session's autonomy".into());
            }
            if *to >= state.autonomy {
                return Err(format!(
                    "demotion must lower autonomy below {}",
                    state.autonomy.name()
                ));
            }
            next.autonomy = *to;
        }
        Trigger::Violation(kind) => {
            // Evidence must postdate the latest violation
            next.evidence.clear();
            next.violations += 1;
            if policy.critical.contains(kind) {
                next.safety = SafetyState::Quarantined;
                next.reason = Some(format!("critical violation: {kind}"));
            } else if state.safety == SafetyState::Active {
                next.safety = SafetyState::Degraded;
                next.violations = 1;
                next.reason = Some(format!("violation: {kind}"));
            } else if state.safety == SafetyState::Degraded
                && policy.violations_to_quarantine > 0
                && next.violations >= policy.violations_to_quarantine
            {
                next.safety = SafetyState::Quarantined;
                next.reason = Some(format!(
                    "{} violations in one incident (latest: {kind})",
                    next.violations
                ));
            }
        }
        Trigger::Quarantine(reason) => {
            next.evidence.clear();
            next.safety = SafetyState::Quarantined;
            next.reason = Some(reason.clone());
        }
        Trigger::Evidence(condition) => {
            if condition.source() != actor {
                return Err(format!(
                    "{} evidence can only come from {}",
                    condition.name(),
                    condition.source().name()
                ));
            }
            let Some(recovery) = policy.recovery(state.safety) else {
                // Nothing to recover from: evidence is not kept for a future incident
                return Ok((next, Vec::new()));
            };
            next.evidence.insert(*condition);
            if recovery
                .any_of
                .iter()
                .any(|set| set.is_subset(&next.evidence))
            {
                next.safety = recovery.to;
                next.evidence.clear();
                next.violations = 0;
                next.reason = (recovery.to != SafetyState::Active)
                    .then(|| format!("recovering from {}", state.safety.name()));
            }
        }
    }
    let mut changes = Vec::new();
    if next.autonomy != state.autonomy {
        changes.push(Change {
            dimension: "autonomy",
            from: state.autonomy.name(),
            to: next.autonomy.name(),
        });
    }
    if next.safety != state.safety {
        changes.push(Change {
            dimension: "safety",
            from: state.safety.name(),
            to: next.safety.name(),
        });
    }
    Ok((next, changes))
}

/** List what is still missing for each recovery set of the current safety state
 * Input
    - policy: &AutonomyPolicy - policy
    - state: &State - state
 * Output
    - Vec<Vec<Condition>> per set, the conditions not yet met (empty when safety is active)
*/
pub(crate) fn missing(policy: &AutonomyPolicy, state: &State) -> Vec<Vec<Condition>> {
    policy
        .recovery(state.safety)
        .map(|recovery| {
            recovery
                .any_of
                .iter()
                .map(|set| set.difference(&state.evidence).copied().collect())
                .collect()
        })
        .unwrap_or_default()
}
