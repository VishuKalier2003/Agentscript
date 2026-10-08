use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

use crate::agent_session::Governance;
use crate::ir::{Clause, ContractSet, Permission};
use crate::model::{ItemKind, Scope};
use crate::repository::git_raw;
use crate::resolver::{canonical_file, definitions, language, matches_target, Definition};
use crate::scope::{
    folder_of, folder_prefix, footprint, function_name, FlowFootprint, Footprint, ScopeContext,
    EXCLUDED_PREFIXES,
};
use crate::zones::model::{Autonomy, SafetyState};

/** Start of the denial reason when a session's autonomy budget is used up */
pub(crate) const BUDGET_EXHAUSTED: &str = "autonomy budget exhausted";

/** What a session has used of its autonomy budget, and the budget
 * Fields
    - actions: u64 - mutating tool calls authorized so far
    - files: BTreeSet<String> - distinct files those calls wrote
    - max_actions: u64 - action budget
    - max_files: u64 - file budget
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Usage {
    pub(crate) actions: u64,
    pub(crate) files: BTreeSet<String>,
    pub(crate) max_actions: u64,
    pub(crate) max_files: u64,
}

impl Usage {
    /** Return a usage without limits, for runtimes that have no session
     * Input
        - None
     * Output
        - Usage
    */
    pub(crate) fn unlimited() -> Self {
        Self {
            actions: 0,
            files: BTreeSet::new(),
            max_actions: u64::MAX,
            max_files: u64::MAX,
        }
    }
}

/** What a proposed tool call does, as far as can be told before it runs
 * Variants
    - Read - only inspects the repository
    - Write - replaces, edits, or deletes named files
    - Execute - runs a shell command, whose effects are unknown until it runs
    - Other - any other tool, such as an MCP server, whose effects are unknown
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Read,
    Write,
    Execute,
    Other,
}

impl Operation {
    /** Return the operation's name for journals and the neutral action format
     * Input
        - None (uses self)
     * Output
        - &'static str, "read", "write", "execute", or "other"
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Other => "other",
        }
    }

    /** Parse an operation name from the neutral action format
     * Input
        - value: &str - operation name
     * Output
        - Option<Operation>, None for an unknown name
    */
    pub(crate) fn parse(value: &str) -> Option<Self> {
        [Self::Read, Self::Write, Self::Execute, Self::Other]
            .into_iter()
            .find(|operation| operation.name() == value)
    }
}

/** One text replacement proposed by an edit tool
 * Fields
    - old: String - text to find
    - new: String - replacement text
    - all: bool - replace every occurrence instead of the first
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextEdit {
    pub(crate) old: String,
    pub(crate) new: String,
    pub(crate) all: bool,
}

/** The new state a write tool proposes for one file
 * Variants
    - Content(String) - the file's complete new text
    - Edits(Vec<TextEdit>) - replacements applied in order to the current text
    - Delete - the file is removed
    - Unknown - the tool does not say, so the effect cannot be simulated
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Proposed {
    Content(String),
    Edits(Vec<TextEdit>),
    Delete,
    Unknown,
}

/** A file a write tool proposes to change
 * Fields
    - path: String - path exactly as the tool gave it, absolute or relative to the working directory
    - proposed: Proposed - the proposed new state
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileChange {
    pub(crate) path: String,
    pub(crate) proposed: Proposed,
}

/** A proposed tool call in provider-neutral form, produced by an agent adapter
 * Fields
    - tool: String - provider tool name, recorded in the journal
    - operation: Operation - what kind of action it is
    - files: Vec<FileChange> - files a write proposes to change
    - command: Option<String> - shell command text for execute
    - arguments: Vec<String> - every string argument of an other tool, scanned for protected
      paths but never written to the journal
    - digest: String - SHA-256 of the raw tool input, recorded instead of the input itself
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentAction {
    pub(crate) tool: String,
    pub(crate) operation: Operation,
    pub(crate) files: Vec<FileChange>,
    pub(crate) command: Option<String>,
    pub(crate) arguments: Vec<String>,
    pub(crate) digest: String,
}

/** The runtime answer for a proposed action
 * Variants
    - Allow - the action may run
    - Deny - the action must not run
    - ApprovalRequired - a human must approve the action first (reserved; preserve and target
      only produce Allow and Deny)
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    Deny,
    ApprovalRequired,
}

impl Decision {
    /** Return the decision's name for journals and responses
     * Input
        - None (uses self)
     * Output
        - &'static str, "allow", "deny", or "approval_required"
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::ApprovalRequired => "approval_required",
        }
    }
}

/** A decision with its explanation
 * Fields
    - decision: Decision - allow, deny, or approval required
    - reasons: Vec<String> - why, naming the clauses involved
    - resources: Vec<String> - normalized resources the action touches, such as
      "file:payment.py" and "symbol:function:PaymentService.charge"
    - policies: Vec<String> - the policies whose clauses decided the action: the denying ones
      when a contract clause denies it, otherwise the authorizing ones (for observability)
    - zones: Vec<String> - every zone constraining the written files and changed symbols (for
      observability)
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) decision: Decision,
    pub(crate) reasons: Vec<String>,
    pub(crate) resources: Vec<String>,
    pub(crate) policies: Vec<String>,
    pub(crate) zones: Vec<String>,
}

/** Runtime authority for one clause: where its item lives at the checkpoint, or why that could
 * not be established (which makes the runtime fail closed)
*/
pub(crate) type Grant = Result<Footprint, String>;

/** Derive the runtime authority for every clause of a contract set, by locating each clause's
 * item (and tracing its flow for flow scope) in the clause's checkpoint commit, in clause order
 * Input
    - set: &ContractSet - compiled contracts
 * Output
    - Vec<Grant>, one per clause in ContractSet::clauses order
*/
pub(crate) fn derive_grants(set: &ContractSet) -> Vec<Grant> {
    let mut context = ScopeContext::new();
    set.clauses()
        .map(|(_, contract, clause)| match &contract.checkpoint_sha {
            Ok(commit) => footprint(
                &mut context,
                clause.scope,
                commit,
                clause.kind,
                &clause.target,
            ),
            Err(error) => Err(error.clone()),
        })
        .collect()
}

/** Serialize a grant for a session file
 * Input
    - grant: &Grant - runtime authority for one clause
 * Output
    - Value JSON object with location, baseline, and flow, or with error
*/
pub(crate) fn grant_to_json(grant: &Grant) -> Value {
    match grant {
        Err(error) => json!({ "error": error }),
        Ok(footprint) => json!({
            "location": footprint.location,
            "baseline": footprint.baseline,
            "flow": footprint.flow.as_ref().map(|flow| json!({
                "members": flow.members,
                "callers_of": flow.callers_of,
                "users_of": flow.users_of,
                "callees": flow.callees,
            })),
        }),
    }
}

/** Read a grant back from a session file
 * Input
    - value: &Value - JSON object written by grant_to_json
 * Output
    - Result<Grant, String>
    - Error if a field is missing or has the wrong type
*/
pub(crate) fn grant_from_json(value: &Value) -> Result<Grant, String> {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Ok(Err(error.to_string()));
    }
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| format!("grant is missing '{key}'"))
    };
    let names = |flow: &Value, key: &str| -> Result<BTreeSet<String>, String> {
        flow.get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("grant flow is missing '{key}'"))?
            .iter()
            .map(|name| {
                name.as_str()
                    .map(String::from)
                    .ok_or_else(|| format!("grant flow '{key}' must hold strings"))
            })
            .collect()
    };
    let flow = match value.get("flow") {
        None | Some(Value::Null) => None,
        Some(flow) => Some(FlowFootprint {
            members: names(flow, "members")?,
            callers_of: names(flow, "callers_of")?,
            users_of: names(flow, "users_of")?,
            callees: names(flow, "callees")?,
        }),
    };
    Ok(Ok(Footprint {
        location: text("location")?,
        baseline: text("baseline")?,
        flow,
    }))
}

/** How a proposed file change affects the code a clause covers
 * Variants
    - None - the covered code is untouched
    - Restores - the covered code changes back to its checkpoint version
    - Unknown(String) - the effect cannot be determined, with the reason
    - Mutates - the covered code changes to something other than its checkpoint version
*/
#[derive(Debug, Clone, PartialEq, Eq)]
enum Effect {
    None,
    Restores,
    Unknown(String),
    Mutates,
}

impl Effect {
    /** Combine two effects on the same clause, keeping the more serious one (Mutates, then
     * Unknown, then Restores, then None)
     * Input
        - other: Effect - second effect
     * Output
        - Effect
    */
    fn worst(self, other: Effect) -> Effect {
        let rank = |effect: &Effect| match effect {
            Effect::None => 0,
            Effect::Restores => 1,
            Effect::Unknown(_) => 2,
            Effect::Mutates => 3,
        };
        if rank(&other) > rank(&self) {
            other
        } else {
            self
        }
    }
}

/** The runtime authority the decision engine applies, independent of any agent provider
 * Fields
    - contracts: &ContractSet - the bound contract set
    - grants: &[Grant] - runtime authority per clause, aligned with ContractSet::clauses
    - root: PathBuf - repository root that action paths are resolved against
    - protected: &[&str] - provider-specific settings files that also count as protected metadata
    - problem: Option<String> - why the authority is unusable (session expired or could not be
      established), which denies every mutating action
    - governance: Option<&Governance> - the session's autonomy mode, budget, zones, and scope;
      None when there is no session
    - usage: Usage - what the session used of its budget
    - safety: SafetyState - the session's safety state
    - safety_reason: Option<String> - why the session is not active
*/
pub(crate) struct Runtime<'a> {
    pub(crate) contracts: &'a ContractSet,
    pub(crate) grants: &'a [Grant],
    pub(crate) root: PathBuf,
    pub(crate) protected: &'a [&'a str],
    pub(crate) problem: Option<String>,
    pub(crate) governance: Option<&'a Governance>,
    pub(crate) usage: Usage,
    pub(crate) safety: SafetyState,
    pub(crate) safety_reason: Option<String>,
    pub(crate) autonomy: Autonomy,
}

impl Runtime<'_> {
    /** Decide whether a proposed action may run, by allowing read-only actions, then denying any
     * action that touches Crane metadata, any mutating action while the authority is unusable or
     * incomplete (malformed policy, missing checkpoint, unresolvable target: fail closed), and
     * any write whose files cannot be determined, and any write whose simulated result changes
     * code a preserve clause covers (unless it restores the checkpoint version); every other
     * action is allowed, with shell and unknown tools left
     * to effect-level verification after they run
     * Input
        - action: &AgentAction - normalized proposed action
     * Output
        - Verdict
    */
    pub(crate) fn decide(&self, action: &AgentAction) -> Verdict {
        let mut resources = action
            .files
            .iter()
            .map(|change| {
                format!(
                    "file:{}",
                    self.relative(&change.path)
                        .unwrap_or_else(|| "(outside repository)".into())
                )
            })
            .collect::<Vec<_>>();
        let verdict = |decision, reasons: Vec<String>, resources| Verdict {
            decision,
            reasons,
            resources,
            policies: Vec::new(),
            zones: Vec::new(),
        };
        if action.operation == Operation::Read {
            return verdict(Decision::Allow, vec!["read-only tool".into()], resources);
        }
        if touches_metadata(action, self.protected) {
            return verdict(
                Decision::Deny,
                vec!["agents may not modify .crane metadata, run mutating crane commands, or edit agent hook settings; a human must review policy, checkpoint, and hook changes".into()],
                resources,
            );
        }
        let problems = self.problems();
        if !problems.is_empty() {
            return verdict(
                Decision::Deny,
                vec![format!(
                    "Crane cannot establish runtime authority ({}); mutating tools are denied and read-only tools remain available",
                    problems.join("; ")
                )],
                resources,
            );
        }
        if action.operation != Operation::Write {
            if let Some((decision, reasons)) = self.govern(&[]) {
                return verdict(decision, reasons, resources);
            }
            return verdict(
                Decision::Allow,
                vec!["the effects of this tool are not known before it runs; Crane verifies the repository after it runs".into()],
                resources,
            );
        }
        if action.files.is_empty() {
            return verdict(
                Decision::Deny,
                vec!["the files this write changes cannot be determined, so Crane cannot authorize it".into()],
                resources,
            );
        }
        let mut denied = Vec::new();
        let mut allowed = Vec::new();
        let mut changed = BTreeMap::new();
        let mut causes = (BTreeSet::new(), BTreeSet::new());
        for change in &action.files {
            self.check_change(
                change,
                &mut denied,
                &mut allowed,
                &mut resources,
                &mut changed,
                &mut causes,
            );
        }
        let paths = action
            .files
            .iter()
            .filter_map(|change| self.relative(&change.path))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|path| {
                let exists = self.root.join(&path).exists();
                let symbols = changed.get(&path).cloned().flatten();
                (path, exists, symbols)
            })
            .collect::<Vec<_>>();
        // Structured causes for observability: the deciding policies and the zones involved
        let zones = self.governance.map_or_else(Vec::new, |governance| {
            paths
                .iter()
                .filter_map(|(path, _, symbols)| governance.constraint_for(path, symbols.as_ref()))
                .flat_map(|(constraint, _)| constraint.zones)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        });
        let explained = |decision, reasons, resources, policies: BTreeSet<String>| Verdict {
            policies: policies.into_iter().collect(),
            zones: zones.clone(),
            ..verdict(decision, reasons, resources)
        };
        if !denied.is_empty() {
            return explained(Decision::Deny, denied, resources, causes.0);
        }
        if let Some((decision, reasons)) = self.govern(&paths) {
            return explained(decision, reasons, resources, causes.1);
        }
        if allowed.is_empty() {
            allowed.push("no contract clause covers this change".into());
        }
        explained(Decision::Allow, allowed, resources, causes.1)
    }

    /** Apply the session's governance to a mutating action the contract allows: deny when the
     * autonomy budget is used up or the session is quarantined; otherwise take the level
     * write_level leaves (session mode, safety state, the zones of every written file and changed
     * symbol, and the task scope); observe denies, assisted needs human approval, delegated and
     * autonomous allow
     * Input
        - paths: &[WrittenPath] - repository-relative paths written, whether each exists, and the
          symbols each change touches; empty for shell and unknown tools, whose files are not
          known before they run
     * Output
        - Option<(Decision, Vec<String>)> a denial or approval requirement with reasons, None to
          allow
    */
    fn govern(&self, paths: &[WrittenPath]) -> Option<(Decision, Vec<String>)> {
        let governance = self.governance?;
        if self.autonomy == Autonomy::Observe {
            // Checked before the budget: an observing session has no budget to exhaust
            return Some((
                Decision::Deny,
                vec!["the session's autonomy mode is observe, so it may only read".into()],
            ));
        }
        let usage = &self.usage;
        if usage.actions >= usage.max_actions {
            return Some((
                Decision::Deny,
                vec![format!(
                    "{BUDGET_EXHAUSTED}: {} of {} mutating actions used; a human must extend or resume the session",
                    usage.actions, usage.max_actions
                )],
            ));
        }
        let new_files = paths
            .iter()
            .filter(|(path, _, _)| !usage.files.contains(path))
            .count() as u64;
        if usage.files.len() as u64 + new_files > usage.max_files {
            return Some((
                Decision::Deny,
                vec![format!(
                    "{BUDGET_EXHAUSTED}: this change would write {} files in all, more than the {} allowed; a human must extend or resume the session",
                    usage.files.len() as u64 + new_files,
                    usage.max_files
                )],
            ));
        }
        let why = self.safety_reason.clone().unwrap_or_default();
        if self.safety == SafetyState::Quarantined {
            return Some((
                Decision::Deny,
                vec![format!(
                    "the session is quarantined ({why}); a human must resume it"
                )],
            ));
        }
        let (level, reasons) = write_level(governance, self.autonomy, self.safety, &why, paths);
        match level {
            Autonomy::Observe => Some((Decision::Deny, reasons)),
            Autonomy::Assisted => Some((Decision::ApprovalRequired, reasons)),
            _ => None,
        }
    }

    /** Select the clauses an executed action could have affected, for incremental verification
     * after the tool runs: none for read-only tools, all for shell and unknown tools, and for
     * writes the clauses whose covered code includes a written file or whose item or flow names
     * the file mentions (clauses without usable authority are always selected)
     * Input
        - action: &AgentAction - normalized action that ran
     * Output
        - Vec<bool>, one per clause in ContractSet::clauses order
    */
    pub(crate) fn relevant(&self, action: &AgentAction) -> Vec<bool> {
        match action.operation {
            Operation::Read => vec![false; self.grants.len()],
            Operation::Execute | Operation::Other => vec![true; self.grants.len()],
            Operation::Write => {
                let files = action
                    .files
                    .iter()
                    .filter_map(|change| self.relative(&change.path))
                    .map(|path| {
                        let text = fs::read(self.root.join(&path))
                            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                            .unwrap_or_default();
                        (path, text)
                    })
                    .collect::<Vec<_>>();
                self.contracts
                    .clauses()
                    .map(|(index, _, clause)| match &self.grants[index] {
                        Err(_) => true,
                        Ok(footprint) => files
                            .iter()
                            .any(|(path, text)| covers(clause, footprint, path, text)),
                    })
                    .collect()
            }
        }
    }

    /** List why the authority cannot be trusted: the runtime problem, malformed policies,
     * checkpoints that could not be loaded, and clauses whose item could not be located
     * Input
        - None (uses self)
     * Output
        - Vec<String>, empty when the authority is complete
    */
    fn problems(&self) -> Vec<String> {
        let mut problems = self.problem.iter().cloned().collect::<Vec<_>>();
        problems.extend(
            self.contracts
                .malformed
                .iter()
                .map(|policy| format!("policy {} is malformed", policy.policy_id)),
        );
        for (index, contract, clause) in self.contracts.clauses() {
            if let Err(error) = &self.grants[index] {
                problems.push(format!(
                    "policy {} cannot resolve {} {}: {error}",
                    contract.policy_id,
                    clause.kind.noun(),
                    clause.target
                ));
            }
        }
        problems
    }

    /** Check one proposed file change against every clause, by simulating the file's new text,
     * diffing its items, and recording a denial for each preserve clause whose covered code would
     * change (or whose effect cannot be determined) and an authorization for each target clause
     * whose covered code would change
     * Input
        - change: &FileChange - proposed change
        - denied: &mut Vec<String> - denial reasons, appended to
        - allowed: &mut Vec<String> - authorization reasons, appended to
        - resources: &mut Vec<String> - touched resources, appended to
        - changed: &mut ChangedSymbols - qualified names of the symbols changed per path, None for
          a path whose items cannot be determined (unsupported language or a new text that would
          not parse), updated
        - causes: &mut (BTreeSet<String>, BTreeSet<String>) - ids of the policies whose clauses
          deny the change and of those that authorize it, appended to
     * Output
        - None (appends to the vectors)
    */
    fn check_change(
        &self,
        change: &FileChange,
        denied: &mut Vec<String>,
        allowed: &mut Vec<String>,
        resources: &mut Vec<String>,
        changed: &mut ChangedSymbols,
        causes: &mut (BTreeSet<String>, BTreeSet<String>),
    ) {
        let Some(path) = self.relative(&change.path) else {
            return; // outside the repository, so no clause covers it
        };
        let before = fs::read(self.root.join(&path))
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        let after = apply(&change.proposed, before.as_deref());
        let items = ItemDiff::new(&path, before.as_deref(), &after);
        for (kind, qualified) in &items.changed {
            resources.push(format!("symbol:{}:{qualified}", kind.noun()));
        }
        let known = (language(&path).is_some() && items.after.is_ok()).then(|| {
            items
                .changed
                .iter()
                .map(|(_, qualified)| qualified.clone())
                .collect::<BTreeSet<_>>()
        });
        // Several changes to one file: every changed symbol counts, and one unknown makes all unknown
        let merged = match (changed.remove(&path), known) {
            (None, known) => known,
            (Some(Some(mut earlier)), Some(known)) => {
                earlier.extend(known);
                Some(earlier)
            }
            _ => None,
        };
        changed.insert(path.clone(), merged);
        for (index, contract, clause) in self.contracts.clauses() {
            let (Ok(footprint), Ok(commit)) = (&self.grants[index], &contract.checkpoint_sha)
            else {
                continue; // problems() already denied every mutating action
            };
            let label = format!(
                "{} {} {} scope {} (policy {})",
                clause.keyword(),
                clause.kind.noun(),
                clause.target,
                clause.scope.name(),
                contract.policy_id
            );
            let effect = self.effect(clause, footprint, commit, &path, &before, &after, &items);
            let policy = contract.policy_id.clone();
            match (clause.permission(), effect) {
                (_, Effect::None) => {}
                (Permission::PermitWrite, _) => {
                    causes.1.insert(policy);
                    allowed.push(format!("{path}: authorized by {label}"))
                }
                (Permission::DenyWrite, Effect::Restores) => {
                    causes.1.insert(policy);
                    allowed.push(format!(
                        "{path}: restores code protected by {label} to its checkpoint version"
                    ))
                }
                (Permission::DenyWrite, Effect::Mutates) => {
                    causes.0.insert(policy);
                    denied.push(format!("{path}: would modify code protected by {label}"))
                }
                (Permission::DenyWrite, Effect::Unknown(why)) => {
                    causes.0.insert(policy);
                    denied.push(format!(
                        "{path}: cannot confirm that code protected by {label} stays unchanged ({why})"
                    ))
                }
            }
        }
    }

    /** Work out how a change affects one clause: the clause's own item always counts; file,
     * folder, and all scopes add the whole file when it lies in the covered region; flow scope
     * adds changes to flow members and to code that would join the flow
     * Input
        - clause: &Clause - clause being checked
        - footprint: &Footprint - where its item lives at the checkpoint
        - commit: &str - the clause's checkpoint commit
        - path: &str - repository-relative path of the changed file
        - before: &Option<String> - current text, None if the file does not exist
        - after: &Result<Option<String>, String> - simulated new text (None when deleted), or
          why it cannot be simulated
        - items: &ItemDiff - item-level diff of the change
     * Output
        - Effect
    */
    #[allow(clippy::too_many_arguments)]
    fn effect(
        &self,
        clause: &Clause,
        footprint: &Footprint,
        commit: &str,
        path: &str,
        before: &Option<String>,
        after: &Result<Option<String>, String>,
        items: &ItemDiff,
    ) -> Effect {
        let item = items.item_effect(clause.kind, &clause.target, &footprint.baseline);
        let in_region = match clause.scope {
            Scope::Block | Scope::Flow => false,
            Scope::File => path == footprint.location,
            Scope::Folder => path.starts_with(&folder_prefix(&folder_of(&footprint.location))),
            Scope::All => !EXCLUDED_PREFIXES
                .iter()
                .any(|prefix| path.starts_with(prefix)),
        };
        let region = if in_region {
            file_effect(commit, path, before, after)
        } else if let (Scope::Flow, Some(flow)) = (clause.scope, &footprint.flow) {
            match items.flow_effect(path, flow) {
                Effect::Mutates if file_effect(commit, path, before, after) == Effect::Restores => {
                    Effect::Restores
                }
                effect => effect,
            }
        } else {
            Effect::None
        };
        item.worst(region)
    }

    /** Turn a tool path into a repository-relative path with "/" separators, by resolving it
     * against the working directory, canonicalizing both it and the repository root (so short
     * and long Windows names and symlinks agree), and stripping the root
     * Input
        - raw: &str - path as the tool gave it
     * Output
        - Option<String>, None if the path is outside the repository
    */
    pub(crate) fn relative(&self, raw: &str) -> Option<String> {
        let path = Path::new(raw);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir().ok()?.join(path)
        };
        let relative = canonical(&absolute)
            .strip_prefix(canonical(&self.root))
            .ok()?
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        (!relative.is_empty()).then_some(relative)
    }
}

/** A path a write changes: repository-relative path, whether it exists now, and the qualified
 * names of the symbols the change touches (None when they cannot be determined) */
pub(crate) type WrittenPath = (String, bool, Option<BTreeSet<String>>);

/** Qualified names of the symbols changed per path, None for a path whose items are unknown */
type ChangedSymbols = BTreeMap<String, Option<BTreeSet<String>>>;

/** Work out the autonomy level a write may run at and why it is lowered: the session's autonomy
 * mode, its safety state (degraded caps at assisted), the zones of every written file (for a file
 * whose zones select only some symbols, the zones of the symbols the change touches; all of them
 * when those are unknown), and the task scope (in delegated and autonomous mode, a write outside
 * the scope needs approval); shared by the pre-tool decision and the zone map view, so both always
 * agree
 * Input
    - governance: &Governance - the session's frozen governance
    - autonomy: Autonomy - the session's current autonomy mode
    - safety: SafetyState - the session's safety state (quarantine is handled by the caller)
    - why: &str - why the session is not active
    - paths: &[WrittenPath] - paths written
 * Output
    - (Autonomy, Vec<String>) the level and the reasons it is lowered
*/
pub(crate) fn write_level(
    governance: &Governance,
    autonomy: Autonomy,
    safety: SafetyState,
    why: &str,
    paths: &[WrittenPath],
) -> (Autonomy, Vec<String>) {
    let mut level = autonomy;
    let mut reasons = Vec::new();
    if level == Autonomy::Assisted {
        reasons.push(
            "the session's autonomy mode is assisted, so every change needs human approval"
                .to_string(),
        );
    }
    if safety == SafetyState::Degraded && level > Autonomy::Assisted {
        level = Autonomy::Assisted;
        reasons.push(format!(
            "the session is degraded ({why}), so changes need human approval"
        ));
    }
    for (path, exists, symbols) in paths {
        if let Some((constraint, on)) = governance.constraint_for(path, symbols.as_ref()) {
            let cap = constraint.autonomy.min(constraint.state.autonomy_cap());
            if cap <= Autonomy::Assisted {
                reasons.push(format!(
                    "{on}: zones {} ({}, {}) allow agents to {}",
                    constraint.zones.join(", "),
                    constraint.criticality.name(),
                    constraint.state.name(),
                    if cap == Autonomy::Observe {
                        "only observe it"
                    } else {
                        "change it only with human approval"
                    }
                ));
            }
            level = level.min(cap);
        }
        if autonomy >= Autonomy::Delegated && !governance.in_scope(path, *exists) {
            reasons.push(format!(
                "{path}: outside the task scope ({}), so it needs human approval",
                if governance.scope_modules.is_empty() {
                    "no module".to_string()
                } else {
                    governance.scope_modules.join(", ")
                }
            ));
            level = level.min(Autonomy::Assisted);
        }
    }
    (level, reasons)
}

/** Canonicalize a path that may not exist yet, by canonicalizing its longest existing ancestor
 * and then appending the remaining components, resolving "." and ".." lexically
 * Input
    - path: &Path - absolute path
 * Output
    - PathBuf
*/
fn canonical(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        if let Ok(mut resolved) = fs::canonicalize(&existing) {
            for name in rest.iter().rev() {
                if name == Component::ParentDir.as_os_str() {
                    resolved.pop();
                } else if name != Component::CurDir.as_os_str() {
                    resolved.push(name);
                }
            }
            return resolved;
        }
        let last = existing
            .components()
            .next_back()
            .map(|component| component.as_os_str().to_os_string());
        match last {
            Some(name) if existing.pop() => rest.push(name),
            _ => return path.to_path_buf(),
        }
    }
}

/** Simulate a proposed change on the current text, applying edits in order; an edit whose text
 * is not found is retried with CRLF line endings, since editors may normalize them
 * Input
    - proposed: &Proposed - proposed new state
    - before: Option<&str> - current text, None if the file does not exist
 * Output
    - Result<Option<String>, String> new text (None when deleted), or why it cannot be simulated
*/
fn apply(proposed: &Proposed, before: Option<&str>) -> Result<Option<String>, String> {
    match proposed {
        Proposed::Content(text) => Ok(Some(text.clone())),
        Proposed::Delete => Ok(None),
        Proposed::Unknown => Err("the tool does not state the new file content".into()),
        Proposed::Edits(edits) => {
            let mut text = before.unwrap_or_default().to_string();
            for edit in edits {
                if edit.old.is_empty() {
                    if !text.is_empty() {
                        return Err("an edit with empty old text targets a non-empty file".into());
                    }
                    text = edit.new.clone();
                    continue;
                }
                let (old, new) = if text.contains(&edit.old) {
                    (edit.old.clone(), edit.new.clone())
                } else {
                    (
                        edit.old.replace('\n', "\r\n"),
                        edit.new.replace('\n', "\r\n"),
                    )
                };
                if !text.contains(&old) {
                    return Err(
                        "the text to replace was not found, so the edit cannot be simulated".into(),
                    );
                }
                text = if edit.all {
                    text.replace(&old, &new)
                } else {
                    text.replacen(&old, &new, 1)
                };
            }
            Ok(Some(text))
        }
    }
}

/** Item-level difference between the current and simulated text of one file
 * Fields
    - before: Vec<Definition> - current items, empty if the file is missing, unsupported, or
      unparsable
    - after: Result<Vec<Definition>, String> - simulated items, or why they are unknown
    - before_text: String - current text, for name mentions when the new items are unknown
    - changed: BTreeSet<(ItemKind, String)> - items whose canonical text differs, added, or removed
*/
struct ItemDiff {
    before: Vec<Definition>,
    after: Result<Vec<Definition>, String>,
    before_text: String,
    changed: BTreeSet<(ItemKind, String)>,
}

impl ItemDiff {
    /** Parse both versions of a file into items and record which items changed, treating
     * unsupported files as having no items and an unparsable new version as unknown
     * Input
        - path: &str - repository-relative path, used to pick the language
        - before: Option<&str> - current text
        - after: &Result<Option<String>, String> - simulated text
     * Output
        - ItemDiff
    */
    fn new(path: &str, before: Option<&str>, after: &Result<Option<String>, String>) -> Self {
        let supported = language(path).is_some();
        let parse = |text: Option<&str>| match text {
            Some(text) if supported => definitions(text, path),
            _ => Ok(Vec::new()),
        };
        let old = parse(before).unwrap_or_default();
        let new = match after {
            Ok(text) => parse(text.as_deref())
                .map_err(|error| format!("the new text would not parse: {error}")),
            Err(why) => Err(why.clone()),
        };
        let mut changed = BTreeSet::new();
        if let Ok(new) = &new {
            let group = |items: &[Definition]| {
                let mut map = BTreeMap::<(ItemKind, String), Vec<String>>::new();
                for item in items {
                    map.entry((item.kind, item.qualified.clone()))
                        .or_default()
                        .push(item.snippet.clone());
                }
                map
            };
            let (old_map, new_map) = (group(&old), group(new));
            for key in old_map.keys().chain(new_map.keys()) {
                if old_map.get(key) != new_map.get(key) {
                    changed.insert(key.clone());
                }
            }
        }
        Self {
            before: old,
            after: new,
            before_text: before.unwrap_or_default().to_string(),
            changed,
        }
    }

    /** Work out how the change affects a clause's own item: unchanged copies mean None; new
     * copies that all equal the checkpoint version mean Restores; other changes mean Mutates;
     * when the new items are unknown, Unknown if the file currently holds or mentions the item
     * Input
        - kind: ItemKind - kind of the clause's item
        - target: &str - qualified target
        - baseline: &str - canonical text of the item at the checkpoint
     * Output
        - Effect
    */
    fn item_effect(&self, kind: ItemKind, target: &str, baseline: &str) -> Effect {
        let matching = |items: &[Definition]| {
            let mut snippets = items
                .iter()
                .filter(|item| {
                    item.kind == kind && matches_target(&item.name, &item.qualified, target)
                })
                .map(|item| item.snippet.clone())
                .collect::<Vec<_>>();
            snippets.sort();
            snippets
        };
        let old = matching(&self.before);
        match &self.after {
            Err(why) if !old.is_empty() || mentions(&self.before_text, &function_name(target)) => {
                Effect::Unknown(why.clone())
            }
            Err(_) => Effect::None,
            Ok(after) => {
                let new = matching(after);
                if new == old {
                    Effect::None
                } else if !new.is_empty() && new.iter().all(|snippet| snippet == baseline) {
                    Effect::Restores
                } else {
                    Effect::Mutates
                }
            }
        }
    }

    /** Work out how the change affects a flow: a changed item that is a flow member mutates it,
     * and so does a changed function that would join it (calling an upstream function, mentioning
     * an upstream item, or being named like a downstream call); when the new items are unknown,
     * Unknown if the file currently holds a member or mentions a flow name
     * Input
        - path: &str - repository-relative path
        - flow: &FlowFootprint - the clause's checkpoint flow
     * Output
        - Effect
    */
    fn flow_effect(&self, path: &str, flow: &FlowFootprint) -> Effect {
        let member = |qualified: &str| flow.members.contains(&format!("{path}#{qualified}"));
        let after = match &self.after {
            Ok(after) => after,
            Err(why) => {
                let names = flow
                    .callers_of
                    .iter()
                    .chain(&flow.users_of)
                    .chain(&flow.callees);
                let holds_member = self.before.iter().any(|item| member(&item.qualified));
                return if holds_member
                    || names
                        .into_iter()
                        .any(|name| mentions(&self.before_text, name))
                {
                    Effect::Unknown(why.clone())
                } else {
                    Effect::None
                };
            }
        };
        let joins = |item: &Definition| {
            item.kind == ItemKind::Function
                && (item.calls.iter().any(|call| flow.callers_of.contains(call))
                    || item.uses.iter().any(|name| flow.users_of.contains(name))
                    || flow.callees.contains(&item.name))
        };
        let touched = self.changed.iter().any(|(kind, qualified)| {
            member(qualified)
                || after
                    .iter()
                    .any(|item| item.kind == *kind && item.qualified == *qualified && joins(item))
        });
        if touched {
            Effect::Mutates
        } else {
            Effect::None
        }
    }
}

/** Work out how a change affects a whole file in a covered region, by comparing canonical text
 * (comments and trailing whitespace ignored) before and after, and calling a changed file a
 * restore when its new text equals the checkpoint version
 * Input
    - commit: &str - checkpoint commit
    - path: &str - repository-relative path
    - before: &Option<String> - current text
    - after: &Result<Option<String>, String> - simulated text, or why it is unknown
 * Output
    - Effect
*/
fn file_effect(
    commit: &str,
    path: &str,
    before: &Option<String>,
    after: &Result<Option<String>, String>,
) -> Effect {
    let canonical = |text: &Option<String>| match text {
        Some(text) => canonical_file(text, path).map(Some),
        None => Ok(None),
    };
    let new = match after {
        Err(why) => return Effect::Unknown(why.clone()),
        Ok(text) => match canonical(text) {
            Ok(new) => new,
            Err(error) => return Effect::Unknown(format!("the new text would not parse: {error}")),
        },
    };
    if canonical(before).ok() == Some(new.clone()) {
        return Effect::None;
    }
    let checkpoint = git_raw(&["show", &format!("{commit}:{path}")]).ok();
    if canonical(&checkpoint).ok() == Some(new) {
        Effect::Restores
    } else {
        Effect::Mutates
    }
}

/** Decide whether an executed write could have affected a clause, for incremental verification:
 * a file in the covered region, the file that defines the item, or a file mentioning the item's
 * or flow's names
 * Input
    - clause: &Clause - clause to test
    - footprint: &Footprint - where its item lives at the checkpoint
    - path: &str - repository-relative path that was written
    - text: &str - the file's text after the write
 * Output
    - bool, true if the clause must be verified
*/
fn covers(clause: &Clause, footprint: &Footprint, path: &str, text: &str) -> bool {
    let item = path == footprint.location || mentions(text, &function_name(&clause.target));
    match clause.scope {
        Scope::All => !EXCLUDED_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix)),
        Scope::Folder => path.starts_with(&folder_prefix(&folder_of(&footprint.location))),
        Scope::File | Scope::Block => item,
        Scope::Flow => {
            item || footprint.flow.as_ref().is_some_and(|flow| {
                flow.members
                    .iter()
                    .any(|member| member.starts_with(&format!("{path}#")))
                    || flow
                        .callers_of
                        .iter()
                        .chain(&flow.users_of)
                        .chain(&flow.callees)
                        .any(|name| mentions(text, name))
            })
        }
    }
}

/** Check whether text contains a name as a whole identifier, by finding each occurrence and
 * requiring that neither neighbor is a letter, digit, or underscore
 * Input
    - text: &str - text to search
    - name: &str - identifier
 * Output
    - bool, true if the name occurs as a whole word
*/
fn mentions(text: &str, name: &str) -> bool {
    let word = |character: Option<char>| {
        character.is_some_and(|character| character.is_alphanumeric() || character == '_')
    };
    !name.is_empty()
        && text.match_indices(name).any(|(start, _)| {
            !word(text[..start].chars().next_back())
                && !word(text[start + name.len()..].chars().next())
        })
}

/** Check whether an action would change Crane's own enforcement: write paths are checked by path
 * only (so documents that mention .crane stay editable), shell commands by their text and by
 * whether they run a mutating crane command (a missing command fails closed), and other tools by
 * every string argument
 * Input
    - action: &AgentAction - normalized action
    - extra: &[&str] - provider-specific protected settings files
 * Output
    - bool, true if the action touches protected metadata
*/
fn touches_metadata(action: &AgentAction, extra: &[&str]) -> bool {
    match action.operation {
        Operation::Read => false,
        Operation::Write => action
            .files
            .iter()
            .any(|change| references_protected_path(&change.path, extra)),
        Operation::Execute => action
            .command
            .as_deref()
            .map(|command| {
                references_protected_path(command, extra) || runs_mutating_crane(command)
            })
            .unwrap_or(true),
        Operation::Other => action
            .arguments
            .iter()
            .any(|argument| references_protected_path(argument, extra)),
    }
}

/** Check whether text references protected metadata, by first normalizing slashes and case, then
 * looking for the provider's settings files, and finally finding each ".crane" occurrence that
 * stands alone as a name (so names like my.crane or .craneignore do not match)
 * Input
    - value: &str - path or command text
    - extra: &[&str] - provider-specific protected paths, lowercase with "/" separators
 * Output
    - bool, true if the text references .crane or a protected settings file
*/
pub(crate) fn references_protected_path(value: &str, extra: &[&str]) -> bool {
    // Match .crane as a whole path component anywhere in the text, plus the hook settings that enforce Crane
    let text = value.replace('\\', "/").to_ascii_lowercase();
    if extra.iter().any(|path| text.contains(path)) {
        return true;
    }
    let is_name_character =
        |character: char| character.is_ascii_alphanumeric() || "_-.".contains(character);
    text.match_indices(".crane").any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + ".crane".len()..].chars().next();
        !before.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
            && !after.is_some_and(is_name_character)
    })
}

/** Detect shell commands by which an agent would change its own autonomy, safety, or budget:
 * crane autonomy promote/demote/approve/refill/credit, and crane agent session resume/extend
 * (which supply recovery evidence); these are denied like any mutating crane command and also quarantine the session
 * Input
    - command: &str - shell command text
 * Output
    - bool
*/
pub(crate) fn changes_own_autonomy(command: &str) -> bool {
    let tokens = command
        .split(|character: char| character.is_whitespace() || "&|;()`\"'".contains(character))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    tokens.iter().enumerate().any(|(index, token)| {
        let program = token.replace('\\', "/").to_ascii_lowercase();
        if !matches!(program.rsplit('/').next(), Some("crane" | "crane.exe")) {
            return false;
        }
        let at = |offset: usize| tokens.get(index + offset).copied().unwrap_or_default();
        matches!(
            (at(1), at(2), at(3)),
            (
                "autonomy",
                "promote" | "demote" | "approve" | "refill" | "credit",
                _
            ) | ("agent", "session", "resume" | "extend")
                | (
                    "deliver",
                    "run" | "approve" | "exception" | "merge" | "merged" | "slack-action",
                    _
                )
                | ("zones", "approve" | "reject", _)
                | ("task", "contract", "compile" | "approve" | "reject")
                | ("task", "approve" | "launch", _)
                | ("agent", "uninstall", _)
                | ("session", "run" | "finish", _)
                | ("flow", "advance", _)
        )
    })
}

/** Detect shell commands that change Crane metadata or authority through the CLI, by splitting
 * the command on whitespace and shell separators, finding tokens whose program name is crane or
 * crane.exe, and checking whether the next tokens are checkpoint, protect, target, init, agent
 * init/install/hook (a forged hook call could open or close a contract session), agent session
 * start/resume/cancel/finalize/sweep/quarantine/extend (session authority is a human's), or policy
 * approve/reject/edit/regenerate (an agent must never review or activate a policy proposal), or
 * task ingest/sync/advance/serve (an agent must never drive its own task lifecycle), or autonomy
 * promote/demote/approve/refill/credit (an agent must never change its own autonomy, safety, or
 * budget)
 * Input
    - command: &str - shell command text
 * Output
    - bool, true if the command runs a mutating crane subcommand
*/
pub(crate) fn runs_mutating_crane(command: &str) -> bool {
    // Re-baselining or rewriting policies through the CLI would bypass the file guard
    let tokens = command
        .split(|character: char| character.is_whitespace() || "&|;()`\"'".contains(character))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    tokens.iter().enumerate().any(|(index, token)| {
        let program = token.replace('\\', "/").to_ascii_lowercase();
        let program = program.rsplit('/').next().unwrap_or_default();
        if program != "crane" && program != "crane.exe" {
            return false;
        }
        match tokens.get(index + 1).copied() {
            Some("checkpoint" | "protect" | "target" | "init") => true,
            Some("agent") => match tokens.get(index + 2).copied() {
                Some("init" | "install" | "uninstall" | "hook") => true,
                // Session management changes who holds authority; listing and showing only read
                Some("session") => matches!(
                    tokens.get(index + 3).copied(),
                    Some(
                        "start"
                            | "resume"
                            | "cancel"
                            | "finalize"
                            | "sweep"
                            | "quarantine"
                            | "extend"
                            | "cleanup"
                    )
                ),
                _ => false,
            },
            Some("policy") => matches!(
                tokens.get(index + 2).copied(),
                Some("approve" | "reject" | "edit" | "regenerate")
            ),
            Some("task") => match tokens.get(index + 2).copied() {
                Some(
                    "ingest" | "sync" | "advance" | "serve" | "prepare" | "approve" | "launch",
                ) => true,
                // Compiling, approving, or rejecting a task contract is a human's; showing reads
                Some("contract") => matches!(
                    tokens.get(index + 3).copied(),
                    Some("compile" | "approve" | "reject")
                ),
                Some("completions") => {
                    matches!(tokens.get(index + 3).copied(), Some("send" | "reconcile"))
                }
                _ => false,
            },
            Some("autonomy") => matches!(
                tokens.get(index + 2).copied(),
                Some("promote" | "demote" | "approve" | "refill" | "credit")
            ),
            // Delivering, approving, excepting, and merging are a human's; status only reads
            Some("deliver") => !matches!(tokens.get(index + 2).copied(), Some("status")),
            // Connecting, serving the control plane, and changing contracts through its API
            Some("connect") => true,
            Some("zones") => matches!(
                tokens.get(index + 2).copied(),
                Some("approve" | "reject" | "review")
            ),
            Some("session") => matches!(tokens.get(index + 2).copied(), Some("run" | "finish")),
            Some("flow") => tokens.get(index + 2).copied() == Some("advance"),
            Some("repo") => matches!(
                tokens.get(index + 2).copied(),
                Some("connect" | "disconnect")
            ),
            // Serving the observability API opens a listener and granting or revoking a viewer
            // changes who may read; querying it (and listing viewers) only reads
            Some("observe") => matches!(tokens.get(index + 2).copied(), Some("serve" | "viewer")),
            Some("dashboard") => match tokens.get(index + 2).copied() {
                Some("api") => tokens
                    .get(index + 3)
                    .is_some_and(|method| method.eq_ignore_ascii_case("post")),
                _ => true,
            },
            _ => false,
        }
    })
}
