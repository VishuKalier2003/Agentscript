// Autonomy budget: the risk-bearing authority a session may spend on its own. It is not tokens,
// compute, or money. Each action the agent may take without a human costs points derived from a
// configurable risk-cost model (operation, resource criticality, environment, scope,
// reversibility, policy sensitivity, privilege escalation); violations are penalized; the budget
// regenerates only through controlled events and never above its maximum; humans refill it with an
// approver, a reason, and an expiry. The budget is a pure replay of the session journal under the
// model bound to the session, so it is deterministic and never shared between sessions.

pub(crate) mod manage;

#[cfg(test)] // Compile the module only when running tests, not in production builds
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::inventory::governance::glob;
use crate::util::sha256;
use crate::zones::model::{Autonomy, Criticality};

/** File in .crane holding the organization's risk-cost model (defaults apply without it) */
pub(crate) const MODEL_FILE: &str = "budget.json";

/** Fixed-point scale of multipliers: 100 means 1.0 */
const SCALE: u128 = 100;

/** Controlled events that regenerate budget, and who may report them
 * (name, reported by a human or trusted process through crane autonomy credit) */
pub(crate) const REGENERATION_SOURCES: &[(&str, bool)] = &[
    ("contract_completed", false),
    ("contract_tests_passed", false),
    ("task_milestone", true),
    ("human_review", true),
    ("merge", true),
    ("sustained_compliance", false),
];

/** One factor table: its name and the keys it must define */
const TABLES: &[(&str, &[&str])] = &[
    (
        "operation",
        &["read", "write", "delete", "execute", "other"],
    ),
    (
        "criticality",
        &["routine", "sensitive", "critical", "restricted"],
    ),
    ("environment", &["isolated", "workspace", "production"]),
    ("scope", &["task", "repository", "shared"]),
    ("reversibility", &["reversible", "irreversible"]),
    ("sensitivity", &["none", "contract"]),
];

/** Pattern lists that classify paths and commands */
const PATTERNS: &[&str] = &[
    "production_paths",
    "production_commands",
    "shared_paths",
    "irreversible_commands",
    "privileged_paths",
    "privileged_commands",
];

/** The organization's risk-cost model, bound to each session when it is created
 * Fields
    - max: BTreeMap<String, u64> - budget_max per initial autonomy mode
    - tables: BTreeMap<String, BTreeMap<String, u64>> - base cost per operation, and multipliers
      (percent) per criticality, environment, scope, reversibility, and policy sensitivity
    - privilege_escalation: u64 - cost added to any privilege-escalating action
    - patterns: BTreeMap<String, Vec<String>> - path globs and command fragments classifying
      production, shared, irreversible, and privileged actions
    - regeneration: BTreeMap<String, u64> - points per controlled event
    - compliance: (u64, u64, u64) - every N compliant actions regenerate M points, at most C per session
    - penalty: u64 - points a violation costs
    - critical_zeroes: bool - a critical violation sets the budget to zero (else costs critical_penalty)
    - critical_penalty: u64 - points a critical violation costs when it does not zero the budget
    - max_expiry: u64 - longest a refill may last, in seconds
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BudgetModel {
    pub(crate) max: BTreeMap<String, u64>,
    pub(crate) tables: BTreeMap<String, BTreeMap<String, u64>>,
    pub(crate) privilege_escalation: u64,
    pub(crate) patterns: BTreeMap<String, Vec<String>>,
    pub(crate) regeneration: BTreeMap<String, u64>,
    pub(crate) compliance: (u64, u64, u64),
    pub(crate) penalty: u64,
    pub(crate) critical_zeroes: bool,
    pub(crate) critical_penalty: u64,
    pub(crate) max_expiry: u64,
}

/** Build a string-keyed table
 * Input
    - entries: &[(&str, u64)] - keys and values
 * Output
    - BTreeMap<String, u64>
*/
fn table(entries: &[(&str, u64)]) -> BTreeMap<String, u64> {
    entries
        .iter()
        .map(|(key, value)| (key.to_string(), *value))
        .collect()
}

/** Build a pattern list
 * Input
    - items: &[&str] - patterns
 * Output
    - Vec<String>
*/
fn list(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

impl Default for BudgetModel {
    /** Build the default model, where a routine source write costs less than a shared library
     * write, which costs less than a production write, which costs less than privilege escalation
     * Input
        - None
     * Output
        - BudgetModel
    */
    fn default() -> Self {
        let tables = [
            (
                "operation",
                table(&[
                    ("read", 0),
                    ("write", 2),
                    ("delete", 4),
                    ("execute", 3),
                    ("other", 3),
                ]),
            ),
            (
                "criticality",
                table(&[
                    ("routine", 100),
                    ("sensitive", 150),
                    ("critical", 300),
                    ("restricted", 600),
                ]),
            ),
            (
                "environment",
                table(&[("isolated", 50), ("workspace", 100), ("production", 500)]),
            ),
            (
                "scope",
                table(&[("task", 100), ("repository", 125), ("shared", 250)]),
            ),
            (
                "reversibility",
                table(&[("reversible", 100), ("irreversible", 300)]),
            ),
            ("sensitivity", table(&[("none", 100), ("contract", 150)])),
        ]
        .into_iter()
        .map(|(name, values)| (name.to_string(), values))
        .collect();
        let patterns = [
            (
                "production_paths",
                list(&["**/prod/**", "**/production/**", "deploy/**", "**/*.prod.*"]),
            ),
            (
                "production_commands",
                list(&[
                    "kubectl ",
                    "terraform apply",
                    "helm upgrade",
                    "--env=prod",
                    "--context prod",
                ]),
            ),
            (
                "shared_paths",
                list(&["lib/**", "libs/**", "shared/**", "common/**", "packages/**"]),
            ),
            (
                "irreversible_commands",
                list(&[
                    "rm -rf",
                    "git push",
                    "git reset --hard",
                    "git clean",
                    "drop table",
                    "truncate table",
                ]),
            ),
            (
                "privileged_paths",
                list(&[
                    ".github/workflows/**",
                    "CODEOWNERS",
                    ".github/CODEOWNERS",
                    "**/sudoers*",
                ]),
            ),
            (
                "privileged_commands",
                list(&[
                    "sudo ",
                    "chmod +s",
                    "chown root",
                    "setcap ",
                    "gh secret",
                    "git config --global",
                ]),
            ),
        ]
        .into_iter()
        .map(|(name, values)| (name.to_string(), values))
        .collect();
        Self {
            max: table(&[
                ("observe", 0),
                ("assisted", 50),
                ("delegated", 100),
                ("autonomous", 200),
            ]),
            tables,
            privilege_escalation: 60,
            patterns,
            regeneration: table(&[
                ("contract_completed", 20),
                ("contract_tests_passed", 10),
                ("task_milestone", 10),
                ("human_review", 15),
                ("merge", 20),
            ]),
            compliance: (10, 2, 20),
            penalty: 10,
            critical_zeroes: true,
            critical_penalty: 50,
            max_expiry: 7 * 24 * 60 * 60,
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

/** Read a whole number
 * Input
    - value: &Value - JSON value
    - what: &str - name for errors
 * Output
    - Result<u64, String>
*/
fn number(value: &Value, what: &str) -> Result<u64, String> {
    value
        .as_u64()
        .ok_or_else(|| format!("{what} must be a whole number"))
}

/** Override a table's entries from JSON, rejecting unknown keys
 * Input
    - target: &mut BTreeMap<String, u64> - table with defaults
    - value: Option<&Value> - overrides
    - what: &str - name for errors
    - minimum: u64 - smallest accepted value
 * Output
    - Result<(), String>
*/
fn override_table(
    target: &mut BTreeMap<String, u64>,
    value: Option<&Value>,
    what: &str,
    minimum: u64,
) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    let keys = target.keys().cloned().collect::<Vec<_>>();
    let known = keys.iter().map(String::as_str).collect::<Vec<_>>();
    for (key, entry) in object(value, &known, what)? {
        let amount = number(entry, &format!("{what}.{key}"))?;
        if amount < minimum {
            return Err(format!("{what}.{key} must be at least {minimum}"));
        }
        target.insert(key.clone(), amount);
    }
    Ok(())
}

impl BudgetModel {
    /** Read a model from .crane/budget.json: every setting is optional and defaults as in
     * BudgetModel::default; unknown settings and invalid values are errors
     * Input
        - value: &Value - parsed JSON
     * Output
        - Result<BudgetModel, String>
    */
    pub(crate) fn from_json(value: &Value) -> Result<Self, String> {
        let mut model = Self::default();
        let mut known = vec![
            "max",
            "privilege_escalation",
            "patterns",
            "regeneration",
            "compliance",
            "penalties",
            "max_refill_expiry_seconds",
        ];
        known.extend(TABLES.iter().map(|(name, _)| *name));
        let map = object(value, &known, "the budget model")?;
        override_table(&mut model.max, map.get("max"), "max", 0)?;
        for (name, _) in TABLES {
            // Multipliers are percentages and must stay positive; base costs may be zero
            let minimum = if *name == "operation" { 0 } else { 1 };
            let entry = model.tables.get_mut(*name).ok_or("missing table")?;
            override_table(entry, map.get(*name), name, minimum)?;
        }
        if let Some(value) = map.get("privilege_escalation") {
            model.privilege_escalation = number(value, "privilege_escalation")?;
        }
        if let Some(value) = map.get("patterns") {
            for (name, items) in object(value, PATTERNS, "patterns")? {
                let items = items
                    .as_array()
                    .ok_or_else(|| format!("patterns.{name} must be a list of strings"))?
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .filter(|item| !item.trim().is_empty())
                            .map(String::from)
                            .ok_or_else(|| {
                                format!("patterns.{name} must be a list of non-empty strings")
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                model.patterns.insert(name.clone(), items);
            }
        }
        override_table(
            &mut model.regeneration,
            map.get("regeneration"),
            "regeneration",
            0,
        )?;
        if let Some(value) = map.get("compliance") {
            let entry = object(value, &["every", "amount", "cap"], "compliance")?;
            let field = |key: &str, default: u64| {
                entry.get(key).map_or(Ok(default), |value| {
                    number(value, &format!("compliance.{key}"))
                })
            };
            model.compliance = (
                field("every", model.compliance.0)?,
                field("amount", model.compliance.1)?,
                field("cap", model.compliance.2)?,
            );
            if model.compliance.0 == 0 {
                return Err("compliance.every must be at least 1".into());
            }
        }
        if let Some(value) = map.get("penalties") {
            let entry = object(value, &["violation", "critical"], "penalties")?;
            if let Some(value) = entry.get("violation") {
                model.penalty = number(value, "penalties.violation")?;
            }
            match entry.get("critical") {
                None => {}
                Some(Value::String(word)) if word == "zero" => model.critical_zeroes = true,
                Some(value) => {
                    model.critical_zeroes = false;
                    model.critical_penalty = number(value, "penalties.critical (or \"zero\")")?;
                }
            }
        }
        if let Some(value) = map.get("max_refill_expiry_seconds") {
            model.max_expiry = number(value, "max_refill_expiry_seconds")?;
            if model.max_expiry == 0 {
                return Err("max_refill_expiry_seconds must be at least 1".into());
            }
        }
        Ok(model)
    }

    /** Serialize the model in the same form from_json reads
     * Input
        - None (uses self)
     * Output
        - Value
    */
    pub(crate) fn to_json(&self) -> Value {
        let mut value = json!({
            "max": self.max,
            "privilege_escalation": self.privilege_escalation,
            "patterns": self.patterns,
            "regeneration": self.regeneration,
            "compliance": {"every": self.compliance.0, "amount": self.compliance.1, "cap": self.compliance.2},
            "penalties": {
                "violation": self.penalty,
                "critical": if self.critical_zeroes { json!("zero") } else { json!(self.critical_penalty) },
            },
            "max_refill_expiry_seconds": self.max_expiry,
        });
        for (name, values) in &self.tables {
            value[name] = json!(values);
        }
        value
    }

    /** Return the model's version, a digest of its canonical form
     * Input
        - None (uses self)
     * Output
        - String
    */
    pub(crate) fn version(&self) -> String {
        sha256(self.to_json().to_string().as_bytes())
    }

    /** Return budget_max for a session starting with this autonomy
     * Input
        - autonomy: Autonomy - initial autonomy
     * Output
        - u64
    */
    pub(crate) fn max_for(&self, autonomy: Autonomy) -> u64 {
        self.max.get(autonomy.name()).copied().unwrap_or(0)
    }

    /** Look up one table value
     * Input
        - name: &str - table
        - key: &str - entry
     * Output
        - u64 (0 for an unknown entry)
    */
    fn value(&self, name: &str, key: &str) -> u64 {
        self.tables
            .get(name)
            .and_then(|values| values.get(key))
            .copied()
            .unwrap_or(0)
    }

    /** Check a path against a pattern list
     * Input
        - name: &str - pattern list
        - path: &str - repository-relative path
     * Output
        - bool
    */
    fn path_matches(&self, name: &str, path: &str) -> bool {
        self.patterns
            .get(name)
            .into_iter()
            .flatten()
            .any(|pattern| {
                glob(pattern.as_bytes(), path.as_bytes())
                    || (!pattern.contains('/')
                        && glob(
                            pattern.as_bytes(),
                            path.rsplit('/').next().unwrap_or(path).as_bytes(),
                        ))
            })
    }

    /** Check a shell command against a pattern list of fragments (case and spacing ignored)
     * Input
        - name: &str - pattern list
        - command: &str - command text
     * Output
        - bool
    */
    fn command_matches(&self, name: &str, command: &str) -> bool {
        let normalized = format!(
            "{} ",
            command
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase()
        );
        self.patterns
            .get(name)
            .into_iter()
            .flatten()
            .any(|fragment| normalized.contains(&fragment.to_ascii_lowercase()))
    }

    /** Classify an action into its risk factors
     * Input
        - facts: &Facts - what the action does
     * Output
        - Factors
    */
    pub(crate) fn factors(&self, facts: &Facts) -> Factors {
        let command = facts.command.as_deref().unwrap_or_default();
        let any_path = |name: &str| {
            facts
                .paths
                .iter()
                .any(|path| self.path_matches(name, &path.path))
        };
        let criticality = facts
            .paths
            .iter()
            .map(|path| path.criticality)
            .max()
            .unwrap_or(Criticality::Routine);
        let environment = if any_path("production_paths")
            || self.command_matches("production_commands", command)
        {
            "production"
        } else if facts.isolated {
            "isolated"
        } else {
            "workspace"
        };
        let scope = if any_path("shared_paths") {
            "shared"
        } else if !facts.paths.is_empty()
            && facts.paths.iter().all(|path| path.in_task == Some(true))
        {
            "task"
        } else {
            "repository"
        };
        let reversibility = if self.command_matches("irreversible_commands", command) {
            "irreversible"
        } else {
            "reversible"
        };
        let sensitivity = if facts.paths.iter().any(|path| path.covered) {
            "contract"
        } else {
            "none"
        };
        Factors {
            operation: facts.operation,
            criticality,
            environment,
            scope,
            reversibility,
            sensitivity,
            privileged: any_path("privileged_paths")
                || self.command_matches("privileged_commands", command),
        }
    }

    /** Price an action: the operation's base cost times every multiplier, rounded up, plus the
     * privilege-escalation cost when it escalates; reads are free
     * Input
        - factors: &Factors - risk factors
     * Output
        - u64
    */
    pub(crate) fn cost(&self, factors: &Factors) -> u64 {
        if factors.operation == "read" {
            return 0;
        }
        let multipliers = [
            self.value("criticality", factors.criticality.name()),
            self.value("environment", factors.environment),
            self.value("scope", factors.scope),
            self.value("reversibility", factors.reversibility),
            self.value("sensitivity", factors.sensitivity),
        ];
        let numerator = multipliers.iter().fold(
            self.value("operation", factors.operation) as u128,
            |total, factor| total * *factor as u128,
        );
        let denominator = SCALE.pow(multipliers.len() as u32);
        let base = numerator.div_ceil(denominator) as u64;
        base + if factors.privileged {
            self.privilege_escalation
        } else {
            0
        }
    }
}

/** What one written path is, for pricing
 * Fields
    - path: String - repository-relative path
    - criticality: Criticality - highest zone criticality (routine when unzoned)
    - in_task: Option<bool> - inside the task scope, None without a task
    - covered: bool - covered by a contract clause
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathFacts {
    pub(crate) path: String,
    pub(crate) criticality: Criticality,
    pub(crate) in_task: Option<bool>,
    pub(crate) covered: bool,
}

/** What an action does, for pricing
 * Fields
    - operation: &'static str - read, write, delete, execute, or other
    - paths: Vec<PathFacts> - files it writes (empty for shell commands)
    - command: Option<String> - shell command text
    - isolated: bool - the session works in its own worktree
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Facts {
    pub(crate) operation: &'static str,
    pub(crate) paths: Vec<PathFacts>,
    pub(crate) command: Option<String>,
    pub(crate) isolated: bool,
}

/** The risk factors of an action
 * Fields
    - operation: &'static str - operation
    - criticality: Criticality - resource criticality
    - environment: &'static str - isolated, workspace, or production
    - scope: &'static str - task, repository, or shared
    - reversibility: &'static str - reversible or irreversible
    - sensitivity: &'static str - none or contract (policy sensitivity)
    - privileged: bool - escalates privilege
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Factors {
    pub(crate) operation: &'static str,
    pub(crate) criticality: Criticality,
    pub(crate) environment: &'static str,
    pub(crate) scope: &'static str,
    pub(crate) reversibility: &'static str,
    pub(crate) sensitivity: &'static str,
    pub(crate) privileged: bool,
}

impl Factors {
    /** Serialize for the journal
     * Input
        - None (uses self)
     * Output
        - Value
    */
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "operation": self.operation,
            "criticality": self.criticality.name(),
            "environment": self.environment,
            "scope": self.scope,
            "reversibility": self.reversibility,
            "sensitivity": self.sensitivity,
            "privileged": self.privileged,
        })
    }
}

/** Temporary budget a human refilled
 * Fields
    - amount: u64 - points left
    - expires_at: u64 - Unix seconds when what is left expires
    - approver: String - who approved it
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lot {
    pub(crate) amount: u64,
    pub(crate) expires_at: u64,
    pub(crate) approver: String,
}

/** A session's budget, replayed from its journal
 * Fields
    - max: u64 - budget_max
    - base: u64 - points not from a refill
    - lots: Vec<Lot> - unexpired refills, earliest expiry first
    - reserved: BTreeMap<String, u64> - points held for authorized actions not yet executed, by
      action digest
    - consumed: u64 - points spent by executed actions
    - regenerated: u64 - points regenerated
    - refilled: u64 - points refilled
    - expired: u64 - refilled points that expired unused
    - penalized: u64 - points lost to violations
    - streak: u64 - compliant actions since the last violation
    - actions: u64 - executed actions that consumed budget
    - credited: BTreeSet<String> - controlled events already credited (source:reference)
    - compliance_credited: u64 - points regenerated for sustained compliance
    - refill_requested: bool - the budget ran out and a human refill was requested
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BudgetState {
    pub(crate) max: u64,
    pub(crate) base: u64,
    pub(crate) lots: Vec<Lot>,
    pub(crate) reserved: BTreeMap<String, u64>,
    pub(crate) consumed: u64,
    pub(crate) regenerated: u64,
    pub(crate) refilled: u64,
    pub(crate) expired: u64,
    pub(crate) penalized: u64,
    pub(crate) streak: u64,
    pub(crate) actions: u64,
    pub(crate) credited: BTreeSet<String>,
    pub(crate) compliance_credited: u64,
    pub(crate) refill_requested: bool,
}

impl BudgetState {
    /** Build a full budget
     * Input
        - max: u64 - budget_max
     * Output
        - BudgetState
    */
    pub(crate) fn initial(max: u64) -> Self {
        Self {
            max,
            base: max,
            lots: Vec::new(),
            reserved: BTreeMap::new(),
            consumed: 0,
            regenerated: 0,
            refilled: 0,
            expired: 0,
            penalized: 0,
            streak: 0,
            actions: 0,
            credited: BTreeSet::new(),
            compliance_credited: 0,
            refill_requested: false,
        }
    }

    /** Return budget_current: everything left, including unexpired refills
     * Input
        - None (uses self)
     * Output
        - u64
    */
    pub(crate) fn current(&self) -> u64 {
        self.base + self.lots.iter().map(|lot| lot.amount).sum::<u64>()
    }

    /** Return budget_reserved
     * Input
        - None (uses self)
     * Output
        - u64
    */
    pub(crate) fn reserved_total(&self) -> u64 {
        self.reserved.values().sum()
    }

    /** Return what can still be committed: current minus reserved
     * Input
        - None (uses self)
     * Output
        - u64
    */
    pub(crate) fn available(&self) -> u64 {
        self.current().saturating_sub(self.reserved_total())
    }

    /** Drop what is left of refills that expired by a time
     * Input
        - at: u64 - Unix seconds
     * Output
        - None
    */
    pub(crate) fn expire(&mut self, at: u64) {
        let (expired, kept): (Vec<Lot>, Vec<Lot>) =
            self.lots.drain(..).partition(|lot| lot.expires_at <= at);
        self.expired += expired.iter().map(|lot| lot.amount).sum::<u64>();
        self.lots = kept;
    }

    /** Take points, from expiring refills first, then the base
     * Input
        - amount: u64 - points
     * Output
        - u64 points actually taken (never more than current)
    */
    fn draw(&mut self, amount: u64) -> u64 {
        let mut left = amount;
        for lot in self.lots.iter_mut() {
            let taken = left.min(lot.amount);
            lot.amount -= taken;
            left -= taken;
        }
        self.lots.retain(|lot| lot.amount > 0);
        let taken = left.min(self.base);
        self.base -= taken;
        left -= taken;
        amount - left
    }

    /** Add points to the base, never above budget_max
     * Input
        - amount: u64 - points
     * Output
        - u64 points actually added
    */
    fn credit(&mut self, amount: u64) -> u64 {
        let added = amount.min(self.max.saturating_sub(self.current()));
        self.base += added;
        added
    }

    /** Apply one journaled budget event at its time (refills that expired before it are dropped
     * first); unknown kinds are ignored
     * Input
        - event: &Value - {"kind": reserve|consume|release|release_all|autonomy|regenerate|penalty|
          refill|refill_requested, ...}
        - at: u64 - the event's Unix time
     * Output
        - None
    */
    pub(crate) fn apply(&mut self, event: &Value, at: u64) {
        self.expire(at);
        let amount = event["amount"].as_u64().unwrap_or(0);
        let digest = event["digest"].as_str().unwrap_or_default().to_string();
        match event["kind"].as_str() {
            Some("reserve") => {
                *self.reserved.entry(digest).or_insert(0) += amount;
            }
            Some("release") => {
                self.reserved.remove(&digest);
            }
            Some("release_all") => self.reserved.clear(),
            Some("consume") => {
                self.reserved.remove(&digest);
                self.consumed += self.draw(amount);
                self.actions += 1;
                if event["compliant"] == true {
                    self.streak += 1;
                } else {
                    self.streak = 0;
                }
            }
            Some("autonomy") => {
                // A human promotion grants the new mode's ceiling; a demotion lowers it
                let max = event["max"].as_u64().unwrap_or(self.max);
                if event["raise"] == true {
                    let grant = max.saturating_sub(self.max);
                    self.max = max;
                    self.credit(grant);
                } else {
                    self.max = max;
                    let excess = self.current().saturating_sub(max);
                    self.draw(excess);
                }
            }
            Some("regenerate") => {
                let added = self.credit(amount);
                self.regenerated += added;
                if let Some(key) = event["key"].as_str() {
                    self.credited.insert(key.to_string());
                }
                if event["source"] == "sustained_compliance" {
                    self.compliance_credited += added;
                }
                if self.available() > 0 {
                    self.refill_requested = false;
                }
            }
            Some("penalty") => {
                self.streak = 0;
                if event["zero"] == true {
                    self.penalized += self.current();
                    self.base = 0;
                    self.lots.clear();
                    self.reserved.clear();
                } else {
                    self.penalized += self.draw(amount);
                }
            }
            Some("refill") => {
                let added = amount.min(self.max.saturating_sub(self.current()));
                if added > 0 {
                    self.lots.push(Lot {
                        amount: added,
                        expires_at: event["expires_at"].as_u64().unwrap_or(at),
                        approver: event["approver"].as_str().unwrap_or_default().to_string(),
                    });
                    self.lots.sort_by_key(|lot| lot.expires_at);
                }
                self.refilled += added;
                self.refill_requested = false;
            }
            Some("refill_requested") => self.refill_requested = true,
            _ => {}
        }
    }

    /** Serialize for status output
     * Input
        - None (uses self)
     * Output
        - Value with budget_current, budget_max, budget_reserved, budget_consumed, and the rest
    */
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "budget_current": self.current(),
            "budget_max": self.max,
            "budget_reserved": self.reserved_total(),
            "budget_available": self.available(),
            "budget_consumed": self.consumed,
            "regenerated": self.regenerated,
            "refilled": self.refilled,
            "refill_expired": self.expired,
            "penalized": self.penalized,
            "compliant_streak": self.streak,
            "refills": self.lots.iter().map(|lot| json!({"remaining": lot.amount, "expires_at": lot.expires_at, "approver": lot.approver})).collect::<Vec<_>>(),
            "exhausted": self.available() == 0,
            "refill_requested": self.refill_requested,
        })
    }
}
