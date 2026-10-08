// The compact projection the observability layer keeps for every session: one Act per journal
// event (with its evidence record), holding only what the aggregate pages read, with repeated
// strings and lists interned and identical governance and contract documents shared. It is
// derived from the journal through the evidence projection when a session's files change and
// thrown away with them, so it never disagrees with the records; the pages of one run or task read
// the full journal again instead.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};

/** Interned strings: each distinct value is stored once for the life of the process */
static STRINGS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();

/** Interned lists of interned strings, likewise */
static LISTS: OnceLock<Mutex<HashSet<&'static [&'static str]>>> = OnceLock::new();

/** Shared documents (bound governance and contracts), by their JSON text */
static DOCUMENTS: OnceLock<Mutex<std::collections::HashMap<String, Arc<Value>>>> = OnceLock::new();

/** Intern a string
 * Input
    - text: &str - text
 * Output
    - &'static str - the one shared copy
*/
pub(super) fn sym(text: &str) -> &'static str {
    let table = STRINGS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut table = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(found) = table.get(text) {
        return found;
    }
    let leaked: &'static str = Box::leak(text.to_string().into_boxed_str());
    table.insert(leaked);
    leaked
}

/** Intern a JSON string, None for anything else
 * Input
    - value: &Value - value
 * Output
    - Option<&'static str>
*/
pub(super) fn text(value: &Value) -> Option<&'static str> {
    value.as_str().map(sym)
}

/** Intern a JSON list of strings (an absent or non-list value is the empty list)
 * Input
    - value: &Value - value
 * Output
    - &'static [&'static str]
*/
pub(super) fn list(value: &Value) -> &'static [&'static str] {
    let items = value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.as_str()
                .map(String::from)
                .or_else(|| item["violation_type"].as_str().map(String::from))
        })
        .map(|item| sym(&item))
        .collect::<Vec<_>>();
    if items.is_empty() {
        return &[];
    }
    let table = LISTS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut table = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(found) = table.get(items.as_slice()) {
        return found;
    }
    let leaked: &'static [&'static str] = Box::leak(items.into_boxed_slice());
    table.insert(leaked);
    leaked
}

/** Share a JSON document: sessions bound to identical governance or contracts hold one copy
 * Input
    - value: Value - document
 * Output
    - Arc<Value>
*/
pub(super) fn share(value: Value) -> Arc<Value> {
    let key = value.to_string();
    let table = DOCUMENTS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut table = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    table.entry(key).or_insert_with(|| Arc::new(value)).clone()
}

/** A budget snapshot (NaN where the record has no value)
 * Fields
    - current: f64 - points held
    - available: f64 - points available
    - reserved: f64 - points reserved
    - max: f64 - maximum
*/
#[derive(Clone, Copy, Debug)]
pub(super) struct Budget {
    pub(super) current: f64,
    pub(super) available: f64,
    pub(super) reserved: f64,
    pub(super) max: f64,
}

impl Budget {
    /** Read a budget snapshot, None when the record has none
     * Input
        - value: &Value - {current, available, reserved, max}
     * Output
        - Option<Budget>
    */
    fn from_json(value: &Value) -> Option<Self> {
        if !value.is_object() {
            return None;
        }
        let number = |key: &str| value[key].as_f64().unwrap_or(f64::NAN);
        Some(Self {
            current: number("current"),
            available: number("available"),
            reserved: number("reserved"),
            max: number("max"),
        })
    }

    /** Return the points held as a whole number (0 when unknown)
     * Input
        - None (uses self)
     * Output
        - i64
    */
    pub(super) fn held(self) -> i64 {
        if self.current.is_finite() {
            self.current as i64
        } else {
            0
        }
    }

    /** Return the snapshot as the evidence record wrote it
     * Input
        - None (uses self)
     * Output
        - Value
    */
    pub(super) fn to_json(self) -> Value {
        let number = |value: f64| {
            if value.is_nan() {
                Value::Null
            } else if value.fract() == 0.0 && value.abs() < 9e15 {
                json!(value as i64)
            } else {
                json!(value)
            }
        };
        json!({"current": number(self.current), "available": number(self.available), "reserved": number(self.reserved), "max": number(self.max)})
    }
}

/** What only some events carry (kept apart so ordinary actions stay small)
 * Fields
    - verification: Option<&'static str> - verification outcome of a post-tool check
    - final_status: Option<&'static str> - final status of a stop or finalization
    - findings: &'static [&'static str] - findings of a finalization
    - trigger: Option<&'static str> - what changed autonomy
    - kind: Option<&'static str> - violation kind of an autonomy change
    - actor: Option<&'static str> - who changed autonomy
    - reason: Option<&'static str> - why
    - by: Option<&'static str> - who acted (delivery, approvals)
    - human: Option<&'static str> - the human action the evidence record names
*/
#[derive(Debug, Default)]
pub(super) struct Extra {
    pub(super) verification: Option<&'static str>,
    pub(super) final_status: Option<&'static str>,
    pub(super) findings: &'static [&'static str],
    pub(super) trigger: Option<&'static str>,
    pub(super) kind: Option<&'static str>,
    pub(super) actor: Option<&'static str>,
    pub(super) reason: Option<&'static str>,
    pub(super) by: Option<&'static str>,
    pub(super) human: Option<&'static str>,
}

/** One journal event with its evidence record, as the aggregate pages read it
 * Fields
    - seq: u64 - sequence number in the journal (with the session id, the event's immutable id)
    - at: u64 - time, Unix seconds
    - event: &'static str - event name
    - category: Option<&'static str> - evidence category (authorization, execution, autonomy, ...)
    - operation: Option<&'static str> - read, write, delete, execute
    - decision: Option<&'static str> - allow, deny, approval_required
    - tool: Option<&'static str> - tool name
    - autonomy: Option<&'static str> - autonomy level the event was decided at
    - safety: Option<&'static str> - safety state then
    - policies: &'static [&'static str] - policies involved
    - zones: &'static [&'static str] - zones involved
    - resources: &'static [&'static str] - resources touched
    - reasons: &'static [&'static str] - reasons of the decision
    - programs: &'static [&'static str] - programs a command runs
    - read: &'static [&'static str] - what a read-only action read
    - before: Option<Budget> - budget before
    - after: Option<Budget> - budget after
    - extra: Option<Box<Extra>> - what only some events carry
*/
#[derive(Debug)]
pub(super) struct Act {
    pub(super) seq: u64,
    pub(super) at: u64,
    pub(super) event: &'static str,
    pub(super) category: Option<&'static str>,
    pub(super) operation: Option<&'static str>,
    pub(super) decision: Option<&'static str>,
    pub(super) tool: Option<&'static str>,
    pub(super) autonomy: Option<&'static str>,
    pub(super) safety: Option<&'static str>,
    pub(super) policies: &'static [&'static str],
    pub(super) zones: &'static [&'static str],
    pub(super) resources: &'static [&'static str],
    pub(super) reasons: &'static [&'static str],
    pub(super) programs: &'static [&'static str],
    pub(super) read: &'static [&'static str],
    pub(super) before: Option<Budget>,
    pub(super) after: Option<Budget>,
    pub(super) extra: Option<Box<Extra>>,
}

/** The empty extra, for events that carry none */
static NONE: Extra = Extra {
    verification: None,
    final_status: None,
    findings: &[],
    trigger: None,
    kind: None,
    actor: None,
    reason: None,
    by: None,
    human: None,
};

impl Act {
    /** Project one journal event and its evidence record
     * Input
        - record: &Value - evidence record
        - event: &Value - journal event
     * Output
        - Act
    */
    pub(super) fn project(record: &Value, event: &Value) -> Self {
        let human = record["human"]["action"].as_str().map(sym);
        let extra = Extra {
            verification: text(&event["verification"]),
            final_status: text(&event["final_status"]),
            findings: list(&event["findings"]),
            trigger: text(&event["trigger"]),
            kind: text(&event["kind"]),
            actor: text(&event["actor"]),
            reason: text(&event["reason"]),
            by: text(&event["by"]),
            human,
        };
        let empty = extra.verification.is_none()
            && extra.final_status.is_none()
            && extra.findings.is_empty()
            && extra.trigger.is_none()
            && extra.kind.is_none()
            && extra.actor.is_none()
            && extra.reason.is_none()
            && extra.by.is_none()
            && extra.human.is_none();
        Self {
            seq: record["seq"].as_u64().unwrap_or(0),
            at: record["at"].as_u64().unwrap_or(0),
            event: text(&record["event"]).unwrap_or(""),
            category: text(&record["category"]),
            operation: text(&record["operation"]),
            decision: text(&record["decision"]),
            tool: text(&record["tool"]),
            autonomy: text(&record["autonomy"]),
            safety: text(&record["safety"]),
            policies: list(&record["policies"]),
            zones: list(&record["zones"]),
            resources: list(&record["resources"]),
            reasons: list(&record["reasons"]),
            programs: list(&record["action_summary"]["programs"]),
            read: list(&record["action_summary"]["read"]),
            before: Budget::from_json(&record["budget"]["before"]),
            after: Budget::from_json(&record["budget"]["after"]),
            extra: (!empty).then(|| Box::new(extra)),
        }
    }

    /** Return what only some events carry (empty for the rest)
     * Input
        - None (uses self)
     * Output
        - &Extra
    */
    pub(super) fn extra(&self) -> &Extra {
        self.extra.as_deref().unwrap_or(&NONE)
    }

    /** Check whether this is an authorization decision on a change (not a read)
     * Input
        - None (uses self)
     * Output
        - bool
    */
    pub(super) fn mutating(&self) -> bool {
        self.category == Some("authorization") && self.operation != Some("read")
    }

    /** Budget points this event consumed (before minus after, never negative)
     * Input
        - None (uses self)
     * Output
        - u64
    */
    pub(super) fn consumed(&self) -> u64 {
        let current = |budget: Option<Budget>| {
            budget
                .map(|budget| budget.current)
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map_or(0, |value| value as u64)
        };
        current(self.before).saturating_sub(current(self.after))
    }
}
