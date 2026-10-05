// Delivery configuration and merge eligibility: what to check before a pull request may merge, who
// must approve which changes, and which failing checks a human may except. Eligibility is a pure
// function of the configuration and the facts of one delivery round.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;

use serde_json::{json, Map, Value};

use crate::repository::root;
use crate::util::io_error;
use crate::zones::model::{Autonomy, Criticality};

/** File in .crane holding the delivery configuration (defaults apply without it) */
pub(crate) const CONFIG_FILE: &str = "delivery.json";

/** The check name of the contract tests: never excepted, whatever the configuration says */
pub(crate) const CONTRACT_TESTS: &str = "contract_tests";

/** The check name of the repository's ordinary tests */
pub(crate) const REPOSITORY_TESTS: &str = "repository_tests";

/** A configured lint or security check
 * Fields
    - name: String - check name
    - kind: String - lint, security, or other
    - command: Vec<String> - program and arguments, run in the delivery branch's working tree
    - timeout: u64 - seconds
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Check {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) command: Vec<String>,
    pub(crate) timeout: u64,
}

/** One merge rule: the changes it covers and what they need
 * Fields
    - name: String - rule name
    - autonomy: Option<Vec<Autonomy>> - session autonomy modes it covers, None for any
    - min_criticality: Option<Criticality> - lowest change criticality it covers
    - max_criticality: Option<Criticality> - highest change criticality it covers
    - approvals: u64 - distinct approvals required
    - approvers: Vec<String> - who may approve, empty for any mapped person
    - auto_merge: bool - merge as soon as the delivery is eligible
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rule {
    pub(crate) name: String,
    pub(crate) autonomy: Option<Vec<Autonomy>>,
    pub(crate) min_criticality: Option<Criticality>,
    pub(crate) max_criticality: Option<Criticality>,
    pub(crate) approvals: u64,
    pub(crate) approvers: Vec<String>,
    pub(crate) auto_merge: bool,
}

impl Rule {
    /** Check whether the rule covers a change
     * Input
        - autonomy: Autonomy - the session's final autonomy
        - criticality: Criticality - the highest criticality of the changed files
     * Output
        - bool
    */
    pub(crate) fn covers(&self, autonomy: Autonomy, criticality: Criticality) -> bool {
        self.autonomy
            .as_ref()
            .is_none_or(|modes| modes.contains(&autonomy))
            && self
                .min_criticality
                .is_none_or(|minimum| criticality >= minimum)
            && self
                .max_criticality
                .is_none_or(|maximum| criticality <= maximum)
    }

    /** Serialize for status output
     * Input
        - None (uses self)
     * Output
        - Value
    */
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "autonomy": self.autonomy.as_ref().map(|modes| modes.iter().map(|mode| mode.name()).collect::<Vec<_>>()),
            "min_criticality": self.min_criticality.map(Criticality::name),
            "max_criticality": self.max_criticality.map(Criticality::name),
            "approvals": self.approvals,
            "approvers": self.approvers,
            "auto_merge": self.auto_merge,
        })
    }
}

/** The delivery configuration, .crane/delivery.json
 * Fields
    - base_branch: Option<String> - branch pull requests merge into (default: the current branch)
    - branch_prefix: String - prefix of delivery branches
    - provider: String - local (branch, record, and git merge in this repository) or github (gh CLI)
    - checks: Vec<Check> - lint and security checks
    - rules: Vec<Rule> - merge rules, first match wins
    - default_rule: Rule - rule for changes no other rule covers
    - notify: Vec<String> - Slack channels and people to notify
    - signing_secret_env: String - environment variable holding the Slack signing secret
    - slack_users: BTreeMap<String, String> - Slack user id to the approver name it stands for
    - pr_url_template: Option<String> - link for local pull requests ({number}, {branch})
    - contract_url_template: Option<String> - link to the contract ({session}, {number})
    - excepted_checks: BTreeSet<String> - checks a human may except
    - max_exception: u64 - longest an exception may last, in seconds
    - default_exception: u64 - duration of an exception approved in Slack, in seconds
    - trackers: Value - tracker completion settings ({"jira": {"done_transition": "31"}})
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveryConfig {
    pub(crate) base_branch: Option<String>,
    pub(crate) branch_prefix: String,
    pub(crate) provider: String,
    pub(crate) checks: Vec<Check>,
    pub(crate) rules: Vec<Rule>,
    pub(crate) default_rule: Rule,
    pub(crate) notify: Vec<String>,
    pub(crate) signing_secret_env: String,
    pub(crate) slack_users: BTreeMap<String, String>,
    pub(crate) pr_url_template: Option<String>,
    pub(crate) contract_url_template: Option<String>,
    pub(crate) excepted_checks: BTreeSet<String>,
    pub(crate) max_exception: u64,
    pub(crate) default_exception: u64,
    pub(crate) trackers: Value,
}

impl Default for DeliveryConfig {
    /** Build the default configuration: autonomous sessions changing only routine code merge
     * automatically once every check passes; critical and restricted changes need two approvals;
     * everything else needs one; nothing can be excepted
     * Input
        - None
     * Output
        - DeliveryConfig
    */
    fn default() -> Self {
        Self {
            base_branch: None,
            branch_prefix: "crane/".into(),
            provider: "local".into(),
            checks: Vec::new(),
            rules: vec![
                Rule {
                    name: "autonomous-routine".into(),
                    autonomy: Some(vec![Autonomy::Autonomous]),
                    min_criticality: None,
                    max_criticality: Some(Criticality::Routine),
                    approvals: 0,
                    approvers: Vec::new(),
                    auto_merge: true,
                },
                Rule {
                    name: "critical".into(),
                    autonomy: None,
                    min_criticality: Some(Criticality::Critical),
                    max_criticality: None,
                    approvals: 2,
                    approvers: Vec::new(),
                    auto_merge: false,
                },
            ],
            default_rule: Rule {
                name: "default".into(),
                autonomy: None,
                min_criticality: None,
                max_criticality: None,
                approvals: 1,
                approvers: Vec::new(),
                auto_merge: false,
            },
            notify: Vec::new(),
            signing_secret_env: "SLACK_SIGNING_SECRET".into(),
            slack_users: BTreeMap::new(),
            pr_url_template: None,
            contract_url_template: None,
            excepted_checks: BTreeSet::new(),
            max_exception: 24 * 60 * 60,
            default_exception: 4 * 60 * 60,
            trackers: json!({}),
        }
    }
}

/** Reject keys an object does not know
 * Input
    - value: &Value - object
    - known: &[&str] - accepted keys
    - what: &str - name for errors
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

/** Read a list of strings
 * Input
    - value: Option<&Value> - JSON list
    - what: &str - name for errors
 * Output
    - Result<Vec<String>, String>
*/
fn strings(value: Option<&Value>, what: &str) -> Result<Vec<String>, String> {
    value.map_or(Ok(Vec::new()), |value| {
        value
            .as_array()
            .ok_or_else(|| format!("{what} must be a list of strings"))?
            .iter()
            .map(|item| {
                item.as_str()
                    .filter(|text| !text.trim().is_empty())
                    .map(String::from)
                    .ok_or_else(|| format!("{what} must be a list of non-empty strings"))
            })
            .collect()
    })
}

/** Read one merge rule; a rule that can cover critical or restricted changes must require approval
 * Input
    - value: &Value - JSON rule
    - what: &str - name for errors
 * Output
    - Result<Rule, String>
*/
fn parse_rule(value: &Value, what: &str) -> Result<Rule, String> {
    let map = object(
        value,
        &[
            "name",
            "autonomy",
            "min_criticality",
            "max_criticality",
            "approvals",
            "approvers",
            "auto_merge",
        ],
        what,
    )?;
    let level = |key: &str| {
        map.get(key)
            .map(|value| Criticality::parse(value.as_str().unwrap_or_default()))
            .transpose()
    };
    let rule = Rule {
        name: map
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(what)
            .to_string(),
        autonomy: map
            .get("autonomy")
            .map(|_| {
                strings(map.get("autonomy"), &format!("{what}.autonomy"))?
                    .iter()
                    .map(|mode| Autonomy::parse(mode))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?,
        min_criticality: level("min_criticality")?,
        max_criticality: level("max_criticality")?,
        approvals: map
            .get("approvals")
            .map(|value| {
                value
                    .as_u64()
                    .ok_or_else(|| format!("{what}.approvals must be a whole number"))
            })
            .transpose()?
            .unwrap_or(1),
        approvers: strings(map.get("approvers"), &format!("{what}.approvers"))?,
        auto_merge: map
            .get("auto_merge")
            .map(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| format!("{what}.auto_merge must be true or false"))
            })
            .transpose()?
            .unwrap_or(false),
    };
    if rule.approvals == 0
        && rule
            .max_criticality
            .is_none_or(|maximum| maximum >= Criticality::Critical)
    {
        return Err(format!(
            "{what} could merge critical or restricted changes without approval; set approvals to at least 1 or max_criticality below critical"
        ));
    }
    if !rule.approvers.is_empty() && (rule.approvers.len() as u64) < rule.approvals {
        return Err(format!(
            "{what} requires {} approvals but names only {} approvers",
            rule.approvals,
            rule.approvers.len()
        ));
    }
    Ok(rule)
}

impl DeliveryConfig {
    /** Read the configuration from JSON: every setting is optional; unknown settings and invalid
     * values are errors
     * Input
        - value: &Value - parsed .crane/delivery.json
     * Output
        - Result<DeliveryConfig, String>
    */
    pub(crate) fn from_json(value: &Value) -> Result<Self, String> {
        let mut config = Self::default();
        let map = object(
            value,
            &[
                "base_branch",
                "branch_prefix",
                "provider",
                "checks",
                "merge_policy",
                "slack",
                "exceptions",
                "trackers",
            ],
            "the delivery configuration",
        )?;
        if let Some(base) = map.get("base_branch") {
            config.base_branch = Some(
                base.as_str()
                    .ok_or("base_branch must be a string")?
                    .to_string(),
            );
        }
        if let Some(prefix) = map.get("branch_prefix") {
            config.branch_prefix = prefix
                .as_str()
                .ok_or("branch_prefix must be a string")?
                .to_string();
        }
        if let Some(provider) = map.get("provider") {
            config.provider = provider
                .as_str()
                .ok_or("provider must be a string")?
                .to_string();
            if !["local", "github"].contains(&config.provider.as_str()) {
                return Err(format!(
                    "unknown provider '{}'; use local or github",
                    config.provider
                ));
            }
        }
        if let Some(checks) = map.get("checks") {
            config.checks = Vec::new();
            for (index, check) in checks
                .as_array()
                .ok_or("checks must be a list")?
                .iter()
                .enumerate()
            {
                let what = format!("checks[{index}]");
                let entry = object(
                    check,
                    &["name", "kind", "command", "timeout_seconds"],
                    &what,
                )?;
                let name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{what}.name is required"))?
                    .to_string();
                if [CONTRACT_TESTS, REPOSITORY_TESTS].contains(&name.as_str())
                    || config.checks.iter().any(|other: &Check| other.name == name)
                {
                    return Err(format!("{what}.name '{name}' is reserved or used twice"));
                }
                let command = strings(entry.get("command"), &format!("{what}.command"))?;
                if command.is_empty() {
                    return Err(format!("{what}.command needs a program"));
                }
                config.checks.push(Check {
                    name,
                    kind: entry
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("lint")
                        .to_string(),
                    command,
                    timeout: entry
                        .get("timeout_seconds")
                        .and_then(Value::as_u64)
                        .unwrap_or(600),
                });
            }
        }
        if let Some(policy) = map.get("merge_policy") {
            let policy = object(policy, &["rules", "default"], "merge_policy")?;
            if let Some(rules) = policy.get("rules") {
                config.rules = rules
                    .as_array()
                    .ok_or("merge_policy.rules must be a list")?
                    .iter()
                    .enumerate()
                    .map(|(index, rule)| parse_rule(rule, &format!("merge_policy.rules[{index}]")))
                    .collect::<Result<_, _>>()?;
            }
            if let Some(default) = policy.get("default") {
                config.default_rule = parse_rule(default, "merge_policy.default")?;
            }
        }
        if let Some(slack) = map.get("slack") {
            let slack = object(
                slack,
                &[
                    "notify",
                    "signing_secret_env",
                    "users",
                    "pr_url_template",
                    "contract_url_template",
                ],
                "slack",
            )?;
            config.notify = strings(slack.get("notify"), "slack.notify")?;
            if let Some(name) = slack.get("signing_secret_env") {
                config.signing_secret_env = name
                    .as_str()
                    .ok_or("slack.signing_secret_env must be a string")?
                    .to_string();
            }
            if let Some(users) = slack.get("users") {
                for (id, name) in users
                    .as_object()
                    .ok_or("slack.users must map Slack user ids to approver names")?
                {
                    config.slack_users.insert(
                        id.clone(),
                        name.as_str()
                            .ok_or("slack.users values must be names")?
                            .to_string(),
                    );
                }
            }
            config.pr_url_template = slack
                .get("pr_url_template")
                .and_then(Value::as_str)
                .map(String::from);
            config.contract_url_template = slack
                .get("contract_url_template")
                .and_then(Value::as_str)
                .map(String::from);
        }
        if let Some(exceptions) = map.get("exceptions") {
            let exceptions = object(
                exceptions,
                &[
                    "allowed_checks",
                    "max_duration_seconds",
                    "default_duration_seconds",
                ],
                "exceptions",
            )?;
            config.excepted_checks = strings(
                exceptions.get("allowed_checks"),
                "exceptions.allowed_checks",
            )?
            .into_iter()
            .collect();
            if config.excepted_checks.contains(CONTRACT_TESTS) {
                return Err("contract tests can never be excepted".into());
            }
            if let Some(max) = exceptions.get("max_duration_seconds") {
                config.max_exception = max
                    .as_u64()
                    .filter(|max| *max > 0)
                    .ok_or("exceptions.max_duration_seconds must be a positive whole number")?;
            }
            if let Some(default) = exceptions.get("default_duration_seconds") {
                config.default_exception = default
                    .as_u64()
                    .filter(|default| *default > 0)
                    .ok_or("exceptions.default_duration_seconds must be a positive whole number")?;
            }
            config.default_exception = config.default_exception.min(config.max_exception);
        }
        if let Some(trackers) = map.get("trackers") {
            object(trackers, &["jira", "asana"], "trackers")?;
            config.trackers = trackers.clone();
        }
        Ok(config)
    }

    /** Select the merge rule for a change: the first covering rule, else the default
     * Input
        - autonomy: Autonomy - the session's final autonomy
        - criticality: Criticality - the highest criticality of the changed files
     * Output
        - &Rule
    */
    pub(crate) fn rule_for(&self, autonomy: Autonomy, criticality: Criticality) -> &Rule {
        self.rules
            .iter()
            .find(|rule| rule.covers(autonomy, criticality))
            .unwrap_or(&self.default_rule)
    }
}

/** Load the delivery configuration from .crane/delivery.json, or the default without one
 * Input
    - None
 * Output
    - Result<DeliveryConfig, String>
*/
pub(crate) fn load() -> Result<DeliveryConfig, String> {
    match fs::read_to_string(root()?.join(CONFIG_FILE)) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map_err(|error| error.to_string())
            .and_then(|value| DeliveryConfig::from_json(&value))
            .map_err(|error| format!(".crane/{CONFIG_FILE}: {error}")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(DeliveryConfig::default()),
        Err(error) => Err(io_error(error)),
    }
}

/** A human's exception to one failing check
 * Fields
    - id: String - exception id
    - check: String - the check it excepts
    - session: String - the session it is bound to
    - head: String - the commit it is bound to
    - approver: String - who approved it
    - reason: String - why
    - expires_at: u64 - Unix seconds
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Exception {
    pub(crate) id: String,
    pub(crate) check: String,
    pub(crate) session: String,
    pub(crate) head: String,
    pub(crate) approver: String,
    pub(crate) reason: String,
    pub(crate) expires_at: u64,
}

/** The facts of one delivery round that merge eligibility depends on
 * Fields
    - session: String - session id
    - decision: String - the session attestation's final decision
    - autonomy: Autonomy - the session's final autonomy
    - safety: String - the session's final safety state
    - criticality: Criticality - highest criticality of the changed files
    - contract_failed: u64 - contract tests that failed in the final run
    - checks: Vec<(String, String)> - repository tests and configured checks with their status
    - head: String - the commit the checks ran on
    - branch_head: Option<String> - the branch's commit now
    - approvals: Vec<(String, String)> - approver and the commit they approved
    - blocks: Vec<(String, String, String)> - rejections and change requests: kind, by, commit
    - exceptions: Vec<Exception> - exceptions granted
    - merged: bool - already merged
    - now: u64 - Unix seconds
*/
pub(crate) struct Facts {
    pub(crate) session: String,
    pub(crate) decision: String,
    pub(crate) autonomy: Autonomy,
    pub(crate) safety: String,
    pub(crate) criticality: Criticality,
    pub(crate) contract_failed: u64,
    pub(crate) checks: Vec<(String, String)>,
    pub(crate) head: String,
    pub(crate) branch_head: Option<String>,
    pub(crate) approvals: Vec<(String, String)>,
    pub(crate) blocks: Vec<(String, String, String)>,
    pub(crate) exceptions: Vec<Exception>,
    pub(crate) merged: bool,
    pub(crate) now: u64,
}

/** Check whether an exception currently applies to a check of a delivery round: it must name the
 * check, be bound to the same session and commit, and not have expired
 * Input
    - exception: &Exception - exception
    - check: &str - check name
    - facts: &Facts - the round
 * Output
    - bool
*/
pub(crate) fn applies(exception: &Exception, check: &str, facts: &Facts) -> bool {
    exception.check == check
        && exception.session == facts.session
        && exception.head == facts.head
        && exception.expires_at > facts.now
}

/** Decide whether a delivery round may merge: the session passed and is safe, the contract tests
 * passed (never excepted), every other check passed or has a valid exception, the branch did not
 * move since the checks, nothing is rejected or awaiting changes, and the covering rule's
 * approvals are in; autonomous merge needs a rule that allows it
 * Input
    - config: &DeliveryConfig - configuration
    - facts: &Facts - the round
 * Output
    - Value {eligible, auto_merge, rule, approvals: {required, counted, by}, missing, excepted}
*/
pub(crate) fn evaluate(config: &DeliveryConfig, facts: &Facts) -> Value {
    let rule = config.rule_for(facts.autonomy, facts.criticality);
    let mut missing = Vec::new();
    let mut excepted = Vec::new();
    if facts.merged {
        missing.push("already merged".to_string());
    }
    if facts.decision != "PASS" {
        missing.push(format!(
            "the session attestation is {}, not PASS",
            facts.decision
        ));
    }
    if facts.safety != "active" {
        missing.push(format!("the session ended {}, not active", facts.safety));
    }
    if facts.contract_failed > 0 {
        missing.push(format!(
            "{} contract tests failed (contract tests can never be excepted)",
            facts.contract_failed
        ));
    }
    for (check, status) in &facts.checks {
        if status == "passed" || status == "not_configured" {
            continue;
        }
        match facts.exceptions.iter().find(|exception| applies(exception, check, facts)) {
            Some(exception) => excepted.push(json!({"check": check, "exception": exception.id, "approver": exception.approver, "expires_at": exception.expires_at})),
            None => missing.push(format!("check {check} is {status}")),
        }
    }
    if facts.branch_head.as_deref() != Some(facts.head.as_str()) {
        missing.push("the branch moved since its checks ran; deliver again".into());
    }
    for (kind, by, head) in &facts.blocks {
        if *head == facts.head {
            missing.push(format!(
                "{by} {}",
                if kind == "rejection" {
                    "rejected the change"
                } else {
                    "requested changes"
                }
            ));
        }
    }
    let counted = facts
        .approvals
        .iter()
        .filter(|(by, head)| {
            *head == facts.head && (rule.approvers.is_empty() || rule.approvers.contains(by))
        })
        .map(|(by, _)| by.clone())
        .collect::<BTreeSet<_>>();
    if (counted.len() as u64) < rule.approvals {
        missing.push(format!(
            "{} of {} required approvals{}",
            counted.len(),
            rule.approvals,
            if rule.approvers.is_empty() {
                String::new()
            } else {
                format!(" from {}", rule.approvers.join(", "))
            }
        ));
    }
    let eligible = missing.is_empty();
    json!({
        "eligible": eligible,
        "auto_merge": eligible && rule.auto_merge,
        "rule": rule.to_json(),
        "criticality": facts.criticality.name(),
        "autonomy": facts.autonomy.name(),
        "approvals": {"required": rule.approvals, "counted": counted.len(), "by": counted},
        "missing": missing,
        "excepted": excepted,
    })
}
