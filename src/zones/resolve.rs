use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use serde_json::{json, Map, Value};

use super::model::{Autonomy, Criticality, SafetyState, Selector, SelectorKind, Zone};
use crate::inventory::governance::glob;
use crate::inventory::graph::Graph;
use crate::inventory::Inventory;
use crate::scope::{folder_of, folder_prefix};
use crate::util::io_error;

/** Version of the stored last-resolution layout */
const STATE_FORMAT: u64 = 1;

/** Number of example entities named in a conflict */
const EXAMPLES: usize = 5;

/** How one selector resolved in the current repository
 * Fields
    - selector: Selector - the selector
    - status: &'static str - "resolved"; "ambiguous" (an exact name matched several things,
      all of which are covered); "missing" (it resolved before and no longer does, so its target
      was deleted or renamed); or "unresolved" (it never resolved)
    - targets: Vec<String> - ids of the symbols, modules, services, folders, or policies matched
    - entities: BTreeSet<usize> - covered entities (indices into the inventory's entities)
    - files: BTreeSet<usize> - covered files (indices into the inventory's files)
    - previous: Vec<String> - targets of the last successful resolution, for missing selectors
    - candidates: Vec<String> - things that may be the renamed or moved target, never applied
      automatically
*/
pub(crate) struct SelectorResult {
    pub(crate) selector: Selector,
    pub(crate) status: &'static str,
    pub(crate) targets: Vec<String>,
    pub(crate) entities: BTreeSet<usize>,
    pub(crate) files: BTreeSet<usize>,
    pub(crate) previous: Vec<String>,
    pub(crate) candidates: Vec<String>,
}

/** How one zone resolved
 * Fields
    - zone: Zone - the definition
    - selectors: Vec<SelectorResult> - per selector, in definition order
    - entities: BTreeSet<usize> - every covered entity
    - files: BTreeSet<usize> - every covered file
    - state: SafetyState - effective state: the declared one, or Degraded when a selector does
      not resolve cleanly
    - autonomy: Autonomy - effective autonomy: the declared one capped by criticality and state
*/
pub(crate) struct ZoneResult {
    pub(crate) zone: Zone,
    pub(crate) selectors: Vec<SelectorResult>,
    pub(crate) entities: BTreeSet<usize>,
    pub(crate) files: BTreeSet<usize>,
    pub(crate) state: SafetyState,
    pub(crate) autonomy: Autonomy,
}

/** The combined zone constraints on one entity or file: the most restrictive of every zone
 * covering it
 * Fields
    - zones: Vec<usize> - indices into Resolution::zones
    - criticality: Criticality - highest criticality
    - autonomy: Autonomy - lowest effective autonomy
    - state: SafetyState - worst effective state
*/
pub(crate) struct Effective {
    pub(crate) zones: Vec<usize>,
    pub(crate) criticality: Criticality,
    pub(crate) autonomy: Autonomy,
    pub(crate) state: SafetyState,
}

/** A conflict between zones, or between a zone and the policies
 * Fields
    - kind: &'static str - overlap, autonomy_exceeds_criticality, policy_reference_missing,
      policy_reference_outside_zone, or policy_requires_change
    - zones: Vec<String> - zones involved
    - message: String - explanation, including how it is resolved
    - entities: Vec<String> - example entity ids or file paths involved
*/
pub(crate) struct Conflict {
    pub(crate) kind: &'static str,
    pub(crate) zones: Vec<String>,
    pub(crate) message: String,
    pub(crate) entities: Vec<String>,
}

/** Every zone resolved against one inventory
 * Fields
    - zones: Vec<ZoneResult> - zones in definition order
    - entities: BTreeMap<usize, Effective> - constraints per covered entity
    - files: BTreeMap<usize, Effective> - constraints per covered file
    - conflicts: Vec<Conflict> - conflicts found
*/
pub(crate) struct Resolution {
    pub(crate) zones: Vec<ZoneResult>,
    pub(crate) entities: BTreeMap<usize, Effective>,
    pub(crate) files: BTreeMap<usize, Effective>,
    pub(crate) conflicts: Vec<Conflict>,
}

/** Match a name against a selector pattern ("*" and "?" stay within one "/" segment, "**"
 * crosses segments)
 * Input
    - pattern: &str - selector value
    - text: &str - candidate name
 * Output
    - bool
*/
fn matches(pattern: &str, text: &str) -> bool {
    glob(pattern.as_bytes(), text.as_bytes())
}

/** Check whether a selector value is a pattern rather than an exact name
 * Input
    - value: &str - selector value
 * Output
    - bool
*/
fn wildcard(value: &str) -> bool {
    value.contains('*') || value.contains('?')
}

/** Split a module, service, or folder name into its segments
 * Input
    - name: &str - name
 * Output
    - Vec<String> lowercase segments
*/
fn segments(name: &str) -> Vec<String> {
    name.split(['.', '/', ':'])
        .filter(|segment| !segment.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/** Resolve one selector against the inventory, without history
 * Input
    - inventory: &Inventory - current inventory
    - selector: &Selector - selector
 * Output
    - (Vec<String>, BTreeSet<usize>, BTreeSet<usize>) matched target ids, entities, and files
*/
fn select(
    inventory: &Inventory,
    selector: &Selector,
) -> (Vec<String>, BTreeSet<usize>, BTreeSet<usize>) {
    let graph = &inventory.graph;
    let snapshot = &inventory.snapshot;
    let value = selector.value.as_str();
    let mut targets = BTreeSet::new();
    let mut entities = BTreeSet::new();
    let mut files = BTreeSet::new();
    let in_files = |files: &BTreeSet<usize>| {
        graph
            .entities
            .iter()
            .enumerate()
            .filter(|(_, entity)| files.contains(&entity.file))
            .map(|(index, _)| index)
            .collect::<BTreeSet<_>>()
    };
    match selector.kind {
        SelectorKind::Symbol => {
            for (index, entity) in graph.entities.iter().enumerate() {
                let subject = if value.contains(':') {
                    entity.id.strip_prefix("symbol:").unwrap_or(&entity.id)
                } else {
                    Graph::symbol(snapshot, entity).qualified.as_str()
                };
                if matches(value, subject) {
                    targets.insert(entity.id.clone());
                    entities.insert(index);
                    files.insert(entity.file);
                }
            }
        }
        SelectorKind::Module => {
            let chosen = graph
                .modules
                .iter()
                .enumerate()
                .filter(|(_, module)| {
                    let subject = if value.contains(':') {
                        module.id.strip_prefix("module:").unwrap_or(&module.id)
                    } else {
                        module.name.as_str()
                    };
                    module.files > 0 && matches(value, subject)
                })
                .map(|(index, module)| {
                    targets.insert(module.id.clone());
                    index
                })
                .collect::<BTreeSet<_>>();
            files.extend(
                (0..snapshot.files.len())
                    .filter(|file| chosen.contains(&graph.files[*file].module)),
            );
            entities = in_files(&files);
        }
        SelectorKind::Service => {
            let chosen = graph
                .services
                .iter()
                .enumerate()
                .filter(|(_, service)| {
                    let path = if service.path.is_empty() {
                        "."
                    } else {
                        &service.path
                    };
                    let name = path.rsplit('/').next().unwrap_or(path);
                    service.files > 0 && (matches(value, path) || matches(value, name))
                })
                .map(|(index, service)| {
                    targets.insert(service.id.clone());
                    index
                })
                .collect::<BTreeSet<_>>();
            files.extend(
                (0..snapshot.files.len())
                    .filter(|file| chosen.contains(&graph.files[*file].service)),
            );
            entities = in_files(&files);
        }
        SelectorKind::Subsystem => {
            let pattern = value.to_ascii_lowercase();
            let hit = |name: &str| {
                segments(name)
                    .iter()
                    .any(|segment| matches(&pattern, segment))
            };
            let services = graph
                .services
                .iter()
                .enumerate()
                .filter(|(_, service)| {
                    service.files > 0 && !service.path.is_empty() && hit(&service.path)
                })
                .map(|(index, service)| {
                    targets.insert(service.id.clone());
                    index
                })
                .collect::<BTreeSet<_>>();
            let modules = graph
                .modules
                .iter()
                .enumerate()
                .filter(|(_, module)| module.files > 0 && hit(&module.name))
                .map(|(index, module)| {
                    targets.insert(module.id.clone());
                    index
                })
                .collect::<BTreeSet<_>>();
            let folders = graph
                .folders
                .iter()
                .filter(|folder| {
                    let name = folder.path.rsplit('/').next().unwrap_or(&folder.path);
                    !folder.path.is_empty() && matches(&pattern, &name.to_ascii_lowercase())
                })
                .map(|folder| {
                    targets.insert(format!("folder:{}", folder.path));
                    folder_prefix(&folder.path)
                })
                .collect::<Vec<_>>();
            files.extend((0..snapshot.files.len()).filter(|file| {
                let info = &graph.files[*file];
                services.contains(&info.service)
                    || modules.contains(&info.module)
                    || folders
                        .iter()
                        .any(|prefix| snapshot.files[*file].path.starts_with(prefix.as_str()))
            }));
            entities = in_files(&files);
        }
        SelectorKind::Policy => {
            for (index, entity) in graph.entities.iter().enumerate() {
                if entity
                    .contracts
                    .iter()
                    .any(|tag| tag == value || tag.starts_with(&format!("{value}:")))
                {
                    entities.insert(index);
                    files.insert(entity.file);
                }
            }
            if !entities.is_empty() {
                targets.insert(format!("policy:{value}"));
            }
        }
        SelectorKind::Tests => {
            for (index, entity) in graph.entities.iter().enumerate() {
                if entity.test {
                    entities.insert(index);
                    files.insert(entity.file);
                }
            }
            files.extend((0..snapshot.files.len()).filter(|file| graph.files[*file].test));
            if !files.is_empty() {
                targets.insert("tests".into());
            }
        }
        SelectorKind::Folder => {
            let folder = value.trim_end_matches('/');
            for candidate in &graph.folders {
                if !candidate.path.is_empty() && matches(folder, &candidate.path) {
                    targets.insert(format!("folder:{}", candidate.path));
                    let prefix = folder_prefix(&candidate.path);
                    files
                        .extend((0..snapshot.files.len()).filter(|file| {
                            snapshot.files[*file].path.starts_with(prefix.as_str())
                        }));
                }
            }
            entities = in_files(&files);
        }
        SelectorKind::Path => {
            for (index, file) in snapshot.files.iter().enumerate() {
                if matches(value, &file.path) {
                    targets.insert(format!("file:{}", file.path));
                    files.insert(index);
                }
            }
            entities = in_files(&files);
        }
    }
    (targets.into_iter().collect(), entities, files)
}

/** The last successful resolution of one selector, kept to explain later failures
 * Fields
    - targets: Vec<String> - matched target ids
    - files: BTreeMap<String, String> - covered file paths and their content ids
    - symbols: Vec<(String, String, String, Vec<String>)> - for symbol selectors, each matched
      symbol's id, file, kind, and the ids of every symbol then in that file
*/
#[derive(Default)]
struct Previous {
    targets: Vec<String>,
    files: BTreeMap<String, String>,
    symbols: Vec<(String, String, String, Vec<String>)>,
}

/** Read the stored last resolutions, keyed by zone id and selector text
 * Input
    - path: &Path - state file
 * Output
    - HashMap<(String, String), Previous>, empty when there is no usable state
*/
fn load_state(path: &Path) -> HashMap<(String, String), Previous> {
    let Some(value) = fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<Value>(&content).ok())
        .filter(|value| value["state_format"].as_u64() == Some(STATE_FORMAT))
    else {
        return HashMap::new();
    };
    let strings = |value: &Value| {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let mut state = HashMap::new();
    for (zone, selectors) in value["zones"].as_object().into_iter().flatten() {
        for (selector, record) in selectors.as_object().into_iter().flatten() {
            let files = record["files"]
                .as_object()
                .map(|files| {
                    files
                        .iter()
                        .filter_map(|(path, blob)| Some((path.clone(), blob.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let symbols = record["symbols"]
                .as_array()
                .map(|symbols| {
                    symbols
                        .iter()
                        .filter_map(|symbol| {
                            Some((
                                symbol["id"].as_str()?.to_string(),
                                symbol["file"].as_str()?.to_string(),
                                symbol["kind"].as_str()?.to_string(),
                                strings(&symbol["siblings"]),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            state.insert(
                (zone.clone(), selector.clone()),
                Previous {
                    targets: strings(&record["targets"]),
                    files,
                    symbols,
                },
            );
        }
    }
    state
}

/** Record a selector's current resolution for later runs
 * Input
    - inventory: &Inventory - current inventory
    - result: &SelectorResult - a resolved or ambiguous selector
 * Output
    - Value JSON record
*/
fn record(inventory: &Inventory, result: &SelectorResult) -> Value {
    let graph = &inventory.graph;
    let snapshot = &inventory.snapshot;
    let files = result
        .files
        .iter()
        .map(|file| {
            (
                snapshot.files[*file].path.clone(),
                json!(snapshot.files[*file].blob),
            )
        })
        .collect::<Map<_, _>>();
    let symbols = if result.selector.kind == SelectorKind::Symbol {
        result
            .entities
            .iter()
            .map(|index| {
                let entity = &graph.entities[*index];
                let siblings = graph
                    .entities
                    .iter()
                    .filter(|other| other.file == entity.file)
                    .map(|other| other.id.clone())
                    .collect::<Vec<_>>();
                json!({
                    "id": entity.id,
                    "file": snapshot.files[entity.file].path,
                    "kind": Graph::symbol(snapshot, entity).kind.name(),
                    "siblings": siblings,
                })
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    json!({"targets": result.targets, "files": files, "symbols": symbols})
}

/** Find what a selector that no longer resolves may now be called, for a human to confirm: for
 * a symbol, a symbol of the same kind that newly appeared in the file that held it; for a module,
 * service, subsystem, or folder, the modules, services, or folder now holding at least half of
 * the content it covered (matched by Git content id, so moves are recognized)
 * Input
    - inventory: &Inventory - current inventory
    - kind: SelectorKind - selector kind
    - previous: &Previous - last successful resolution
 * Output
    - Vec<String> candidate target ids, sorted
*/
fn candidates(inventory: &Inventory, kind: SelectorKind, previous: &Previous) -> Vec<String> {
    let graph = &inventory.graph;
    let snapshot = &inventory.snapshot;
    let mut found = BTreeSet::new();
    if kind == SelectorKind::Symbol {
        for (_, file, symbol_kind, siblings) in &previous.symbols {
            for entity in &graph.entities {
                if &snapshot.files[entity.file].path == file
                    && Graph::symbol(snapshot, entity).kind.name() == symbol_kind
                    && !siblings.contains(&entity.id)
                {
                    found.insert(entity.id.clone());
                }
            }
        }
        return found.into_iter().collect();
    }
    if previous.files.is_empty()
        || matches!(
            kind,
            SelectorKind::Policy | SelectorKind::Tests | SelectorKind::Path
        )
    {
        return Vec::new();
    }
    let old_blobs = previous.files.values().collect::<BTreeSet<_>>();
    let holders = (0..snapshot.files.len())
        .filter(|file| old_blobs.contains(&snapshot.files[*file].blob))
        .collect::<Vec<_>>();
    let half = previous.files.len().div_ceil(2);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for file in &holders {
        let info = &graph.files[*file];
        if matches!(kind, SelectorKind::Module | SelectorKind::Subsystem)
            && info.module != usize::MAX
        {
            *counts
                .entry(graph.modules[info.module].id.clone())
                .or_default() += 1;
        }
        if matches!(kind, SelectorKind::Service | SelectorKind::Subsystem) {
            *counts
                .entry(graph.services[info.service].id.clone())
                .or_default() += 1;
        }
    }
    found.extend(
        counts
            .into_iter()
            .filter(|(_, count)| *count >= half)
            .map(|(id, _)| id),
    );
    if kind == SelectorKind::Folder && holders.len() >= half {
        let folders = holders
            .iter()
            .map(|file| folder_of(&snapshot.files[*file].path))
            .collect::<Vec<_>>();
        let mut common = folders[0].clone();
        for folder in &folders[1..] {
            while !(folder == &common || folder.starts_with(&folder_prefix(&common))) {
                common = folder_of(&common);
            }
        }
        if !common.is_empty() {
            found.insert(format!("folder:{common}"));
        }
    }
    found.into_iter().collect()
}

/** Resolve every zone against the inventory: match each selector, classify it (resolved,
 * ambiguous, missing with rename candidates from its last good resolution, or unresolved),
 * derive each zone's effective state and autonomy, combine overlapping zones per entity and file
 * (most restrictive wins), find conflicts, and store the new last resolutions
 * Input
    - inventory: &Inventory - current inventory
    - zones: Vec<Zone> - zone definitions
    - state_path: &Path - file holding the last resolutions
 * Output
    - Result<Resolution, String>
    - Error if the state file cannot be written
*/
pub(crate) fn resolve(
    inventory: &Inventory,
    zones: Vec<Zone>,
    state_path: &Path,
) -> Result<Resolution, String> {
    let mut history = load_state(state_path);
    let mut stored = Map::new();
    let mut results = Vec::new();
    for zone in zones {
        let mut selectors = Vec::new();
        let mut records = Map::new();
        for selector in &zone.selectors {
            let key = (zone.zone_id.clone(), selector.text());
            let (targets, entities, files) = select(inventory, selector);
            let previous = history.remove(&key);
            let exact = matches!(
                selector.kind,
                SelectorKind::Symbol | SelectorKind::Module | SelectorKind::Service
            ) && !wildcard(&selector.value);
            let status = match (targets.len(), &previous) {
                (0, Some(_)) => "missing",
                (0, None) => "unresolved",
                (count, _) if exact && count > 1 => "ambiguous",
                _ => "resolved",
            };
            let mut result = SelectorResult {
                selector: selector.clone(),
                status,
                targets,
                entities,
                files,
                previous: Vec::new(),
                candidates: Vec::new(),
            };
            match previous {
                Some(previous) if status == "missing" => {
                    result.candidates = candidates(inventory, selector.kind, &previous);
                    result.previous = previous.targets.clone();
                    // Keep the last good resolution until the selector resolves again
                    records.insert(selector.text(), previous_json(&previous));
                }
                _ if status == "unresolved" => {}
                _ => {
                    records.insert(selector.text(), record(inventory, &result));
                }
            }
            selectors.push(result);
        }
        stored.insert(zone.zone_id.clone(), Value::Object(records));
        let entities = selectors
            .iter()
            .flat_map(|result| result.entities.iter().copied())
            .collect::<BTreeSet<_>>();
        let files = selectors
            .iter()
            .flat_map(|result| result.files.iter().copied())
            .collect::<BTreeSet<_>>();
        let degraded = selectors.iter().any(|result| result.status != "resolved");
        let state = if degraded {
            zone.safety_state.max(SafetyState::Degraded)
        } else {
            zone.safety_state
        };
        let autonomy = zone
            .default_autonomy
            .min(zone.criticality.autonomy_cap())
            .min(state.autonomy_cap());
        results.push(ZoneResult {
            zone,
            selectors,
            entities,
            files,
            state,
            autonomy,
        });
    }
    save_state(state_path, stored)?;
    let mut entities: BTreeMap<usize, Effective> = BTreeMap::new();
    let mut files: BTreeMap<usize, Effective> = BTreeMap::new();
    for (index, result) in results.iter().enumerate() {
        let combine = |map: &mut BTreeMap<usize, Effective>, key: usize| {
            let entry = map.entry(key).or_insert(Effective {
                zones: Vec::new(),
                criticality: Criticality::Routine,
                autonomy: Autonomy::Autonomous,
                state: SafetyState::Active,
            });
            entry.zones.push(index);
            entry.criticality = entry.criticality.max(result.zone.criticality);
            entry.autonomy = entry.autonomy.min(result.autonomy);
            entry.state = entry.state.max(result.state);
        };
        for entity in &result.entities {
            combine(&mut entities, *entity);
        }
        for file in &result.files {
            combine(&mut files, *file);
        }
    }
    let conflicts = conflicts(inventory, &results, &entities, &files);
    Ok(Resolution {
        zones: results,
        entities,
        files,
        conflicts,
    })
}

/** Serialize a kept last resolution unchanged
 * Input
    - previous: &Previous - record
 * Output
    - Value JSON record
*/
fn previous_json(previous: &Previous) -> Value {
    json!({
        "targets": previous.targets,
        "files": previous.files,
        "symbols": previous.symbols.iter().map(|(id, file, kind, siblings)| json!({
            "id": id, "file": file, "kind": kind, "siblings": siblings,
        })).collect::<Vec<_>>(),
    })
}

/** Write the last resolutions (records of zones or selectors that no longer exist are dropped),
 * through a temporary file renamed into place, keeping the runtime directory out of Git
 * Input
    - path: &Path - state file
    - zones: Map<String, Value> - records by zone id and selector text
 * Output
    - Result<(), String>
    - Error if the file cannot be written
*/
fn save_state(path: &Path, zones: Map<String, Value>) -> Result<(), String> {
    let directory = path.parent().ok_or("invalid zone state path")?;
    fs::create_dir_all(directory).map_err(io_error)?;
    if let Some(runtime) = directory.parent() {
        let ignore = runtime.join(".gitignore");
        if !ignore.exists() {
            fs::write(ignore, "*\n").map_err(io_error)?;
        }
    }
    let document = json!({"state_format": STATE_FORMAT, "zones": zones});
    let temporary = directory.join(format!("resolution.json.{}", std::process::id()));
    fs::write(&temporary, document.to_string()).map_err(io_error)?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        io_error(error)
    })
}

/** Find conflicts: zones declaring more autonomy than their criticality allows; overlapping
 * zones that disagree (resolved by the most restrictive values); zone policy references to
 * policies that do not exist or cover nothing in the zone; and target rules that require a
 * change where the zones only let agents observe
 * Input
    - inventory: &Inventory - inventory with contract coverage
    - results: &[ZoneResult] - resolved zones
    - entities: &BTreeMap<usize, Effective> - combined constraints per entity
    - files: &BTreeMap<usize, Effective> - combined constraints per file
 * Output
    - Vec<Conflict>
*/
fn conflicts(
    inventory: &Inventory,
    results: &[ZoneResult],
    entities: &BTreeMap<usize, Effective>,
    files: &BTreeMap<usize, Effective>,
) -> Vec<Conflict> {
    let graph = &inventory.graph;
    let mut found = Vec::new();
    for result in results {
        let zone = &result.zone;
        if zone.default_autonomy > zone.criticality.autonomy_cap() {
            found.push(Conflict {
                kind: "autonomy_exceeds_criticality",
                zones: vec![zone.zone_id.clone()],
                message: format!(
                    "zone {} declares autonomy {} but {} resources allow at most {}; {} applies",
                    zone.zone_id,
                    zone.default_autonomy.name(),
                    zone.criticality.name(),
                    zone.criticality.autonomy_cap().name(),
                    zone.criticality.autonomy_cap().name()
                ),
                entities: Vec::new(),
            });
        }
        if let Some(policy) = &zone.policy_reference {
            let clauses = inventory
                .contracts
                .iter()
                .filter(|clause| &clause.policy_id == policy)
                .collect::<Vec<_>>();
            if clauses.is_empty() {
                found.push(Conflict {
                    kind: "policy_reference_missing",
                    zones: vec![zone.zone_id.clone()],
                    message: format!(
                        "zone {} references policy {}, which does not exist or does not compile",
                        zone.zone_id, policy
                    ),
                    entities: Vec::new(),
                });
            } else {
                let prefix = format!("{policy}:");
                let covered = graph
                    .entities
                    .iter()
                    .enumerate()
                    .filter(|(_, entity)| {
                        entity.contracts.iter().any(|tag| tag.starts_with(&prefix))
                    })
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                if !covered.is_empty()
                    && !covered.iter().any(|index| result.entities.contains(index))
                {
                    found.push(Conflict {
                        kind: "policy_reference_outside_zone",
                        zones: vec![zone.zone_id.clone()],
                        message: format!(
                            "zone {} references policy {}, but nothing that policy covers is in the zone",
                            zone.zone_id, policy
                        ),
                        entities: covered
                            .iter()
                            .take(EXAMPLES)
                            .map(|index| graph.entities[*index].id.clone())
                            .collect(),
                    });
                }
            }
        }
    }
    // Overlapping zones that disagree, per pair
    let mut pairs: BTreeMap<(usize, usize), Vec<String>> = BTreeMap::new();
    let describe = |index: usize| {
        let result = &results[index];
        (result.zone.criticality, result.autonomy, result.state)
    };
    let mut note = |zones: &[usize], label: String| {
        for (position, left) in zones.iter().enumerate() {
            for right in &zones[position + 1..] {
                if describe(*left) != describe(*right) {
                    pairs
                        .entry((*left, *right))
                        .or_default()
                        .push(label.clone());
                }
            }
        }
    };
    for (entity, effective) in entities {
        note(&effective.zones, graph.entities[*entity].id.clone());
    }
    for (file, effective) in files {
        if !graph.entities.iter().any(|entity| entity.file == *file) {
            note(
                &effective.zones,
                format!("file:{}", inventory.snapshot.files[*file].path),
            );
        }
    }
    for ((left, right), shared) in pairs {
        let combined = |index: usize| describe(index);
        let (left_values, right_values) = (combined(left), combined(right));
        found.push(Conflict {
            kind: "overlap",
            zones: vec![results[left].zone.zone_id.clone(), results[right].zone.zone_id.clone()],
            message: format!(
                "zones {} ({}, {}, {}) and {} ({}, {}, {}) both cover {} entities; the most restrictive values apply: {}, {}, {}",
                results[left].zone.zone_id,
                left_values.0.name(),
                left_values.1.name(),
                left_values.2.name(),
                results[right].zone.zone_id,
                right_values.0.name(),
                right_values.1.name(),
                right_values.2.name(),
                shared.len(),
                left_values.0.max(right_values.0).name(),
                left_values.1.min(right_values.1).name(),
                left_values.2.max(right_values.2).name()
            ),
            entities: shared.into_iter().take(EXAMPLES).collect(),
        });
    }
    // Target rules that require changes agents may only observe
    for clause in inventory
        .contracts
        .iter()
        .filter(|clause| clause.rule == "target")
    {
        for anchor in &clause.anchors {
            if let Some(effective) = entities
                .get(anchor)
                .filter(|effective| effective.autonomy == Autonomy::Observe)
            {
                found.push(Conflict {
                    kind: "policy_requires_change",
                    zones: effective
                        .zones
                        .iter()
                        .map(|index| results[*index].zone.zone_id.clone())
                        .collect(),
                    message: format!(
                        "policy {} requires {} to change, but its zones only let agents observe it; a human must make the change or the zone or policy must change",
                        clause.policy_id, clause.target
                    ),
                    entities: vec![graph.entities[*anchor].id.clone()],
                });
            }
        }
    }
    found
}
