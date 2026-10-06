// Zone recommendations: turn what discovery already knows into candidate zones. The signals are the
// existing ones (the inventory's risk signals, the proposal engine's payment/auth/secrets
// vocabulary and protected-kind regions, the Payments and Testing packs, and session history);
// modules are grouped into one recommendation per category, and every candidate is resolved by
// the zone engine itself, so the resources it lists are exactly the ones authority would govern.
// Nothing here activates anything: recommendations go to review (zones/review.rs).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use serde_json::{json, Value};

use super::model::{parse, Autonomy, Criticality, SafetyState, Zone};
use super::resolve::{resolve, Resolution};
use crate::inventory::graph::Graph;
use crate::inventory::Inventory;
use crate::proposals::engine::{candidates, domains};
use crate::session::{session_ids, ContractSession};
use crate::util::sha256;

/** Version of the recommender (part of every recommendation's provenance) */
pub(crate) const RECOMMENDER: &str = "zone-recommender@1";

/** One kind of governed code: its recommendation id and zone name, its defaults, and its rank
 * (a module is recommended for its highest-ranked category only) */
struct Category {
    key: &'static str,
    title: &'static str,
    criticality: Criticality,
    autonomy: Autonomy,
    rank: u8,
}

/** The categories, from the most restrictive */
const CATEGORIES: &[Category] = &[
    Category {
        key: "security",
        title: "authentication, authorization, and secrets",
        criticality: Criticality::Restricted,
        autonomy: Autonomy::Observe,
        rank: 6,
    },
    Category {
        key: "payments",
        title: "payment processing, refunds, and transactions",
        criticality: Criticality::Critical,
        autonomy: Autonomy::Assisted,
        rank: 5,
    },
    Category {
        key: "data_access",
        title: "persistence and data access",
        criticality: Criticality::Sensitive,
        autonomy: Autonomy::Delegated,
        rank: 4,
    },
    Category {
        key: "api",
        title: "production-facing entry points (controllers, handlers, routes)",
        criticality: Criticality::Sensitive,
        autonomy: Autonomy::Delegated,
        rank: 3,
    },
    Category {
        key: "integrations",
        title: "external integrations (network calls)",
        criticality: Criticality::Sensitive,
        autonomy: Autonomy::Delegated,
        rank: 2,
    },
    Category {
        key: "shared_core",
        title: "shared components many modules depend on",
        criticality: Criticality::Sensitive,
        autonomy: Autonomy::Delegated,
        rank: 1,
    },
];

/** Look up a category
 * Input
    - key: &str - category key
 * Output
    - &'static Category
*/
fn category(key: &str) -> &'static Category {
    CATEGORIES
        .iter()
        .find(|category| category.key == key)
        .expect("known category")
}

/** Evidence collected for one module: per category, the signals seen (signal name to example
 * symbols) and where they came from */
#[derive(Default)]
struct Evidence {
    signals: BTreeMap<&'static str, BTreeMap<String, BTreeSet<String>>>,
    sources: BTreeMap<&'static str, BTreeSet<&'static str>>,
}

impl Evidence {
    /** Record a signal for a category
     * Input
        - category: &'static str - category key
        - signal: String - signal name
        - symbol: String - the symbol showing it
        - source: &'static str - where it came from
     * Output
        - None
    */
    fn add(
        &mut self,
        category: &'static str,
        signal: String,
        symbol: String,
        source: &'static str,
    ) {
        self.signals
            .entry(category)
            .or_default()
            .entry(signal)
            .or_default()
            .insert(symbol);
        self.sources.entry(category).or_default().insert(source);
    }
}

/** A candidate zone being assembled
 * Fields
    - id: String - recommendation id
    - name: String - preferred zone name
    - criticality: Criticality - suggested criticality
    - autonomy: Autonomy - suggested autonomy
    - selectors: Vec<String> - selector statements
    - rationale: Vec<String> - why
    - signals: Vec<Value> - evidence
    - sources: BTreeSet<String> - where the evidence came from
    - confidence: &'static str - high, medium, or low
*/
struct Draft {
    id: String,
    name: String,
    criticality: Criticality,
    autonomy: Autonomy,
    selectors: Vec<String>,
    rationale: Vec<String>,
    signals: Vec<Value>,
    sources: BTreeSet<String>,
    confidence: &'static str,
}

/** A recommendation before review
 * Fields
    - id: String - stable id (category, region, or tests), safe in file names and URLs
    - zone_id: String - name of the zone it would create
    - zone_text: String - the exact .zone file it would write
    - value: Value - everything else (selectors, resources, rationale, signals, confidence, sources)
*/
pub(crate) struct Recommendation {
    pub(crate) id: String,
    pub(crate) zone_id: String,
    pub(crate) zone_text: String,
    pub(crate) value: Value,
}

/** Write a zone definition in the .zone format
 * Input
    - zone_id: &str - zone name
    - criticality: Criticality - criticality
    - autonomy: Autonomy - default autonomy
    - selectors: &[String] - selector statements without "select " and ";"
 * Output
    - String
*/
fn zone_text(
    zone_id: &str,
    criticality: Criticality,
    autonomy: Autonomy,
    selectors: &[String],
) -> String {
    let mut text = format!(
        "zone {zone_id} {{\n    criticality {};\n    autonomy {};\n    state active;\n",
        criticality.name(),
        autonomy.name()
    );
    for selector in selectors {
        text.push_str(&format!("    select {selector};\n"));
    }
    text.push_str("}\n");
    text
}

/** Make an identifier from text (lowercase words joined by underscores)
 * Input
    - text: &str - text
 * Output
    - String
*/
fn slug(text: &str) -> String {
    let words = crate::packs::words(text);
    if words.is_empty() {
        "region".into()
    } else {
        words.join("_")
    }
}

/** Count, per module, the sessions whose executed effects touched its files and the behaviour
 * violations they caused there
 * Input
    - inventory: &Inventory - inventory
 * Output
    - (usize, BTreeMap<usize, (usize, usize)>) sessions considered, and per module index the
      sessions touching it and violations
*/
fn task_history(inventory: &Inventory) -> (usize, BTreeMap<usize, (usize, usize)>) {
    let module_of = inventory
        .snapshot
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.clone(), inventory.graph.files[index].module))
        .collect::<BTreeMap<_, _>>();
    let mut history: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    let mut considered = 0;
    for id in session_ids().unwrap_or_default() {
        let Ok(Some(session)) = ContractSession::load(&id) else {
            continue;
        };
        considered += 1;
        let mut touched = BTreeSet::new();
        let mut violated = BTreeSet::new();
        for event in session
            .events()
            .iter()
            .filter(|event| event["event"] == "post_tool_use")
        {
            let effect = &event["effect"]["files"];
            let files = ["added", "modified", "deleted"]
                .iter()
                .flat_map(|key| effect[*key].as_array().cloned().unwrap_or_default())
                .filter_map(|path| path.as_str().map(String::from))
                .collect::<Vec<_>>();
            let violations = event["effect"]["violations"]
                .as_array()
                .is_some_and(|items| !items.is_empty());
            for module in files
                .iter()
                .filter_map(|file| module_of.get(file))
                .filter(|module| **module != usize::MAX)
            {
                touched.insert(*module);
                if violations {
                    violated.insert(*module);
                }
            }
        }
        for module in touched {
            let entry = history.entry(module).or_default();
            entry.0 += 1;
            entry.1 += usize::from(violated.contains(&module));
        }
    }
    (considered, history)
}

/** Produce the recommendations for a repository: one per category over the modules whose
 * strongest evidence is that category, one per protected-kind region, and one for tests; zones
 * already governing the same resources at the same or a higher criticality make a recommendation
 * covered rather than new
 * Input
    - inventory: &Inventory - inventory
    - existing: &Resolution - the active zones resolved against it
    - reserved: &BTreeSet<String> - names of zones written by hand (never reused)
    - owned: &BTreeMap<String, String> - recommendation id to the zone name an earlier review
      gave it (kept, so approving again replaces the same zone)
    - scratch: &std::path::Path - state file for resolving candidates (never the real one)
 * Output
    - Result<(Vec<Recommendation>, Value), String> recommendations and the discovery result
*/
pub(crate) fn recommend(
    inventory: &Inventory,
    existing: &Resolution,
    reserved: &BTreeSet<String>,
    owned: &BTreeMap<String, String>,
    scratch: &std::path::Path,
) -> Result<(Vec<Recommendation>, Value), String> {
    let graph = &inventory.graph;
    let snapshot = &inventory.snapshot;
    let mut evidence: BTreeMap<usize, Evidence> = BTreeMap::new();
    for entity in graph
        .entities
        .iter()
        .filter(|entity| !entity.test && !graph.files[entity.file].test)
    {
        if entity.module == usize::MAX {
            continue;
        }
        let symbol = Graph::symbol(snapshot, entity);
        let path = &snapshot.files[entity.file].path;
        let module = evidence.entry(entity.module).or_default();
        let name = symbol.qualified.clone();
        for (domain, word) in domains(&symbol.qualified) {
            let key = if domain == "payment" {
                "payments"
            } else {
                "security"
            };
            module.add(
                key,
                format!("{domain}_name:{word}"),
                name.clone(),
                "proposals:engine",
            );
        }
        if let Some((pack_category, _, _)) =
            crate::packs::payment_category(&symbol.name, &symbol.qualified, path)
        {
            module.add(
                "payments",
                format!("pack:{pack_category}"),
                name.clone(),
                "pack:payments@1",
            );
        }
        for signal in &entity.risk {
            let kind = signal.split(':').next().unwrap_or_default();
            let target = match kind {
                "persistence" => Some("data_access"),
                "entrypoint" => Some("api"),
                "network" => Some("integrations"),
                "high_fan_in" | "cross_module_callers" => Some("shared_core"),
                _ => None,
            };
            if let Some(target) = target {
                module.add(target, kind.to_string(), name.clone(), "inventory:risk");
            }
        }
    }
    let (sessions, history) = task_history(inventory);
    // Each module goes to its highest-ranked category
    let mut grouped: BTreeMap<&'static str, Vec<usize>> = BTreeMap::new();
    for (module, found) in &evidence {
        if let Some(best) = found.signals.keys().max_by_key(|key| category(key).rank) {
            grouped.entry(best).or_default().push(*module);
        }
    }
    let mut drafts: Vec<Draft> = Vec::new();
    for (key, modules) in &grouped {
        let category = category(key);
        // Module selectors take the id without its "module:" prefix
        let mut selectors = modules
            .iter()
            .map(|module| {
                format!(
                    "module {}",
                    graph.modules[*module].id.trim_start_matches("module:")
                )
            })
            .collect::<Vec<_>>();
        selectors.sort();
        let mut signals: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut sources = BTreeSet::new();
        for module in modules {
            let found = &evidence[module];
            for (signal, symbols) in &found.signals[key] {
                signals
                    .entry(signal.clone())
                    .or_default()
                    .extend(symbols.iter().cloned());
            }
            sources.extend(found.sources[key].iter().map(|source| source.to_string()));
        }
        let kinds = signals
            .keys()
            .map(|signal| signal.split(':').next().unwrap_or_default().to_string())
            .collect::<BTreeSet<_>>();
        let symbols = signals.values().flatten().collect::<BTreeSet<_>>().len();
        let (touched, violated) = modules
            .iter()
            .filter_map(|module| history.get(module))
            .fold((0, 0), |sum, (touched, violated)| {
                (sum.0 + touched, sum.1 + violated)
            });
        let strong = kinds.iter().any(|kind| {
            kind.ends_with("_name")
                || kind.starts_with("pack")
                || kind == "persistence"
                || kind == "entrypoint"
        });
        let mut confidence = if kinds.len() >= 2 && strong {
            "high"
        } else if strong || symbols >= 3 {
            "medium"
        } else {
            "low"
        };
        let mut rationale = vec![format!(
            "{} module(s) hold {}: {} symbol(s) show it",
            modules.len(),
            category.title,
            symbols
        )];
        if touched > 0 {
            sources.insert("task_history".into());
            rationale.push(format!(
                "agent sessions changed this code {touched} time(s){}",
                if violated > 0 {
                    format!(", with violations {violated} time(s)")
                } else {
                    String::new()
                }
            ));
            if violated > 0 && confidence == "medium" {
                confidence = "high";
            }
        }
        let signal_list = signals.iter().map(|(signal, symbols)| json!({"signal": signal, "symbols": symbols.len(), "examples": symbols.iter().take(3).collect::<Vec<_>>()})).collect::<Vec<_>>();
        drafts.push(Draft {
            id: key.to_string(),
            name: key.to_string(),
            criticality: category.criticality,
            autonomy: category.autonomy,
            selectors,
            rationale,
            signals: signal_list,
            sources,
            confidence,
        });
    }
    for candidate in candidates(inventory, Some(existing))
        .into_iter()
        .filter(|candidate| candidate.kind == "region")
    {
        let signal = candidate
            .signals
            .first()
            .map(|signal| signal.signal.clone())
            .unwrap_or_default();
        let (criticality, autonomy) = if signal == "secrets_config" {
            (Criticality::Restricted, Autonomy::Observe)
        } else {
            (Criticality::Critical, Autonomy::Assisted)
        };
        let selector = candidate
            .suggested_zone
            .as_deref()
            .and_then(|zone| {
                zone.lines().find_map(|line| {
                    line.trim()
                        .strip_prefix("select ")
                        .map(|selector| selector.trim_end_matches(';').to_string())
                })
            })
            .unwrap_or_else(|| format!("path {}", candidate.candidate));
        let id = format!(
            "region_{}",
            slug(&format!("{signal} {}", candidate.candidate))
        );
        drafts.push(Draft {
            id: id.clone(),
            name: id,
            criticality,
            autonomy,
            selectors: vec![selector],
            rationale: vec![
                candidate.note.clone().unwrap_or_default(),
                candidate.reason(),
            ],
            signals: candidate
                .signals
                .iter()
                .map(|signal| json!({"signal": signal.signal, "detail": signal.detail}))
                .collect(),
            sources: BTreeSet::from(["proposals:engine".to_string()]),
            confidence: candidate.confidence.name(),
        });
    }
    if graph.files.iter().any(|file| file.test) {
        drafts.push(Draft { id: "tests".into(), name: "tests".into(), criticality: Criticality::Routine, autonomy: Autonomy::Delegated, selectors: vec!["tests".into()], rationale: vec!["test code: agents may change it within their task; contract tests never depend on it".into()], signals: vec![json!({"signal": "test_files", "files": graph.files.iter().filter(|file| file.test).count()})], sources: BTreeSet::from(["pack:testing@1".to_string()]), confidence: "high" });
    }

    // Resolve every candidate with the zone engine itself, on a scratch state file
    let mut texts = Vec::new();
    let mut zones: Vec<Zone> = Vec::new();
    for draft in &drafts {
        let zone_id = match owned.get(&draft.id) {
            Some(zone_id) => zone_id.clone(),
            None => {
                let taken = |name: &String| {
                    reserved.contains(name)
                        || owned
                            .iter()
                            .any(|(id, zone)| zone == name && *id != draft.id)
                };
                let mut zone_id = draft.name.clone();
                let mut suffix = 2;
                while taken(&zone_id) {
                    zone_id = format!("{}_{suffix}", draft.name);
                    suffix += 1;
                }
                zone_id
            }
        };
        let text = zone_text(
            &zone_id,
            draft.criticality,
            draft.autonomy,
            &draft.selectors,
        );
        let parsed = parse(&text, &format!("recommendation {}", draft.id))?;
        zones.extend(parsed);
        texts.push((draft.id.clone(), text));
    }
    let resolution = resolve(inventory, zones, scratch)?;
    let _ = fs::remove_file(scratch);
    let mut recommendations = Vec::new();
    for (index, draft) in drafts.iter().enumerate() {
        let result = &resolution.zones[index];
        let files = result
            .files
            .iter()
            .map(|file| snapshot.files[*file].path.clone())
            .collect::<Vec<_>>();
        let entities = result
            .entities
            .iter()
            .map(|entity| graph.entities[*entity].id.clone())
            .collect::<Vec<_>>();
        // Covered when active zones already govern every file at this criticality or higher
        let covered = !result.files.is_empty()
            && result.files.iter().all(|file| {
                existing
                    .files
                    .get(file)
                    .is_some_and(|effective| effective.criticality >= draft.criticality)
            });
        let zone_id = result.zone.zone_id.clone();
        let text = texts[index].1.clone();
        recommendations.push(Recommendation {
            id: draft.id.clone(),
            zone_id: zone_id.clone(),
            zone_text: text.clone(),
            value: json!({
                "category": draft.id,
                "selectors": draft.selectors,
                "selector_status": result.selectors.iter().map(|selector| json!({"selector": selector.selector.text(), "status": selector.status})).collect::<Vec<_>>(),
                "affected": {"files": files, "entities": entities.len(), "entity_examples": entities.iter().take(8).collect::<Vec<_>>()},
                "criticality": draft.criticality.name(),
                "autonomy": draft.autonomy.name(),
                "safety_state": SafetyState::Active.name(),
                "rationale": draft.rationale,
                "signals": draft.signals,
                "confidence": draft.confidence,
                "sources": draft.sources,
                "covered_by_active_zones": covered,
                "digest": sha256(text.as_bytes()),
            }),
        });
    }
    let discovery = json!({
        "recommender": RECOMMENDER,
        "head": inventory.head,
        "inventory": {"files": snapshot.files.len(), "symbols": graph.entities.len(), "modules": graph.modules.len()},
        "signals": {
            "risk_scored_symbols": graph.entities.iter().filter(|entity| !entity.risk.is_empty()).count(),
            "modules_with_evidence": evidence.len(),
        },
        "task_history": {"sessions_considered": sessions, "modules_touched": history.len()},
        "existing_zones": reserved,
        "recommendations": recommendations.iter().map(|recommendation| recommendation.id.clone()).collect::<Vec<_>>(),
    });
    Ok((recommendations, discovery))
}
