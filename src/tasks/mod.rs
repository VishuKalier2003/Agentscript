// Task-to-contract planning: a tracker-independent task description is analyzed against the
// inventory, the active policies, the zones, the checkpoint, and earlier contracts, and turned into
// a proposed task contract. Only code-like references can become authority; vague wording never
// widens scope, and an underspecified task is returned with exactly what is missing.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::inventory::graph::Graph;
use crate::inventory::Inventory;
use crate::ir::checkpoint_commit;
use crate::policy::parse;
use crate::repository::{git, load_checkpoint, root};
use crate::resolver::matches_target;
use crate::session::{read_attestation, session_ids, ContractSession};
use crate::util::{io_error, sha256};
use crate::zones::model::Autonomy;
use crate::zones::{inspect, Zones};

/** Version of the task plan JSON layout */
const PLAN_FORMAT: u64 = 1;

/** Phrases that make a clause say its code must stay as it is */
const NEGATIONS: &[&str] = &[
    "must not change",
    "must not be changed",
    "must not modify",
    "must not be modified",
    "must not touch",
    "must not be touched",
    "must not be edited",
    "do not change",
    "do not modify",
    "do not touch",
    "do not edit",
    "don't change",
    "don't modify",
    "don't touch",
    "don't edit",
    "never change",
    "never modify",
    "remain unchanged",
    "remains unchanged",
    "stay unchanged",
    "stays unchanged",
    "left unchanged",
    "leave unchanged",
    "without changing",
    "without modifying",
    "without touching",
    "untouched",
    "read-only",
];

/** Phrases asking for unbounded changes, which no task contract grants */
const BROAD: &[&str] = &[
    "everywhere",
    "wherever",
    "entire codebase",
    "whole codebase",
    "entire repository",
    "whole repository",
    "entire repo",
    "whole repo",
    "all files",
    "all modules",
    "all services",
    "across the codebase",
    "across the repository",
    "any file",
    "as needed",
    "whatever is necessary",
];

/** Words that start a new clause, so a negation only applies to its own clause */
const CLAUSE_WORDS: &[&str] = &[
    " but ",
    " without ",
    " while ",
    " except ",
    " and do not ",
    " and don't ",
];

/** A task normalized from any tracker
 * Fields
    - task_id: String - tracker id, such as PAY-1821
    - title: String - one-line summary
    - description: String - what must change and why
    - acceptance_criteria: Vec<String> - conditions that make the task done
    - repositories: Vec<String> - repositories the task changes (names or owner/name)
    - requester: Option<String> - who asked
    - team: Option<String> - owning team
    - priority: Option<String> - priority as given
    - labels: Vec<String> - labels as given
    - symbols: Vec<String> - references.symbols: code that must change
    - must_not_change: Vec<String> - references.must_not_change: code that must stay as it is
    - files: Vec<String> - references.files: files in scope
    - modules: Vec<String> - references.modules: modules in scope
*/
pub(crate) struct TaskInput {
    pub(crate) task_id: String,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) acceptance_criteria: Vec<String>,
    pub(crate) repositories: Vec<String>,
    pub(crate) requester: Option<String>,
    pub(crate) team: Option<String>,
    pub(crate) priority: Option<String>,
    pub(crate) labels: Vec<String>,
    pub(crate) symbols: Vec<String>,
    pub(crate) must_not_change: Vec<String>,
    pub(crate) files: Vec<String>,
    pub(crate) modules: Vec<String>,
}

impl TaskInput {
    /** Read a task from its JSON form; missing fields become empty (and are reported as
     * clarifications by the planner), while fields of the wrong type are errors
     * Input
        - value: &Value - task JSON
     * Output
        - Result<TaskInput, String>
        - Error naming a field with the wrong type or an unsupported format
    */
    pub(crate) fn from_json(value: &Value) -> Result<Self, String> {
        if !value.is_object() {
            return Err("a task must be a JSON object".into());
        }
        if !matches!(value.get("task_format"), None | Some(Value::Null))
            && value["task_format"].as_u64() != Some(1)
        {
            return Err("unsupported task_format; expected 1".into());
        }
        let text = |key: &str| -> Result<Option<String>, String> {
            match value.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(text)) => {
                    Ok(Some(text.trim().to_string()).filter(|text| !text.is_empty()))
                }
                Some(_) => Err(format!("task field '{key}' must be text")),
            }
        };
        let list = |container: &Value, key: &str, label: &str| -> Result<Vec<String>, String> {
            match container.get(key) {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(Value::String(text)) => Ok(vec![text.trim().to_string()]),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(|text| text.trim().to_string())
                            .ok_or_else(|| format!("task field '{label}' must hold text"))
                    })
                    .filter(|item| item.as_ref().map_or(true, |text| !text.is_empty()))
                    .collect(),
                Some(_) => Err(format!("task field '{label}' must be a list of text")),
            }
        };
        let references = value.get("references").cloned().unwrap_or(Value::Null);
        if !references.is_null() && !references.is_object() {
            return Err("task field 'references' must be an object".into());
        }
        Ok(Self {
            task_id: text("task_id")?.unwrap_or_default(),
            title: text("title")?.unwrap_or_default(),
            description: text("description")?.unwrap_or_default(),
            acceptance_criteria: list(value, "acceptance_criteria", "acceptance_criteria")?,
            repositories: list(value, "repositories", "repositories")?,
            requester: text("requester")?,
            team: text("team")?,
            priority: text("priority")?,
            labels: list(value, "labels", "labels")?,
            symbols: list(&references, "symbols", "references.symbols")?,
            must_not_change: list(&references, "must_not_change", "references.must_not_change")?,
            files: list(&references, "files", "references.files")?,
            modules: list(&references, "modules", "references.modules")?,
        })
    }

    /** Serialize the normalized task
     * Input
        - None (uses self)
     * Output
        - Value JSON object
    */
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "task_format": 1,
            "task_id": self.task_id,
            "title": self.title,
            "description": self.description,
            "acceptance_criteria": self.acceptance_criteria,
            "repositories": self.repositories,
            "requester": self.requester,
            "team": self.team,
            "priority": self.priority,
            "labels": self.labels,
            "references": {
                "symbols": self.symbols,
                "must_not_change": self.must_not_change,
                "files": self.files,
                "modules": self.modules,
            },
        })
    }
}

/** Check a task id: 1 to 64 ASCII letters, digits, '.', '_', or '-'
 * Input
    - id: &str - task id
 * Output
    - bool
*/
fn valid_task_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
}

/** Load a task by id from .crane/tasks/ID.json, or from a path
 * Input
    - spec: &str - task id or path to a task file
 * Output
    - Result<(TaskInput, String), String> the task and the digest of its file
    - Error if the file is missing or invalid
*/
pub(crate) fn load(spec: &str) -> Result<(TaskInput, String), String> {
    let path = if spec.ends_with(".json") || spec.contains('/') || spec.contains('\\') {
        PathBuf::from(spec)
    } else if valid_task_id(spec) {
        root()?.join("tasks").join(format!("{spec}.json"))
    } else {
        return Err(format!("invalid task id '{spec}'"));
    };
    let content = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read task {}: {}", path.display(), io_error(error)))?;
    let value: Value = serde_json::from_str(&content)
        .map_err(|error| format!("task {} is not valid JSON: {error}", path.display()))?;
    let task = TaskInput::from_json(&value)
        .map_err(|error| format!("task {}: {error}", path.display()))?;
    Ok((task, sha256(content.as_bytes())))
}

/** One reference to code found in the task
 * Fields
    - token: String - the referenced name or path, as written
    - source: &'static str - field it came from
    - negated: bool - whether its clause says the code must stay as it is
    - explicit: bool - whether it came from references rather than prose
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mention {
    pub(crate) token: String,
    pub(crate) source: &'static str,
    pub(crate) negated: bool,
    pub(crate) explicit: bool,
}

/** Check whether a prose word looks like code: a path, a qualified name (Type.method or
 * Type::method), a snake_case or camelCase identifier; plain words never qualify
 * Input
    - word: &str - word with surrounding punctuation removed
 * Output
    - bool
*/
fn code_like(word: &str) -> bool {
    let Some(first) = word.chars().next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    if word.contains('/') {
        return true;
    }
    if !word
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "_.:".contains(character))
    {
        return false;
    }
    let dotted = word.replace("::", ".");
    if dotted.contains('.') {
        let parts = dotted.split('.').collect::<Vec<_>>();
        return parts.iter().all(|part| !part.is_empty())
            && parts.iter().any(|part| part.len() >= 3)
            && !dotted.contains(':');
    }
    let camel = word
        .chars()
        .zip(word.chars().skip(1))
        .any(|(left, right)| left.is_ascii_lowercase() && right.is_ascii_uppercase());
    camel
        || (word.contains('_')
            && word
                .chars()
                .any(|character| character.is_ascii_alphabetic()))
}

/** Remove call parentheses and trailing punctuation from a code token
 * Input
    - token: &str - raw token
 * Output
    - String
*/
fn clean(token: &str) -> String {
    token
        .trim()
        .trim_end_matches("()")
        .trim_end_matches(['.', ',', ';', ':', '!', '?'])
        .trim_end_matches("()")
        .to_string()
}

/** Find the code references in one clause: every backticked span without spaces, and every
 * code-like word of the prose around them
 * Input
    - clause: &str - clause text
 * Output
    - Vec<String> tokens in order
*/
pub(crate) fn code_tokens(clause: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut prose = String::new();
    for (index, part) in clause.split('`').enumerate() {
        if index % 2 == 1 {
            let token = clean(part);
            if !token.is_empty() && !token.contains(char::is_whitespace) {
                tokens.push(token);
            }
        } else {
            prose.push_str(part);
            prose.push(' ');
        }
    }
    for word in prose
        .split(|character: char| character.is_whitespace() || ",;!?\"'()[]{}<>".contains(character))
    {
        let word = clean(word.trim_start_matches('.'));
        if code_like(&word) {
            tokens.push(word);
        }
    }
    tokens
}

/** Split text into clauses: sentences, then parts separated by commas, semicolons, or words such
 * as "but" and "without", so a negation applies only to the clause holding it
 * Input
    - text: &str - prose
 * Output
    - Vec<String> clauses
*/
fn clauses(text: &str) -> Vec<String> {
    let mut clauses = Vec::new();
    for sentence in text.replace(". ", ".\n").split(['\n', ';', ',']) {
        // ASCII lowercasing keeps byte offsets, so cuts found in it apply to the sentence
        let lower = sentence.to_ascii_lowercase();
        let mut cuts = CLAUSE_WORDS
            .iter()
            .flat_map(|word| lower.match_indices(word).map(|(index, _)| index))
            .filter(|index| *index > 0)
            .collect::<Vec<_>>();
        cuts.sort_unstable();
        cuts.dedup();
        let mut start = 0;
        for cut in cuts {
            clauses.push(sentence[start..cut].to_string());
            start = cut;
        }
        clauses.push(sentence[start..].to_string());
    }
    clauses.retain(|clause| !clause.trim().is_empty());
    clauses
}

/** Collect every code reference of a task: explicit references first, then prose mentions from
 * the title, description, and acceptance criteria, each marked negated when its clause says the
 * code must stay as it is
 * Input
    - task: &TaskInput - task
 * Output
    - Vec<Mention>
*/
pub(crate) fn mentions(task: &TaskInput) -> Vec<Mention> {
    let mut found = Vec::new();
    let explicit = |tokens: &[String], source: &'static str, negated: bool| {
        tokens
            .iter()
            .map(move |token| Mention {
                token: token.clone(),
                source,
                negated,
                explicit: true,
            })
            .collect::<Vec<_>>()
    };
    found.extend(explicit(&task.symbols, "references.symbols", false));
    found.extend(explicit(
        &task.must_not_change,
        "references.must_not_change",
        true,
    ));
    found.extend(explicit(&task.files, "references.files", false));
    found.extend(explicit(&task.modules, "references.modules", false));
    let prose = std::iter::once(("title", task.title.as_str()))
        .chain(std::iter::once(("description", task.description.as_str())))
        .chain(
            task.acceptance_criteria
                .iter()
                .map(|criterion| ("acceptance_criteria", criterion.as_str())),
        );
    for (source, text) in prose {
        for clause in clauses(text) {
            let lower = clause.to_ascii_lowercase();
            let negated = NEGATIONS.iter().any(|phrase| lower.contains(phrase));
            for token in code_tokens(&clause) {
                found.push(Mention {
                    token,
                    source,
                    negated,
                    explicit: false,
                });
            }
        }
    }
    found
}

/** Return the names this repository answers to: its folder name, its origin remote's name, and
 * "."
 * Input
    - inventory: &Inventory - inventory with the repository root
 * Output
    - BTreeSet<String> lowercase names
*/
fn repository_names(inventory: &Inventory) -> BTreeSet<String> {
    let mut names = BTreeSet::from([".".to_string()]);
    if let Some(name) = inventory.root.file_name().and_then(|name| name.to_str()) {
        names.insert(name.to_ascii_lowercase());
    }
    if let Ok(url) = git(&["config", "--get", "remote.origin.url"]) {
        if let Some(name) = url.trim_end_matches('/').rsplit(['/', ':']).next() {
            names.insert(name.trim_end_matches(".git").to_ascii_lowercase());
        }
    }
    names
}

/** Find the non-test entities a symbol reference names: by id ("symbol:LANG:..." or
 * "LANG:..."), else by qualified or bare name with the resolver's matching
 * Input
    - inventory: &Inventory - inventory
    - token: &str - reference
 * Output
    - Vec<usize> entity indices
*/
fn resolve_symbol(inventory: &Inventory, token: &str) -> Vec<usize> {
    let graph = &inventory.graph;
    let normalized = token.replace("::", ".");
    let by_id = normalized.strip_prefix("symbol:").unwrap_or(&normalized);
    graph
        .entities
        .iter()
        .enumerate()
        .filter(|(_, entity)| !entity.test)
        .filter(|(_, entity)| {
            let symbol = Graph::symbol(&inventory.snapshot, entity);
            if by_id.contains(':') {
                entity.id.strip_prefix("symbol:") == Some(by_id)
            } else {
                matches_target(&symbol.name, &symbol.qualified, by_id)
            }
        })
        .map(|(index, _)| index)
        .collect()
}

/** Find the files a path reference names: the exact path, or files ending in "/" + reference
 * Input
    - inventory: &Inventory - inventory
    - token: &str - reference
 * Output
    - Vec<usize> file indices
*/
fn resolve_file(inventory: &Inventory, token: &str) -> Vec<usize> {
    let token = token.trim_start_matches("./");
    inventory
        .snapshot
        .files
        .iter()
        .enumerate()
        .filter(|(_, file)| file.path == token || file.path.ends_with(&format!("/{token}")))
        .map(|(index, _)| index)
        .collect()
}

/** Find the modules a module reference names: by id ("module:LANG:NAME" or "LANG:NAME") or name
 * Input
    - inventory: &Inventory - inventory
    - token: &str - reference
 * Output
    - Vec<usize> module indices
*/
fn resolve_module(inventory: &Inventory, token: &str) -> Vec<usize> {
    let token = token.strip_prefix("module:").unwrap_or(token);
    inventory
        .graph
        .modules
        .iter()
        .enumerate()
        .filter(|(_, module)| module.files > 0)
        .filter(|(_, module)| {
            module.name == token || module.id.strip_prefix("module:") == Some(token)
        })
        .map(|(index, _)| index)
        .collect()
}

/** Build the AgentScript rule for an entity, when policies can target it unambiguously
 * Input
    - inventory: &Inventory - inventory
    - entity: usize - entity index
    - keyword: &str - "target" or "preserve"
 * Output
    - Result<String, String> the statement, or why there is none
*/
fn rule_for(inventory: &Inventory, entity: usize, keyword: &str) -> Result<String, String> {
    inventory
        .suggestion(&inventory.graph.entities[entity])
        .map(|rule| rule.replacen("preserve", keyword, 1))
        .map_err(String::from)
}

/** Plan a task: check that it is specified well enough, resolve its references against the
 * inventory, and derive the task contract (MUST_CHANGE, MUST_NOT_CHANGE, MAY_CHANGE,
 * REQUIRES_APPROVAL, TASK_SCOPE, EXPECTED_TESTS) together with the relevant zones, the active
 * policies, the checkpoint, earlier contracts, and candidate AgentScript; the status is
 * task_unrelated, task_needs_clarification, task_conflicts_with_policy, or planned
 * Input
    - task: &TaskInput - task
    - task_digest: &str - digest of the task file
    - checkpoint: &str - checkpoint the contract binds to
 * Output
    - Result<Value, String> the plan
    - Error if Crane is not initialized, the checkpoint is unusable, or discovery fails
*/
pub(crate) fn plan(task: &TaskInput, task_digest: &str, checkpoint: &str) -> Result<Value, String> {
    let stored = load_checkpoint(checkpoint)?;
    let commit = checkpoint_commit(checkpoint, &stored.name, &stored.commit)?;
    let zones = inspect()?;
    let inventory = &zones.inventory;
    let graph = &inventory.graph;
    let mut clarifications: Vec<Value> = Vec::new();
    let mut clarify = |field: &str, message: String| {
        clarifications.push(json!({"field": field, "message": message}))
    };

    // Specification
    if !valid_task_id(&task.task_id) {
        clarify(
            "task_id",
            "task_id is missing or invalid: use 1 to 64 letters, digits, '.', '_', or '-'".into(),
        );
    }
    if task.title.is_empty() {
        clarify(
            "title",
            "title is empty: give a one-line summary of the change".into(),
        );
    }
    if task.description.is_empty() {
        clarify(
            "description",
            "description is empty: say what must change and why".into(),
        );
    }
    if task.acceptance_criteria.is_empty() {
        clarify(
            "acceptance_criteria",
            "acceptance_criteria is empty: list the observable conditions that make the task done"
                .into(),
        );
    }
    let names = repository_names(inventory);
    let shown = names
        .iter()
        .filter(|name| *name != ".")
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if task.repositories.is_empty() {
        clarify(
            "repositories",
            format!("repositories is empty: name the repository to change (this one is {shown})"),
        );
    }
    let related = task.repositories.is_empty()
        || task.repositories.iter().any(|repository| {
            let name = repository
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(repository);
            names.contains(&name.trim_end_matches(".git").to_ascii_lowercase())
        });
    for text in std::iter::once(&task.title)
        .chain(std::iter::once(&task.description))
        .chain(task.acceptance_criteria.iter())
    {
        let lower = text.to_ascii_lowercase();
        if let Some(phrase) = BROAD.iter().find(|phrase| lower.contains(*phrase)) {
            clarify("description", format!("the task asks for changes '{phrase}', which no task contract grants: list the symbols or modules in scope instead"));
            break;
        }
    }

    // References
    let mut must_change: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut must_not: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut scope_modules: BTreeSet<usize> = BTreeSet::new();
    let mut scope_files: BTreeSet<usize> = BTreeSet::new();
    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    for mention in mentions(task) {
        let quoted = format!("`{}` ({})", mention.token, mention.source);
        if mention.source == "references.modules" {
            let modules = resolve_module(inventory, &mention.token);
            if modules.is_empty() {
                clarify(
                    mention.source,
                    format!("module {quoted} does not exist in this repository"),
                );
            }
            for module in modules {
                resolved.push(json!({"mention": mention.token, "source": mention.source, "resolved": graph.modules[module].id}));
                scope_modules.insert(module);
            }
            continue;
        }
        let path_like = mention.token.contains('/') || mention.source == "references.files";
        if path_like {
            let files = resolve_file(inventory, &mention.token);
            match files.len() {
                0 if mention.explicit => clarify(
                    mention.source,
                    format!("file {quoted} does not exist in this repository"),
                ),
                0 => unresolved.push(json!({"mention": mention.token, "source": mention.source})),
                _ => {
                    for file in files {
                        resolved.push(json!({"mention": mention.token, "source": mention.source, "resolved": format!("file:{}", inventory.snapshot.files[file].path)}));
                        scope_files.insert(file);
                        if graph.files[file].module != usize::MAX {
                            scope_modules.insert(graph.files[file].module);
                        }
                    }
                }
            }
            continue;
        }
        let entities = resolve_symbol(inventory, &mention.token);
        match entities.as_slice() {
            [] if mention.explicit => clarify(
                mention.source,
                format!("symbol {quoted} does not exist in this repository"),
            ),
            [] => unresolved.push(json!({"mention": mention.token, "source": mention.source})),
            [entity] => {
                let entity = *entity;
                let id = graph.entities[entity].id.clone();
                resolved.push(json!({"mention": mention.token, "source": mention.source, "resolved": id, "negated": mention.negated}));
                let symbol = Graph::symbol(&inventory.snapshot, &graph.entities[entity]);
                let reason = format!(
                    "{} in {}",
                    if mention.explicit { "listed" } else { "named" },
                    mention.source
                );
                if mention.negated {
                    must_not.entry(entity).or_default().push(reason);
                } else if mention.explicit || symbol.kind.callable() {
                    must_change.entry(entity).or_default().push(reason);
                    scope_modules.insert(graph.entities[entity].module);
                } else {
                    // A type named in prose sets scope, it does not have to change
                    scope_modules.insert(graph.entities[entity].module);
                }
            }
            many => clarify(
                mention.source,
                format!(
                    "symbol {quoted} is ambiguous: it matches {}; use a qualified name or an id",
                    many.iter()
                        .map(|entity| graph.entities[*entity].id.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
        }
    }
    if must_change.is_empty() && related {
        clarify("references.symbols", "no function or class that must change could be identified: list it in references.symbols or write it as `Type.method` in the description".into());
    }
    for entity in must_change.keys() {
        if must_not.contains_key(entity) {
            clarify(
                "description",
                format!(
                    "{} is named both as something to change and something to keep unchanged",
                    graph.entities[*entity].id
                ),
            );
        }
    }

    // Policies and zones
    let mut conflicts = Vec::new();
    let mut approvals = Vec::new();
    let zone_names = |zones: &Zones, indices: &[usize]| {
        indices
            .iter()
            .map(|index| zones.resolution.zones[*index].zone.zone_id.clone())
            .collect::<Vec<_>>()
    };
    for entity in must_change.keys() {
        let id = &graph.entities[*entity].id;
        for tag in &graph.entities[*entity].contracts {
            if let Some(policy) = tag.strip_suffix(":preserve") {
                conflicts.push(json!({
                    "kind": "permanent_policy",
                    "entity": id,
                    "policy": policy,
                    "message": format!("policy {policy} permanently preserves {id}; the task cannot change it unless a human changes that policy"),
                }));
            }
        }
        if let Some(effective) = zones.resolution.entities.get(entity) {
            let names = zone_names(&zones, &effective.zones);
            if effective.autonomy == Autonomy::Observe {
                conflicts.push(json!({
                    "kind": "zone",
                    "entity": id,
                    "zones": names,
                    "message": format!("zones {} let agents only observe {id} ({}, {})", names.join(", "), effective.criticality.name(), effective.state.name()),
                }));
            } else if effective.autonomy == Autonomy::Assisted {
                approvals.push(json!({
                    "entity": id,
                    "zones": names,
                    "reason": format!("zones {} ({}) require human approval of changes", names.join(", "), effective.criticality.name()),
                }));
            }
        }
    }
    let services = scope_modules
        .iter()
        .map(|module| graph.modules[*module].service)
        .collect::<BTreeSet<_>>();
    if services.len() > 1 {
        approvals.push(json!({
            "scope": "services",
            "services": services.iter().map(|service| graph.services[*service].id.clone()).collect::<Vec<_>>(),
            "reason": format!("the task spans {} services, so a human must approve the cross-service change", services.len()),
        }));
    }

    // Scope, neighbors, and permanent policies
    for (entity, entry) in graph.entities.iter().enumerate() {
        if entry.test || must_change.contains_key(&entity) {
            continue;
        }
        let near = must_change.keys().any(|target| {
            graph.entities[*target].callers.contains(&entity)
                || graph.entities[*target].callees.contains(&entity)
        });
        let in_scope = scope_modules.contains(&entry.module);
        if let Some(policy) = entry
            .contracts
            .iter()
            .find_map(|tag| tag.strip_suffix(":preserve"))
        {
            if near || in_scope {
                must_not
                    .entry(entity)
                    .or_default()
                    .push(format!("permanent policy {policy}"));
            }
        } else if near && !in_scope && Graph::symbol(&inventory.snapshot, entry).kind.callable() {
            must_not
                .entry(entity)
                .or_default()
                .push("outside the task scope (calls or is called by a target)".into());
        }
    }
    let may_change = graph
        .entities
        .iter()
        .enumerate()
        .filter(|(entity, entry)| {
            !entry.test
                && scope_modules.contains(&entry.module)
                && !must_change.contains_key(entity)
                && !must_not.contains_key(entity)
        })
        .map(|(_, entry)| entry.id.clone())
        .collect::<Vec<_>>();
    let mut test_files = BTreeSet::new();
    let mut expected_tests = Vec::new();
    for entity in must_change.keys() {
        let entry = &graph.entities[*entity];
        for test in &entry.tested_by {
            expected_tests.push(json!({"test": test, "covers": entry.id}));
        }
        for file in &graph.files[entry.file].tested_by {
            test_files.insert(*file);
        }
        if entry.tested_by.is_empty() {
            expected_tests
                .push(json!({"add_test_for": entry.id, "reason": "no existing test calls it"}));
        }
    }
    for file in &test_files {
        expected_tests
            .push(json!({"test_file": inventory.snapshot.files[*file].path, "run": true}));
    }
    let task_entities = must_change
        .keys()
        .chain(must_not.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let relevant_zones = zones
        .resolution
        .zones
        .iter()
        .filter_map(|result| {
            let touched = result
                .entities
                .iter()
                .filter(|entity| {
                    task_entities.contains(*entity)
                        || scope_modules.contains(&graph.entities[**entity].module)
                })
                .count();
            (touched > 0).then(|| {
                json!({
                    "zone_id": result.zone.zone_id,
                    "criticality": result.zone.criticality.name(),
                    "effective_autonomy": result.autonomy.name(),
                    "effective_safety_state": result.state.name(),
                    "entities_in_task": touched,
                })
            })
        })
        .collect::<Vec<_>>();

    // Status and AgentScript
    let status = if !task.repositories.is_empty() && !related {
        "task_unrelated"
    } else if !clarifications.is_empty() {
        "task_needs_clarification"
    } else if !conflicts.is_empty() {
        "task_conflicts_with_policy"
    } else {
        "planned"
    };
    let policy_name = format!(
        "task_{}",
        task.task_id
            .to_ascii_lowercase()
            .chars()
            .map(|character| if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            })
            .collect::<String>()
    );
    let entry = |entity: usize, keyword: &str, reasons: &[String]| {
        let symbol = Graph::symbol(&inventory.snapshot, &graph.entities[entity]);
        let rule = rule_for(inventory, entity, keyword);
        json!({
            "id": graph.entities[entity].id,
            "qualified": symbol.qualified,
            "kind": symbol.kind.name(),
            "file": inventory.snapshot.files[graph.entities[entity].file].path,
            "because": reasons,
            "rule": rule.as_ref().ok(),
            "enforceable_note": rule.err(),
        })
    };
    let must_change_json = must_change
        .iter()
        .map(|(entity, reasons)| entry(*entity, "target", reasons))
        .collect::<Vec<_>>();
    let must_not_json = must_not
        .iter()
        .map(|(entity, reasons)| entry(*entity, "preserve", reasons))
        .collect::<Vec<_>>();
    let mut rules = Vec::new();
    for item in &must_change_json {
        if let Some(rule) = item["rule"].as_str() {
            rules.push(rule.to_string());
        }
    }
    let targets = rules.len();
    for item in &must_not_json {
        let permanent = item["because"].as_array().is_some_and(|reasons| {
            reasons.iter().any(|reason| {
                reason
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("permanent policy")
            })
        });
        if let Some(rule) = item["rule"].as_str().filter(|_| !permanent) {
            if !rules.iter().any(|existing| existing == rule) {
                rules.push(rule.to_string());
            }
        }
    }
    let agentscript = if status == "planned" && targets > 0 {
        let mut text = format!("policy {policy_name} {{\n    checkpoint {checkpoint};\n");
        for rule in &rules {
            text.push_str(&format!("    {rule}\n"));
        }
        text.push_str("}\n");
        parse(&text).map_err(|error| format!("generated task contract is invalid: {error}"))?;
        Some(text)
    } else {
        None
    };
    let agentscript_note = match (status, &agentscript) {
        ("planned", None) => Some("no MUST_CHANGE symbol is in a language policies can target yet; the plan stands, but no AgentScript can enforce it"),
        (_, None) => Some("no contract is proposed until the status is planned"),
        _ => None,
    };

    // Earlier contracts for this task
    let mut previous = Vec::new();
    for id in session_ids()? {
        if let Ok(Some(session)) = ContractSession::load(&id) {
            if session.describe()["task_id"] == task.task_id.as_str() {
                previous.push(json!({
                    "session_id": id,
                    "lifecycle": session.describe()["lifecycle"],
                    "final_status": read_attestation(&id).ok().flatten().map(|attestation| attestation["final_status"].clone()),
                }));
            }
        }
    }
    if let Ok(proposal) = crate::proposals::store::Proposal::load(&policy_name) {
        previous.push(json!({"proposal": policy_name, "status": proposal.status(), "policy_digest": proposal.digest()}));
    }

    Ok(json!({
        "task_plan_format": PLAN_FORMAT,
        "task_id": task.task_id,
        "status": status,
        "clarifications": clarifications,
        "conflicts": conflicts,
        "task": task.to_json(),
        "task_digest": task_digest,
        "repository": {"names": names, "related": related},
        "checkpoint": {"name": checkpoint, "commit": commit},
        "policy_packs": {
            "contract_version": inventory.contract_version,
            "policies": inventory.contracts.iter().map(|clause| clause.policy_id.clone()).collect::<BTreeSet<_>>(),
        },
        "zone_set_version": zones.version,
        "zones": relevant_zones,
        "contract": {
            "MUST_CHANGE": must_change_json,
            "MUST_NOT_CHANGE": must_not_json,
            "MAY_CHANGE": may_change,
            "REQUIRES_APPROVAL": approvals,
            "TASK_SCOPE": {
                "repositories": task.repositories,
                "services": services.iter().map(|service| graph.services[*service].id.clone()).collect::<Vec<_>>(),
                "modules": scope_modules.iter().map(|module| graph.modules[*module].id.clone()).collect::<Vec<_>>(),
                "files": scope_files.iter().chain(must_change.keys().map(|entity| &graph.entities[*entity].file)).map(|file| inventory.snapshot.files[*file].path.clone()).collect::<BTreeSet<_>>(),
            },
            "EXPECTED_TESTS": expected_tests,
        },
        "policy_name": policy_name,
        "agentscript": agentscript,
        "agentscript_note": agentscript_note,
        "mentions": {"resolved": resolved, "unresolved": unresolved},
        "previous_contracts": previous,
        "active": false,
    }))
}

/** Plan a task again and return what a task-contract proposal holds, for crane task plan
 * --propose and for regenerating such a proposal
 * Input
    - task_id: &str - task id
    - name: &str - proposal and policy name
    - checkpoint: &str - checkpoint the contract binds to
 * Output
    - Result<(Value, String, Value, Value), String> candidates, policy text, zone suggestions
      (none), and source state
    - Error if the task is not planned or yields no AgentScript
*/
pub(crate) fn proposal_content(
    task_id: &str,
    name: &str,
    checkpoint: &str,
) -> Result<(Value, String, Value, Value), String> {
    let (task, digest) = load(task_id)?;
    let plan = plan(&task, &digest, checkpoint)?;
    if plan["status"] != "planned" {
        return Err(format!(
            "task {task_id} is {}; no contract can be proposed",
            plan["status"].as_str().unwrap_or_default()
        ));
    }
    let script = plan["agentscript"].as_str().ok_or_else(|| {
        plan["agentscript_note"]
            .as_str()
            .unwrap_or("no AgentScript")
            .to_string()
    })?;
    // The proposal may carry a versioned name (task_ID_vN), so the policy is renamed to match
    let planned = format!(
        "policy {} {{",
        plan["policy_name"].as_str().unwrap_or_default()
    );
    let policy = script.replacen(&planned, &format!("policy {name} {{"), 1);
    parse(&policy).map_err(|error| format!("task contract {name} is invalid: {error}"))?;
    let candidates = json!({
        "MUST_CHANGE": plan["contract"]["MUST_CHANGE"],
        "MUST_NOT_CHANGE": plan["contract"]["MUST_NOT_CHANGE"],
        "REQUIRES_APPROVAL": plan["contract"]["REQUIRES_APPROVAL"],
        "TASK_SCOPE": plan["contract"]["TASK_SCOPE"],
        "EXPECTED_TESTS": plan["contract"]["EXPECTED_TESTS"],
    });
    let source = json!({
        "task_id": task_id,
        "task_digest": digest,
        "contract_version": plan["policy_packs"]["contract_version"],
        "zone_set_version": plan["zone_set_version"],
        "generated_in_agent_environment": crate::proposals::store::agent_environment(),
    });
    Ok((candidates, policy, json!([]), source))
}

/** Render a plan for a terminal
 * Input
    - plan: &Value - plan from plan()
 * Output
    - String ending in a newline
*/
pub(crate) fn render(plan: &Value) -> String {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    let mut out = format!(
        "Task {}: {}\nStatus: {}\n",
        text(&plan["task_id"]),
        text(&plan["task"]["title"]),
        text(&plan["status"])
    );
    for item in plan["clarifications"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  needs clarification [{}]: {}\n",
            text(&item["field"]),
            text(&item["message"])
        ));
    }
    for item in plan["conflicts"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  conflict [{}]: {}\n",
            text(&item["kind"]),
            text(&item["message"])
        ));
    }
    let contract = &plan["contract"];
    for (section, keyword) in [("MUST_CHANGE", "target"), ("MUST_NOT_CHANGE", "preserve")] {
        let items = contract[section].as_array().cloned().unwrap_or_default();
        out.push_str(&format!("{section} ({}):\n", items.len()));
        for item in items {
            out.push_str(&format!(
                "  {} [{}] ({}){}\n",
                text(&item["qualified"]),
                text(&item["id"]),
                item["because"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(text)
                    .collect::<Vec<_>>()
                    .join("; "),
                item["rule"].as_str().map_or_else(
                    || format!(" - no {keyword} rule: {}", text(&item["enforceable_note"])),
                    |_| String::new()
                )
            ));
        }
    }
    out.push_str(&format!(
        "MAY_CHANGE ({} symbols in scope modules)\n",
        contract["MAY_CHANGE"].as_array().map_or(0, Vec::len)
    ));
    out.push_str(&format!(
        "REQUIRES_APPROVAL ({}):\n",
        contract["REQUIRES_APPROVAL"].as_array().map_or(0, Vec::len)
    ));
    for item in contract["REQUIRES_APPROVAL"]
        .as_array()
        .into_iter()
        .flatten()
    {
        out.push_str(&format!("  {}\n", text(&item["reason"])));
    }
    let scope = &contract["TASK_SCOPE"];
    let join = |value: &Value| {
        value
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect::<Vec<_>>()
            .join(", ")
    };
    out.push_str(&format!(
        "TASK_SCOPE: services [{}]; modules [{}]\n",
        join(&scope["services"]),
        join(&scope["modules"])
    ));
    out.push_str("EXPECTED_TESTS:\n");
    for item in contract["EXPECTED_TESTS"].as_array().into_iter().flatten() {
        if let Some(test) = item["test"].as_str() {
            out.push_str(&format!(
                "  run {test} (covers {})\n",
                text(&item["covers"])
            ));
        } else if let Some(file) = item["test_file"].as_str() {
            out.push_str(&format!("  run tests in {file}\n"));
        } else {
            out.push_str(&format!(
                "  add a test for {} ({})\n",
                text(&item["add_test_for"]),
                text(&item["reason"])
            ));
        }
    }
    let zones = plan["zones"].as_array().cloned().unwrap_or_default();
    if !zones.is_empty() {
        out.push_str("Zones:\n");
        for zone in zones {
            out.push_str(&format!(
                "  {} ({}, autonomy {}, {})\n",
                text(&zone["zone_id"]),
                text(&zone["criticality"]),
                text(&zone["effective_autonomy"]),
                text(&zone["effective_safety_state"])
            ));
        }
    }
    match plan["agentscript"].as_str() {
        Some(script) => {
            out.push_str("Proposed task contract (not active; 'crane task plan ID --propose' stores it for approval):\n");
            for line in script.lines() {
                out.push_str(&format!("  {line}\n"));
            }
        }
        None => out.push_str(&format!(
            "No task contract: {}\n",
            text(&plan["agentscript_note"])
        )),
    }
    out
}
