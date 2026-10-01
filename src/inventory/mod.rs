// Repository intelligence: an advisory, incrementally indexed semantic inventory of the
// repository (files, languages, modules, services, symbols, call and test relationships,
// ownership, existing contracts, and checkpoints). Nothing here is enforced.

pub(crate) mod governance;
pub(crate) mod graph;
pub(crate) mod index;
pub(crate) mod outline;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::ir::compile;
use crate::repository::git;
use governance::{CheckpointInfo, ClauseCoverage, CRITICAL_SCORE};
use graph::{Entity, Graph};
use index::Snapshot;

/** Version of the inventory JSON layout */
pub(crate) const INVENTORY_FORMAT: u64 = 1;

/** How a discovery run is performed
 * Fields
    - full: bool - ignore the cache and extract every file again
*/
pub(crate) struct Options {
    pub(crate) full: bool,
}

/** The result of a discovery run
 * Fields
    - root: PathBuf - repository root
    - head: Option<String> - HEAD commit, None for a repository without commits
    - snapshot: Snapshot - files and outlines
    - graph: Graph - entities and relationships
    - codeowners: Option<(String, usize)> - CODEOWNERS path and rule count
    - initialized: bool - whether Crane is initialized (contracts and the cache need .crane)
    - contracts: Vec<ClauseCoverage> - existing contract clauses and what they cover
    - malformed: Vec<String> - malformed policy messages
    - contract_version: Option<String> - version of the compiled contract set
    - checkpoints: Vec<CheckpointInfo> - checkpoints and their relationships
    - cache: Option<PathBuf> - cache file written by this run
    - warnings: Vec<String> - problems that did not stop discovery
*/
pub(crate) struct Inventory {
    pub(crate) root: PathBuf,
    pub(crate) head: Option<String>,
    pub(crate) snapshot: Snapshot,
    pub(crate) graph: Graph,
    pub(crate) codeowners: Option<(String, usize)>,
    pub(crate) initialized: bool,
    pub(crate) contracts: Vec<ClauseCoverage>,
    pub(crate) malformed: Vec<String>,
    pub(crate) contract_version: Option<String>,
    pub(crate) checkpoints: Vec<CheckpointInfo>,
    pub(crate) cache: Option<PathBuf>,
    pub(crate) warnings: Vec<String>,
}

/** Totals per language
 * Fields
    - files: usize - files
    - lines: usize - lines in parsed or counted files
    - symbols: usize - symbols
    - parsed: usize - files parsed without errors
    - partial: usize - files parsed with syntax errors (symbols still read)
    - enforceable: bool - whether policies can target it
*/
#[derive(Default)]
pub(crate) struct LanguageTotals {
    pub(crate) files: usize,
    pub(crate) lines: usize,
    pub(crate) symbols: usize,
    pub(crate) parsed: usize,
    pub(crate) partial: usize,
    pub(crate) enforceable: bool,
}

/** Coverage of the inventory by existing contracts
 * Fields
    - callables: usize - non-test functions and methods
    - callables_covered: usize - those covered by a clause
    - entities: usize - non-test symbols
    - entities_covered: usize - those covered by a clause
    - critical: usize - critical candidates
    - critical_covered: usize - those covered by a clause
*/
pub(crate) struct Coverage {
    pub(crate) callables: usize,
    pub(crate) callables_covered: usize,
    pub(crate) entities: usize,
    pub(crate) entities_covered: usize,
    pub(crate) critical: usize,
    pub(crate) critical_covered: usize,
}

/** Run discovery on the repository containing the current directory: snapshot its files
 * (reusing cached outlines by content id), build the graph (relinking only what changed), add
 * ownership, existing contracts, checkpoints, and risk, and finally store the cache when Crane
 * is initialized and something changed; discovery only reads the repository and writes its
 * own cache
 * Input
    - options: &Options - run options
 * Output
    - Result<Inventory, String>
    - Error if the current directory is not in a Git repository or git fails
*/
pub(crate) fn discover(options: &Options) -> Result<Inventory, String> {
    let root = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?);
    let crane = root.join(".crane");
    let initialized = crane.is_dir();
    let cache_directory = initialized.then(|| crane.join("runtime").join("inventory"));
    let snapshot = index::snapshot(&root, cache_directory.as_deref(), options.full)?;
    let mut graph = graph::build(&snapshot);
    let codeowners = governance::ownership(&root, &snapshot, &mut graph);
    let mut warnings = Vec::new();
    let mut contracts = Vec::new();
    let mut malformed = Vec::new();
    let mut contract_version = None;
    let mut checkpoints = Vec::new();
    if initialized {
        match compile() {
            Ok(set) => {
                contracts = governance::contracts(&set, &snapshot, &mut graph);
                checkpoints = governance::checkpoints(&crane, &set, &snapshot, &graph);
                malformed = set
                    .malformed
                    .iter()
                    .map(|policy| policy.message.clone())
                    .collect();
                contract_version = Some(set.version.clone());
            }
            Err(error) => warnings.push(format!("policies could not be compiled: {error}")),
        }
    }
    governance::assess_risk(&snapshot, &mut graph);
    let unchanged = snapshot.links.is_some() && snapshot.parsed == 0 && snapshot.changed.is_empty();
    let cache = match &cache_directory {
        Some(directory) if unchanged => Some(directory.join("index.json")),
        Some(directory) => match index::save(directory, &snapshot, &graph.links) {
            Ok(path) => Some(path),
            Err(error) => {
                warnings.push(format!("the inventory cache could not be written: {error}"));
                None
            }
        },
        None => None,
    };
    Ok(Inventory {
        head: git(&["rev-parse", "--verify", "-q", "HEAD"]).ok(),
        root,
        snapshot,
        graph,
        codeowners,
        initialized,
        contracts,
        malformed,
        contract_version,
        checkpoints,
        cache,
        warnings,
    })
}

impl Inventory {
    /** Total files, lines, symbols, and parse results per language, sorted by name
     * Input
        - None (uses self)
     * Output
        - BTreeMap<&'static str, LanguageTotals>
    */
    pub(crate) fn languages(&self) -> BTreeMap<&'static str, LanguageTotals> {
        let mut totals: BTreeMap<&'static str, LanguageTotals> = BTreeMap::new();
        for file in &self.snapshot.files {
            let Some(language) = file.language else {
                continue;
            };
            let entry = totals.entry(language.name).or_default();
            entry.files += 1;
            entry.lines += file.outline.lines;
            entry.symbols += file.outline.symbols.len();
            entry.parsed += usize::from(file.outline.status == "parsed");
            entry.partial += usize::from(file.outline.status == "partial");
            entry.enforceable = language.enforceable;
        }
        totals
    }

    /** Return the critical candidates: non-test symbols scoring at least CRITICAL_SCORE, highest
     * score first, then by id
     * Input
        - None (uses self)
     * Output
        - Vec<usize> entity indices
    */
    pub(crate) fn critical(&self) -> Vec<usize> {
        let mut critical = (0..self.graph.entities.len())
            .filter(|index| {
                let entity = &self.graph.entities[*index];
                !entity.test && entity.score >= CRITICAL_SCORE
            })
            .collect::<Vec<_>>();
        critical.sort_by(|left, right| {
            let (left, right) = (&self.graph.entities[*left], &self.graph.entities[*right]);
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.id.cmp(&right.id))
        });
        critical
    }

    /** Measure how much of the inventory existing contracts cover
     * Input
        - None (uses self)
     * Output
        - Coverage
    */
    pub(crate) fn coverage(&self) -> Coverage {
        let mut coverage = Coverage {
            callables: 0,
            callables_covered: 0,
            entities: 0,
            entities_covered: 0,
            critical: 0,
            critical_covered: 0,
        };
        for entity in self.graph.entities.iter().filter(|entity| !entity.test) {
            let covered = !entity.contracts.is_empty();
            coverage.entities += 1;
            coverage.entities_covered += usize::from(covered);
            if self.symbol(entity).kind.callable() {
                coverage.callables += 1;
                coverage.callables_covered += usize::from(covered);
            }
        }
        for index in self.critical() {
            coverage.critical += 1;
            coverage.critical_covered +=
                usize::from(!self.graph.entities[index].contracts.is_empty());
        }
        coverage
    }

    /** Return the symbol behind an entity
     * Input
        - entity: &Entity - entity
     * Output
        - &Symbol
    */
    fn symbol(&self, entity: &Entity) -> &outline::Symbol {
        Graph::symbol(&self.snapshot, entity)
    }

    /** Suggest a preserve rule for an entity, for a human to review; only symbols that policies
     * can target today and whose qualified name is unique among them get a rule
     * Input
        - entity: &Entity - entity
     * Output
        - Result<String, &'static str> the policy statement, or why there is none
    */
    pub(crate) fn suggestion(&self, entity: &Entity) -> Result<String, &'static str> {
        let symbol = self.symbol(entity);
        let kind = symbol
            .kind
            .item_kind()
            .ok_or("no policy item kind targets this symbol")?;
        if !symbol.targetable {
            return Err("this language or declaration cannot be targeted by policies yet");
        }
        let matches = self
            .graph
            .entities
            .iter()
            .filter(|other| {
                let other = self.symbol(other);
                other.targetable
                    && other.kind.item_kind() == Some(kind)
                    && crate::resolver::matches_target(
                        &other.name,
                        &other.qualified,
                        &symbol.qualified,
                    )
            })
            .count();
        if matches != 1 {
            return Err("the qualified name is ambiguous, so a rule would fail closed");
        }
        Ok(format!("preserve {} {};", kind.flag(), symbol.qualified))
    }

    /** Serialize an entity with its relationships as ids
     * Input
        - entity: &Entity - entity
     * Output
        - Value JSON object
    */
    fn entity_json(&self, entity: &Entity) -> Value {
        let symbol = self.symbol(entity);
        let file = &self.snapshot.files[entity.file];
        let language = file.language.map_or("", |language| language.name);
        let ids = |indices: &[usize]| {
            indices
                .iter()
                .map(|index| self.graph.entities[*index].id.clone())
                .collect::<Vec<_>>()
        };
        json!({
            "id": entity.id,
            "kind": symbol.kind.name(),
            "name": symbol.name,
            "qualified": symbol.qualified,
            "language": language,
            "file": file.path,
            "lines": [symbol.start_line, symbol.end_line],
            "module": self.graph.modules[entity.module].id,
            "service": self.graph.services[self.graph.files[entity.file].service].id,
            "owners": self.graph.files[entity.file].owners,
            "test": entity.test,
            "callers": ids(&entity.callers),
            "callees": ids(&entity.callees),
            "ambiguous_calls": entity.ambiguous,
            "tested_by": entity.tested_by,
            "contracts": entity.contracts,
            "policy_target": symbol.targetable.then(|| symbol.kind.item_kind().map(|kind| format!("{} {}", kind.flag(), symbol.qualified))).flatten(),
            "complexity": {"loops": symbol.loops, "branches": symbol.branches},
            "risk_signals": entity.risk,
            "risk_score": entity.score,
        })
    }

    /** Serialize the whole inventory, the machine-readable form of crane discover
     * Input
        - None (uses self)
     * Output
        - Value JSON object
    */
    pub(crate) fn to_json(&self) -> Value {
        let graph = &self.graph;
        let coverage = self.coverage();
        let languages = self
            .languages()
            .into_iter()
            .map(|(name, totals)| {
                json!({
                    "language": name,
                    "files": totals.files,
                    "lines": totals.lines,
                    "symbols": totals.symbols,
                    "parsed": totals.parsed,
                    "partial": totals.partial,
                    "enforceable": totals.enforceable,
                })
            })
            .collect::<Vec<_>>();
        let files = self
            .snapshot
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let info = &graph.files[index];
                let paths = |set: &std::collections::BTreeSet<usize>| {
                    set.iter()
                        .map(|other| self.snapshot.files[*other].path.clone())
                        .collect::<Vec<_>>()
                };
                json!({
                    "id": format!("file:{}", file.path),
                    "path": file.path,
                    "blob": file.blob,
                    "language": file.language.map(|language| language.name),
                    "lines": file.outline.lines,
                    "status": file.outline.status,
                    "module": (info.module != usize::MAX).then(|| graph.modules[info.module].id.clone()),
                    "service": graph.services[info.service].id,
                    "owners": info.owners,
                    "test": info.test,
                    "tests": paths(&info.tests),
                    "tested_by": paths(&info.tested_by),
                    "imports": file.outline.imports,
                    "symbols": file.outline.symbols.len(),
                })
            })
            .collect::<Vec<_>>();
        let modules = graph
            .modules
            .iter()
            .map(|module| {
                json!({
                    "id": module.id,
                    "language": module.language,
                    "name": module.name,
                    "files": module.files,
                    "symbols": module.symbols,
                    "service": graph.services[module.service].id,
                    "depends_on": module.depends_on.iter().map(|(target, count)| json!({"module": graph.modules[*target].id, "calls": count})).collect::<Vec<_>>(),
                    "imports": module.imports,
                })
            })
            .collect::<Vec<_>>();
        let services = graph
            .services
            .iter()
            .filter(|service| service.files > 0)
            .map(|service| {
                json!({
                    "id": service.id,
                    "path": service.path,
                    "evidence": service.evidence,
                    "files": service.files,
                    "symbols": service.symbols,
                    "languages": service.languages,
                    "depends_on": service.depends_on.iter().map(|(target, count)| json!({"service": graph.services[*target].id, "calls": count})).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let folders = graph
            .folders
            .iter()
            .map(|folder| {
                json!({
                    "id": format!("folder:{}", if folder.path.is_empty() { "." } else { &folder.path }),
                    "path": folder.path,
                    "files": folder.files,
                    "files_total": folder.files_total,
                    "symbols_total": folder.symbols_total,
                    "children": folder.children,
                    "depends_on": folder.depends_on,
                })
            })
            .collect::<Vec<_>>();
        let critical = self
            .critical()
            .into_iter()
            .map(|index| {
                let entity = &graph.entities[index];
                let suggestion = self.suggestion(entity);
                json!({
                    "id": entity.id,
                    "risk_score": entity.score,
                    "risk_signals": entity.risk,
                    "covered_by": entity.contracts,
                    "suggestion": suggestion.as_ref().ok(),
                    "suggestion_note": suggestion.err(),
                })
            })
            .collect::<Vec<_>>();
        let contracts = self.initialized.then(|| {
            json!({
                "contract_version": self.contract_version,
                "malformed": self.malformed,
                "clauses": self.contracts.iter().map(|clause| json!({
                    "policy_id": clause.policy_id,
                    "rule": clause.rule,
                    "kind": clause.kind.noun(),
                    "target": clause.target,
                    "scope": clause.scope.name(),
                    "checkpoint": clause.checkpoint,
                    "status": clause.status,
                    "anchors": clause.anchors.iter().map(|index| graph.entities[*index].id.clone()).collect::<Vec<_>>(),
                    "covered_entities": clause.covered,
                    "scope_approximate": clause.scope == crate::model::Scope::Flow,
                })).collect::<Vec<_>>(),
            })
        });
        let checkpoints = self
            .checkpoints
            .iter()
            .map(|checkpoint| {
                json!({
                    "name": checkpoint.name,
                    "commit": checkpoint.commit,
                    "status": checkpoint.status,
                    "exists": checkpoint.exists,
                    "policies": checkpoint.policies,
                    "commits_since": checkpoint.commits_since,
                    "changed_files": checkpoint.changed_files,
                    "covered_entities_changed": checkpoint.covered_changed,
                })
            })
            .collect::<Vec<_>>();
        let percent = |part: usize, whole: usize| {
            if whole == 0 {
                0.0
            } else {
                (part as f64 * 1000.0 / whole as f64).round() / 10.0
            }
        };
        json!({
            "inventory_format": INVENTORY_FORMAT,
            "advisory": true,
            "repository": {
                "root": self.root.to_string_lossy(),
                "head": self.head,
                "files": self.snapshot.files.len(),
                "excluded_files": self.snapshot.excluded,
                "lines": self.snapshot.files.iter().map(|file| file.outline.lines).sum::<usize>(),
                "symbols": graph.entities.len(),
                "crane_initialized": self.initialized,
            },
            "languages": languages,
            "services": services,
            "modules": modules,
            "folders": folders,
            "files": files,
            "entities": graph.entities.iter().map(|entity| self.entity_json(entity)).collect::<Vec<_>>(),
            "ownership": self.codeowners.as_ref().map(|(source, rules)| json!({"source": source, "rules": rules})),
            "contracts": contracts,
            "checkpoints": checkpoints,
            "coverage": {
                "callables": coverage.callables,
                "callables_covered": coverage.callables_covered,
                "callables_percent": percent(coverage.callables_covered, coverage.callables),
                "entities": coverage.entities,
                "entities_covered": coverage.entities_covered,
                "entities_percent": percent(coverage.entities_covered, coverage.entities),
                "critical": coverage.critical,
                "critical_covered": coverage.critical_covered,
            },
            "critical_candidates": critical,
            "index": {
                "files_parsed": self.snapshot.parsed,
                "files_reused": self.snapshot.reused,
                "files_changed": if self.snapshot.links.is_some() { self.snapshot.changed.len() } else { self.snapshot.files.len() },
                "symbols_relinked": graph.relinked,
                "incremental": self.snapshot.links.is_some(),
                "cache": self.cache.as_ref().map(|path| path.to_string_lossy().into_owned()),
            },
            "warnings": self.warnings,
        })
    }
}
