use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fs;
use std::path::Path;

use super::graph::Graph;
use super::index::Snapshot;
use crate::ir::{checkpoint_commit, ContractSet};
use crate::model::{ItemKind, Scope};
use crate::repository::{git, git_raw, load_checkpoint};
use crate::resolver::matches_target;
use crate::scope::{folder_of, folder_prefix};

/** Places GitHub and GitLab look for a CODEOWNERS file, in priority order */
const CODEOWNERS_PATHS: &[&str] = &[
    ".github/CODEOWNERS",
    "CODEOWNERS",
    "docs/CODEOWNERS",
    ".gitlab/CODEOWNERS",
];

/** Name words that suggest business-critical or security-sensitive code */
const SENSITIVE: &[&str] = &[
    "payment",
    "pay",
    "charge",
    "billing",
    "invoice",
    "refund",
    "transfer",
    "balance",
    "ledger",
    "settle",
    "settlement",
    "wallet",
    "account",
    "tax",
    "price",
    "fee",
    "auth",
    "authorize",
    "authenticate",
    "authorization",
    "authentication",
    "login",
    "password",
    "credential",
    "token",
    "secret",
    "crypto",
    "encrypt",
    "decrypt",
    "signature",
    "permission",
    "role",
    "admin",
    "security",
    "migration",
    "migrate",
];

/** Called names that suggest persistence side effects */
const PERSISTENCE: &[&str] = &[
    "executeQuery",
    "executeUpdate",
    "executemany",
    "rollback",
    "persist",
    "upsert",
    "QueryRow",
    "QueryContext",
    "ExecContext",
    "saveAndFlush",
    "bulk_create",
    "raw_sql",
];

/** Called names that suggest network side effects */
const NETWORK: &[&str] = &[
    "fetch",
    "urlopen",
    "request",
    "Do",
    "Post",
    "Get",
    "post",
    "send",
    "sendRequest",
    "connect",
    "axios",
    "http",
    "grpc",
];

/** Folder names that hold entry points */
const ENTRY_FOLDERS: &[&str] = &[
    "controller",
    "controllers",
    "handler",
    "handlers",
    "routes",
    "api",
    "endpoints",
    "cmd",
];

/** Score at which a symbol becomes a critical candidate */
pub(crate) const CRITICAL_SCORE: u32 = 4;

/** One existing contract clause and the inventory entities it covers
 * Fields
    - policy_id: String - policy name
    - rule: &'static str - "preserve" or "target"
    - kind: ItemKind - targeted item kind
    - target: String - qualified target
    - scope: Scope - covered scope
    - checkpoint: String - checkpoint name
    - status: &'static str - "resolved", "missing", or "ambiguous" in the current worktree
    - anchors: Vec<usize> - entities the target names
    - covered: usize - entities the clause covers (flow scope is approximated from the call graph)
*/
pub(crate) struct ClauseCoverage {
    pub(crate) policy_id: String,
    pub(crate) rule: &'static str,
    pub(crate) kind: ItemKind,
    pub(crate) target: String,
    pub(crate) scope: Scope,
    pub(crate) checkpoint: String,
    pub(crate) status: &'static str,
    pub(crate) anchors: Vec<usize>,
    pub(crate) covered: usize,
}

/** A checkpoint and how it relates to the policies and the current worktree
 * Fields
    - name: String - checkpoint name
    - commit: String - recorded commit
    - status: String - "valid" or why the checkpoint cannot be bound
    - exists: bool - whether the commit is available locally
    - policies: Vec<String> - policies using it
    - commits_since: Option<usize> - commits from it to HEAD
    - changed_files: Vec<String> - files that differ between it and the worktree
    - covered_changed: usize - contract-covered entities in those files
*/
pub(crate) struct CheckpointInfo {
    pub(crate) name: String,
    pub(crate) commit: String,
    pub(crate) status: String,
    pub(crate) exists: bool,
    pub(crate) policies: Vec<String>,
    pub(crate) commits_since: Option<usize>,
    pub(crate) changed_files: Vec<String>,
    pub(crate) covered_changed: usize,
}

/** Match text against a glob where "*" and "?" stay inside one path segment and "**" crosses
 * segments ("**" followed by "/" also matches zero folders)
 * Input
    - pattern: &[u8] - glob
    - text: &[u8] - path or name
 * Output
    - bool
*/
pub(crate) fn glob(pattern: &[u8], text: &[u8]) -> bool {
    match pattern {
        [] => text.is_empty(),
        [b'*', b'*', rest @ ..] => {
            let (rest, at_boundary) = match rest {
                [b'/', after @ ..] => (after, true),
                _ => (rest, false),
            };
            (0..=text.len()).any(|index| {
                (!at_boundary || index == 0 || text[index - 1] == b'/')
                    && glob(rest, &text[index..])
            })
        }
        [b'*', rest @ ..] => {
            for index in 0..=text.len() {
                if glob(rest, &text[index..]) {
                    return true;
                }
                if index < text.len() && text[index] == b'/' {
                    return false;
                }
            }
            false
        }
        [b'?', rest @ ..] => !text.is_empty() && text[0] != b'/' && glob(rest, &text[1..]),
        [character, rest @ ..] => text.first() == Some(character) && glob(rest, &text[1..]),
    }
}

/** Check whether a CODEOWNERS pattern applies to a file, with gitignore-style semantics: a
 * pattern with a leading or inner "/" is anchored at the root, otherwise it matches a name at any
 * depth; a trailing "/" matches folders only; a pattern matching a folder covers its contents
 * Input
    - pattern: &str - CODEOWNERS pattern
    - path: &str - repository-relative file path
 * Output
    - bool
*/
pub(crate) fn codeowners_match(pattern: &str, path: &str) -> bool {
    let folders_only = pattern.ends_with('/');
    let core = pattern.trim_start_matches('/').trim_end_matches('/');
    let anchored = pattern.starts_with('/') || core.contains('/');
    if core.is_empty() {
        return true;
    }
    let mut candidates = path
        .match_indices('/')
        .map(|(index, _)| &path[..index])
        .collect::<Vec<_>>();
    candidates.push(path);
    candidates.into_iter().any(|candidate| {
        if folders_only && candidate == path {
            return false;
        }
        let subject = if anchored {
            candidate
        } else {
            candidate.rsplit('/').next().unwrap_or(candidate)
        };
        glob(core.as_bytes(), subject.as_bytes())
    })
}

/** Read the repository's CODEOWNERS file (the first that exists) and give every file its owners,
 * the last matching rule winning as on GitHub
 * Input
    - root: &Path - repository root
    - snapshot: &Snapshot - files
    - graph: &mut Graph - graph whose files receive owners
 * Output
    - Option<(String, usize)> the CODEOWNERS path and its rule count, None if there is none
*/
pub(crate) fn ownership(
    root: &Path,
    snapshot: &Snapshot,
    graph: &mut Graph,
) -> Option<(String, usize)> {
    let (source, content) = CODEOWNERS_PATHS
        .iter()
        .find_map(|path| Some((*path, fs::read_to_string(root.join(path)).ok()?)))?;
    let rules = content
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pattern = words.next()?.to_string();
            Some((pattern, words.map(String::from).collect::<Vec<_>>()))
        })
        .collect::<Vec<_>>();
    for (index, file) in snapshot.files.iter().enumerate() {
        if let Some((_, owners)) = rules
            .iter()
            .rev()
            .find(|(pattern, _)| codeowners_match(pattern, &file.path))
        {
            graph.files[index].owners = owners.clone();
        }
    }
    Some((source.to_string(), rules.len()))
}

/** Check whether a symbol's item kind is the kind a clause targets (variables and their data
 * resolve to the same declarations)
 * Input
    - clause: ItemKind - kind in the rule
    - symbol: Option<ItemKind> - kind of the symbol
 * Output
    - bool
*/
fn same_item_kind(clause: ItemKind, symbol: Option<ItemKind>) -> bool {
    let value = |kind: ItemKind| match kind {
        ItemKind::Data => ItemKind::Variable,
        other => other,
    };
    symbol.is_some_and(|symbol| value(symbol) == value(clause))
}

/** Map every existing contract clause onto the inventory: find the entities its target names
 * (with the resolver's own matching, in enforceable languages only), expand them by the clause's
 * scope, and tag every covered entity with "POLICY:RULE"; this only describes the contracts, it
 * never changes or enforces them
 * Input
    - set: &ContractSet - compiled policies
    - snapshot: &Snapshot - files and outlines
    - graph: &mut Graph - graph whose entities receive contract tags
 * Output
    - Vec<ClauseCoverage> in policy order
*/
pub(crate) fn contracts(
    set: &ContractSet,
    snapshot: &Snapshot,
    graph: &mut Graph,
) -> Vec<ClauseCoverage> {
    let mut coverage = Vec::new();
    for (_, contract, clause) in set.clauses() {
        let anchors = graph
            .entities
            .iter()
            .enumerate()
            .filter(|(_, entity)| {
                let symbol = Graph::symbol(snapshot, entity);
                symbol.targetable
                    && same_item_kind(clause.kind, symbol.kind.item_kind())
                    && matches_target(&symbol.name, &symbol.qualified, &clause.target)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let path_of = |index: usize| snapshot.files[graph.entities[index].file].path.as_str();
        let covered: BTreeSet<usize> = match clause.scope {
            Scope::Block => anchors.iter().copied().collect(),
            Scope::File => {
                let files = anchors
                    .iter()
                    .map(|anchor| graph.entities[*anchor].file)
                    .collect::<HashSet<_>>();
                (0..graph.entities.len())
                    .filter(|index| files.contains(&graph.entities[*index].file))
                    .collect()
            }
            Scope::Folder => {
                let prefixes = anchors
                    .iter()
                    .map(|anchor| folder_prefix(&folder_of(path_of(*anchor))))
                    .collect::<Vec<_>>();
                (0..graph.entities.len())
                    .filter(|index| {
                        prefixes
                            .iter()
                            .any(|prefix| path_of(*index).starts_with(prefix.as_str()))
                    })
                    .collect()
            }
            Scope::Flow => {
                let mut seen = anchors.iter().copied().collect::<BTreeSet<_>>();
                let mut queue = anchors.iter().copied().collect::<VecDeque<_>>();
                while let Some(current) = queue.pop_front() {
                    for callee in &graph.entities[current].callees {
                        if seen.insert(*callee) {
                            queue.push_back(*callee);
                        }
                    }
                }
                for anchor in &anchors {
                    seen.extend(graph.entities[*anchor].callers.iter().copied());
                }
                seen
            }
            Scope::All => (0..graph.entities.len()).collect(),
        };
        let label = format!("{}:{}", contract.policy_id, clause.keyword());
        for index in &covered {
            let tags = &mut graph.entities[*index].contracts;
            if !tags.contains(&label) {
                tags.push(label.clone());
            }
        }
        coverage.push(ClauseCoverage {
            policy_id: contract.policy_id.clone(),
            rule: clause.keyword(),
            kind: clause.kind,
            target: clause.target.clone(),
            scope: clause.scope,
            checkpoint: contract.checkpoint.clone(),
            status: match anchors.len() {
                0 => "missing",
                1 => "resolved",
                _ => "ambiguous",
            },
            anchors,
            covered: covered.len(),
        });
    }
    coverage
}

/** Describe every checkpoint in .crane/checkpoints: whether it can be bound, whether its commit
 * exists, which policies use it, how far HEAD has moved since, which files differ from it now,
 * and how many contract-covered entities sit in those files
 * Input
    - crane: &Path - the .crane directory
    - set: &ContractSet - compiled policies
    - snapshot: &Snapshot - files
    - graph: &Graph - graph with contract tags
 * Output
    - Vec<CheckpointInfo> sorted by name
*/
pub(crate) fn checkpoints(
    crane: &Path,
    set: &ContractSet,
    snapshot: &Snapshot,
    graph: &Graph,
) -> Vec<CheckpointInfo> {
    let mut names = fs::read_dir(crane.join("checkpoints"))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(".json").map(String::from)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let policies = set
                .contracts
                .iter()
                .filter(|contract| contract.checkpoint == name)
                .map(|contract| contract.policy_id.clone())
                .collect::<Vec<_>>();
            let (commit, status) = match load_checkpoint(&name) {
                Ok(checkpoint) => {
                    let status = checkpoint_commit(&name, &checkpoint.name, &checkpoint.commit)
                        .map(|_| "valid".to_string())
                        .unwrap_or_else(|error| error);
                    (checkpoint.commit, status)
                }
                Err(error) => (String::new(), error),
            };
            let exists = !commit.is_empty()
                && git(&["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_ok();
            let commits_since = exists
                .then(|| git(&["rev-list", "--count", &format!("{commit}..HEAD")]).ok())
                .flatten()
                .and_then(|count| count.parse().ok());
            let changed_files = if exists {
                git_raw(&["diff", "--name-only", &commit, "--"])
                    .map(|output| output.lines().map(String::from).collect::<Vec<_>>())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let changed = changed_files.iter().collect::<HashSet<_>>();
            let covered_changed = graph
                .entities
                .iter()
                .filter(|entity| {
                    changed.contains(&snapshot.files[entity.file].path)
                        && entity.contracts.iter().any(|tag| {
                            policies
                                .iter()
                                .any(|policy| tag.starts_with(&format!("{policy}:")))
                        })
                })
                .count();
            CheckpointInfo {
                name,
                commit,
                status,
                exists,
                policies,
                commits_since,
                changed_files,
                covered_changed,
            }
        })
        .collect()
}

/** Split an identifier or path into lowercase words at case changes, digits, and punctuation
 * Input
    - text: &str - identifier or path
 * Output
    - Vec<String>
*/
pub(crate) fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for character in text.chars() {
        if !character.is_ascii_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        if character.is_ascii_uppercase() && previous_lower && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        previous_lower = character.is_ascii_lowercase() || character.is_ascii_digit();
        current.push(character.to_ascii_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/** Find the first sensitive word in a list of words, accepting plurals
 * Input
    - words: &[String] - lowercase words
 * Output
    - Option<&'static str> the matched keyword
*/
fn sensitive(words: &[String]) -> Option<&'static str> {
    words.iter().find_map(|word| {
        SENSITIVE.iter().copied().find(|keyword| {
            word == keyword
                || word.strip_suffix('s') == Some(keyword)
                || word.strip_suffix("es") == Some(keyword)
        })
    })
}

/** Score every non-test symbol with advisory risk signals: a sensitive name (3) or path (1), fan
 * in (1, or 2 from six callers), callers from two or more other modules (1), complexity (1, or 2
 * from fifteen loops and branches), size (1 from sixty lines), an entry point (1), persistence
 * calls (2), network calls (1), and, for code with other signals, no tests (1)
 * Input
    - snapshot: &Snapshot - files and outlines
    - graph: &mut Graph - graph whose entities receive signals and scores
 * Output
    - None
*/
pub(crate) fn assess_risk(snapshot: &Snapshot, graph: &mut Graph) {
    for index in 0..graph.entities.len() {
        let entity = &graph.entities[index];
        if entity.test {
            continue;
        }
        let symbol = Graph::symbol(snapshot, entity);
        if symbol.kind.item_kind().is_none() {
            continue;
        }
        let path = &snapshot.files[entity.file].path;
        let mut signals: Vec<(String, u32)> = Vec::new();
        let mut add = |signal: String, weight: u32| signals.push((signal, weight));
        if let Some(keyword) = sensitive(&words(&symbol.qualified)) {
            add(format!("sensitive_name:{keyword}"), 3);
        } else if let Some(keyword) = sensitive(&words(&folder_of(path))) {
            add(format!("sensitive_path:{keyword}"), 1);
        }
        if symbol.kind.callable() {
            let callers = entity.callers.len();
            if callers >= 6 {
                add(format!("high_fan_in:{callers}"), 2);
            } else if callers >= 3 {
                add(format!("fan_in:{callers}"), 1);
            }
            let modules = entity
                .callers
                .iter()
                .map(|caller| graph.entities[*caller].module)
                .filter(|module| *module != entity.module)
                .collect::<HashSet<_>>()
                .len();
            if modules >= 2 {
                add(format!("cross_module_callers:{modules}"), 1);
            }
            let complexity = symbol.loops + symbol.branches;
            if complexity >= 15 {
                add(format!("complexity:{complexity}"), 2);
            } else if complexity >= 8 {
                add(format!("complexity:{complexity}"), 1);
            }
            let lines = symbol.end_line + 1 - symbol.start_line;
            if lines >= 60 {
                add(format!("size:{lines}"), 1);
            }
            let folders = words(&folder_of(path));
            if symbol.name == "main"
                || folders
                    .iter()
                    .any(|folder| ENTRY_FOLDERS.contains(&folder.as_str()))
            {
                add("entrypoint".into(), 1);
            }
            if symbol
                .calls
                .iter()
                .any(|call| PERSISTENCE.contains(&call.as_str()))
            {
                add("persistence".into(), 2);
            }
            if symbol
                .calls
                .iter()
                .any(|call| NETWORK.contains(&call.as_str()))
            {
                add("network".into(), 1);
            }
            if !signals.is_empty()
                && entity.tested_by.is_empty()
                && graph.files[entity.file].tested_by.is_empty()
            {
                signals.push(("untested".into(), 1));
            }
        }
        graph.entities[index].score = signals.iter().map(|(_, weight)| weight).sum();
        graph.entities[index].risk = signals.into_iter().map(|(signal, _)| signal).collect();
    }
}
