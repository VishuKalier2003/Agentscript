use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::inventory::governance::words;
use crate::inventory::graph::Graph;
use crate::inventory::Inventory;
use crate::scope::folder_of;
use crate::zones::model::Criticality;
use crate::zones::resolve::Resolution;

/** Words naming payment handling */
const PAYMENT: &[&str] = &[
    "payment",
    "pay",
    "charge",
    "billing",
    "bill",
    "invoice",
    "refund",
    "ledger",
    "settle",
    "settlement",
    "wallet",
    "transaction",
    "checkout",
    "payout",
    "balance",
    "transfer",
];

/** Words naming authentication or authorization */
const AUTH: &[&str] = &[
    "auth",
    "authenticate",
    "authentication",
    "authorize",
    "authorization",
    "login",
    "logout",
    "signin",
    "signup",
    "permission",
    "role",
    "acl",
    "oauth",
    "jwt",
    "sso",
    "rbac",
];

/** Words naming credentials or secret handling */
const SECRETS: &[&str] = &[
    "secret",
    "credential",
    "password",
    "passwd",
    "apikey",
    "keystore",
    "vault",
    "kms",
    "encrypt",
    "decrypt",
    "cipher",
    "crypto",
    "hmac",
    "signature",
];

/** Word pairs naming keys and tokens */
const SECRET_PAIRS: &[(&str, &str)] = &[
    ("api", "key"),
    ("private", "key"),
    ("access", "key"),
    ("secret", "key"),
    ("access", "token"),
    ("auth", "token"),
    ("refresh", "token"),
];

/** Folder names holding database migrations */
const MIGRATION_FOLDERS: &[&str] = &[
    "migrations",
    "migration",
    "migrate",
    "alembic",
    "flyway",
    "liquibase",
];

/** Folder names holding infrastructure definitions */
const INFRA_FOLDERS: &[&str] = &[
    "terraform",
    "infra",
    "infrastructure",
    "k8s",
    "kubernetes",
    "helm",
    "charts",
    "ansible",
    "cloudformation",
    "pulumi",
    "deploy",
    "deployment",
];

/** Configuration file extensions considered for production configuration */
const CONFIG_EXTENSIONS: &[&str] = &[
    "yml",
    "yaml",
    "json",
    "properties",
    "toml",
    "ini",
    "conf",
    "env",
    "cfg",
    "xml",
];

/** Callers at which a function counts as central */
const CENTRAL_CALLERS: usize = 5;

/** How sure the heuristics are that a candidate deserves protection
 * Variants
    - Low - one weak signal
    - Medium - a domain word in the name, a Sensitive zone, or a protected-kind region
    - High - an explicit Critical or Restricted zone, or a domain word in the name confirmed by
      context, centrality, tests, or ownership
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    /** Return the confidence keyword
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /** Parse a confidence keyword
     * Input
        - value: &str - keyword
     * Output
        - Result<Confidence, String>
        - Error naming the accepted keywords
    */
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => Err(format!(
                "invalid confidence '{value}'; expected high, medium, or low"
            )),
        }
    }
}

/** One observed signal, named after the heuristic that produced it
 * Fields
    - signal: String - heuristic name, such as payment_name or zone_critical
    - detail: String - human-readable evidence
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Signal {
    pub(crate) signal: String,
    pub(crate) detail: String,
}

/** A candidate for protection
 * Fields
    - id: String - the entity id, or "region:SIGNAL:PATH" for a region
    - candidate: String - display name (qualified name or path)
    - kind: &'static str - "symbol" or "region"
    - signals: Vec<Signal> - every signal observed
    - confidence: Confidence - derived from the signals by fixed rules
    - suggested_rule: Option<String> - AgentScript statement, when one would resolve
    - suggested_zone: Option<String> - zone definition, for regions no rule can target
    - note: Option<String> - why there is no rule, when there is none
    - covered_by: Vec<String> - existing contract clauses already covering it
    - entities: Vec<String> - affected entity ids
    - files: Vec<String> - affected file paths
*/
pub(crate) struct Candidate {
    pub(crate) id: String,
    pub(crate) candidate: String,
    pub(crate) kind: &'static str,
    pub(crate) signals: Vec<Signal>,
    pub(crate) confidence: Confidence,
    pub(crate) suggested_rule: Option<String>,
    pub(crate) suggested_zone: Option<String>,
    pub(crate) note: Option<String>,
    pub(crate) covered_by: Vec<String>,
    pub(crate) entities: Vec<String>,
    pub(crate) files: Vec<String>,
}

impl Candidate {
    /** Join the signals' evidence into one reason
     * Input
        - None (uses self)
     * Output
        - String such as "criticality=Critical (zone payments) + payment name 'charge'"
    */
    pub(crate) fn reason(&self) -> String {
        self.signals
            .iter()
            .map(|signal| signal.detail.as_str())
            .collect::<Vec<_>>()
            .join(" + ")
    }

    /** Serialize the candidate
     * Input
        - None (uses self)
     * Output
        - Value JSON object
    */
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "candidate": self.candidate,
            "kind": self.kind,
            "reason": self.reason(),
            "confidence": self.confidence.name(),
            "signals": self.signals.iter().map(|signal| json!({"signal": signal.signal, "detail": signal.detail})).collect::<Vec<_>>(),
            "suggestion": "preserve",
            "suggested_rule": self.suggested_rule,
            "suggested_zone": self.suggested_zone,
            "note": self.note,
            "covered_by": self.covered_by,
            "affected_entities": self.entities,
            "affected_files": self.files,
        })
    }
}

/** Describe every heuristic, for output that must not overstate what was understood
 * Input
    - None
 * Output
    - Value JSON array of {signal, meaning}
*/
pub(crate) fn heuristics() -> Value {
    json!([
        {"signal": "zone_critical / zone_restricted / zone_sensitive", "meaning": "a zone in .crane/zones gives the code that criticality"},
        {"signal": "payment_name / auth_name / secrets_name", "meaning": "the symbol's own name contains a word from that vocabulary"},
        {"signal": "payment_context / auth_context / secrets_context", "meaning": "the enclosing type, module, service, or folder name contains such a word"},
        {"signal": "high_centrality", "meaning": "at least 5 resolved callers, or 3 from at least 2 other modules"},
        {"signal": "tested", "meaning": "a test calls it"},
        {"signal": "owned", "meaning": "CODEOWNERS assigns it owners other than the repository-wide default"},
        {"signal": "migration / infrastructure / production_config / secrets_config", "meaning": "the file path or name follows a common convention for that kind of file"},
    ])
}

/** Find the vocabulary word in a list of words, accepting plurals
 * Input
    - words: &[String] - lowercase words
    - vocabulary: &[&'static str] - vocabulary
 * Output
    - Option<&'static str>
*/
fn find_word(words: &[String], vocabulary: &[&'static str]) -> Option<&'static str> {
    words.iter().find_map(|word| {
        vocabulary.iter().copied().find(|item| {
            word == item
                || word.strip_suffix('s') == Some(item)
                || word.strip_suffix("es") == Some(item)
        })
    })
}

/** Find the domains a text names: payment, auth, or secrets, with the matched word
 * Input
    - text: &str - identifier or path
 * Output
    - Vec<(&'static str, String)> domain and matched word
*/
pub(crate) fn domains(text: &str) -> Vec<(&'static str, String)> {
    let words = words(text);
    let mut found = Vec::new();
    if let Some(word) = find_word(&words, PAYMENT) {
        found.push(("payment", word.to_string()));
    }
    if let Some(word) = find_word(&words, AUTH) {
        found.push(("auth", word.to_string()));
    }
    let pair = words.windows(2).find_map(|pair| {
        SECRET_PAIRS
            .iter()
            .find(|(first, second)| pair[0] == *first && pair[1].trim_end_matches('s') == *second)
            .map(|(first, second)| format!("{first} {second}"))
    });
    if let Some(word) = find_word(&words, SECRETS).map(String::from).or(pair) {
        found.push(("secrets", word));
    }
    found
}

/** Return the owner set most files share when it covers more than half of the owned files: the
 * repository-wide default, which says nothing about a particular file
 * Input
    - inventory: &Inventory - inventory with owners
 * Output
    - Option<Vec<String>>
*/
fn default_owners(inventory: &Inventory) -> Option<Vec<String>> {
    let mut counts: BTreeMap<&Vec<String>, usize> = BTreeMap::new();
    let mut owned = 0;
    for info in &inventory.graph.files {
        if !info.owners.is_empty() {
            owned += 1;
            *counts.entry(&info.owners).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then(right.0.cmp(left.0)))
        .filter(|(_, count)| *count * 2 > owned)
        .map(|(owners, _)| owners.clone())
}

/** Derive the confidence of a symbol candidate from its signals by fixed rules: High for an
 * explicit Critical or Restricted zone, or a domain word in the name confirmed by domain context,
 * centrality, tests, or specific ownership; Medium for a domain word in the name alone, a
 * Sensitive zone, or domain context with centrality; Low for domain context or centrality alone
 * Input
    - signals: &[Signal] - observed signals
 * Output
    - Option<Confidence>, None when nothing points at the symbol
*/
pub(crate) fn symbol_confidence(signals: &[Signal]) -> Option<Confidence> {
    let has =
        |predicate: &dyn Fn(&str) -> bool| signals.iter().any(|signal| predicate(&signal.signal));
    let zoned_high = has(&|signal| signal == "zone_critical" || signal == "zone_restricted");
    let name = has(&|signal| signal.ends_with("_name"));
    let context = has(&|signal| signal.ends_with("_context"));
    let central = has(&|signal| signal == "high_centrality");
    let supported = context || central || has(&|signal| signal == "tested" || signal == "owned");
    if zoned_high || (name && supported) {
        Some(Confidence::High)
    } else if name || has(&|signal| signal == "zone_sensitive") || (context && central) {
        Some(Confidence::Medium)
    } else if context || central {
        Some(Confidence::Low)
    } else {
        None
    }
}

/** Find candidate symbols: non-test functions and methods (and variables or constants whose
 * names concern secrets) with at least one signal
 * Input
    - inventory: &Inventory - inventory with relationships, owners, and contracts
    - zones: Option<&Resolution> - resolved zones, when Crane is initialized
 * Output
    - Vec<Candidate>
*/
fn symbol_candidates(inventory: &Inventory, zones: Option<&Resolution>) -> Vec<Candidate> {
    let graph = &inventory.graph;
    let snapshot = &inventory.snapshot;
    let default = default_owners(inventory);
    let mut candidates = Vec::new();
    for (index, entity) in graph.entities.iter().enumerate() {
        let symbol = Graph::symbol(snapshot, entity);
        let info = &graph.files[entity.file];
        if entity.test || info.test {
            continue;
        }
        let own = domains(&symbol.name);
        let secret_data = matches!(symbol.kind.name(), "variable" | "data")
            && own.iter().any(|(domain, _)| *domain == "secrets");
        if !symbol.kind.callable() && !secret_data {
            continue;
        }
        let path = &snapshot.files[entity.file].path;
        let mut signals = Vec::new();
        if let Some(effective) = zones.and_then(|zones| zones.entities.get(&index)) {
            let names = effective
                .zones
                .iter()
                .map(|zone| {
                    zones.map_or(String::new(), |zones| {
                        zones.zones[*zone].zone.zone_id.clone()
                    })
                })
                .collect::<Vec<_>>()
                .join(", ");
            let signal = match effective.criticality {
                Criticality::Restricted => Some("zone_restricted"),
                Criticality::Critical => Some("zone_critical"),
                Criticality::Sensitive => Some("zone_sensitive"),
                Criticality::Routine => None,
            };
            if let Some(signal) = signal {
                let label = effective.criticality.name();
                let mut chars = label.chars();
                let label = chars
                    .next()
                    .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                    .unwrap_or_default();
                signals.push(Signal {
                    signal: signal.into(),
                    detail: format!("criticality={label} (zone {names})"),
                });
            }
        }
        for (domain, word) in &own {
            signals.push(Signal {
                signal: format!("{domain}_name"),
                detail: format!("{domain} name '{word}'"),
            });
        }
        let enclosing = symbol
            .qualified
            .strip_suffix(&symbol.name)
            .unwrap_or_default()
            .trim_end_matches('.');
        let module = &graph.modules[entity.module];
        let service = &graph.services[info.service];
        let places = [
            ("type", enclosing.to_string()),
            ("module", module.name.clone()),
            ("service", service.path.clone()),
            ("folder", folder_of(path)),
        ];
        let mut seen = BTreeSet::new();
        for (place, name) in places {
            for (domain, word) in domains(&name) {
                if seen.insert(domain) {
                    let subsystem = if domain == "payment" {
                        "payment subsystem"
                    } else {
                        domain
                    };
                    signals.push(Signal {
                        signal: format!("{domain}_context"),
                        detail: format!("{subsystem} ({place} {name}, word '{word}')"),
                    });
                }
            }
        }
        let other_modules = entity
            .callers
            .iter()
            .map(|caller| graph.entities[*caller].module)
            .filter(|module| *module != entity.module)
            .collect::<BTreeSet<_>>()
            .len();
        if entity.callers.len() >= CENTRAL_CALLERS
            || (entity.callers.len() >= 3 && other_modules >= 2)
        {
            signals.push(Signal {
                signal: "high_centrality".into(),
                detail: format!(
                    "{} callers from {} other modules",
                    entity.callers.len(),
                    other_modules
                ),
            });
        }
        if signals.is_empty() {
            continue;
        }
        if let Some(test) = entity.tested_by.first() {
            signals.push(Signal {
                signal: "tested".into(),
                detail: format!("tested by {test}"),
            });
        }
        if !info.owners.is_empty() && default.as_ref() != Some(&info.owners) {
            signals.push(Signal {
                signal: "owned".into(),
                detail: format!("owners {}", info.owners.join(" ")),
            });
        }
        let Some(confidence) = symbol_confidence(&signals) else {
            continue;
        };
        let (suggested_rule, note) = match inventory.suggestion(entity) {
            Ok(rule) => (Some(rule), None),
            Err(note) => (None, Some(note.to_string())),
        };
        candidates.push(Candidate {
            id: entity.id.clone(),
            candidate: symbol.qualified.clone(),
            kind: "symbol",
            signals,
            confidence,
            suggested_rule,
            suggested_zone: None,
            note,
            covered_by: entity.contracts.clone(),
            entities: vec![entity.id.clone()],
            files: vec![path.clone()],
        });
    }
    candidates
}

/** Classify a file path into a protected-kind region, if it follows a convention: database
 * migrations, infrastructure definitions, production configuration, or secret configuration;
 * the region is the convention's folder, or the file itself
 * Input
    - path: &str - repository-relative path
 * Output
    - Option<(&'static str, String, bool)> signal, region path, and whether the region is a folder
*/
pub(crate) fn region_of(path: &str) -> Option<(&'static str, String, bool)> {
    let parts = path.split('/').collect::<Vec<_>>();
    let name = parts.last().copied().unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    let folder_up_to = |index: usize| parts[..=index].join("/");
    let folders = &parts[..parts.len() - 1];
    if let Some(index) = folders
        .iter()
        .position(|folder| MIGRATION_FOLDERS.contains(&folder.to_ascii_lowercase().as_str()))
    {
        return Some(("migration", folder_up_to(index), true));
    }
    if lower.ends_with(".sql") && lower.starts_with('v') && lower.contains("__") {
        return Some(("migration", folder_of(path), true));
    }
    if path.starts_with(".github/workflows/") {
        return Some(("infrastructure", ".github/workflows".into(), true));
    }
    if let Some(index) = folders
        .iter()
        .position(|folder| INFRA_FOLDERS.contains(&folder.to_ascii_lowercase().as_str()))
    {
        return Some(("infrastructure", folder_up_to(index), true));
    }
    if lower.ends_with(".tf")
        || lower.ends_with(".tfvars")
        || lower == "dockerfile"
        || lower.starts_with("dockerfile.")
        || lower.starts_with("docker-compose")
    {
        return Some(("infrastructure", path.into(), false));
    }
    let extension = lower.rsplit('.').next().unwrap_or_default();
    let config = CONFIG_EXTENSIONS.contains(&extension) || lower.starts_with(".env");
    let words = words(path);
    if config
        && words.iter().any(|word| {
            word == "secret" || word == "secrets" || word == "credentials" || word == "credential"
        })
    {
        return Some(("secrets_config", path.into(), false));
    }
    if config
        && words
            .iter()
            .any(|word| word == "prod" || word == "production")
    {
        return Some(("production_config", path.into(), false));
    }
    None
}

/** Find region candidates: files no AgentScript rule can target (or folders of them) following
 * migration, infrastructure, production-configuration, or secret-configuration conventions;
 * each gets a zone suggestion instead of a rule, and High confidence when a zone already marks it
 * Critical or Restricted, it has specific owners, or it holds secrets
 * Input
    - inventory: &Inventory - inventory
    - zones: Option<&Resolution> - resolved zones
 * Output
    - Vec<Candidate>
*/
fn region_candidates(inventory: &Inventory, zones: Option<&Resolution>) -> Vec<Candidate> {
    let graph = &inventory.graph;
    let snapshot = &inventory.snapshot;
    let default = default_owners(inventory);
    let mut regions: BTreeMap<(&'static str, String, bool), Vec<usize>> = BTreeMap::new();
    for (index, file) in snapshot.files.iter().enumerate() {
        if let Some(region) = region_of(&file.path) {
            regions.entry(region).or_default().push(index);
        }
    }
    regions
        .into_iter()
        .map(|((signal, path, folder), files)| {
            let detail = match signal {
                "migration" => "database migrations",
                "infrastructure" => "infrastructure definitions",
                "secrets_config" => "secret configuration",
                _ => "production configuration",
            };
            let mut signals = vec![Signal {
                signal: signal.into(),
                detail: format!("{detail} ({} {path})", if folder { "folder" } else { "file" }),
            }];
            let zoned = files
                .iter()
                .filter_map(|file| zones.and_then(|zones| zones.files.get(file)))
                .map(|effective| effective.criticality)
                .max();
            if let Some(criticality) = zoned.filter(|criticality| *criticality >= Criticality::Critical) {
                signals.push(Signal {
                    signal: format!("zone_{}", criticality.name()),
                    detail: format!("criticality={} (zone)", criticality.name()),
                });
            }
            let owners = files
                .iter()
                .map(|file| &graph.files[*file].owners)
                .find(|owners| !owners.is_empty() && default.as_ref() != Some(*owners));
            if let Some(owners) = owners {
                signals.push(Signal {
                    signal: "owned".into(),
                    detail: format!("owners {}", owners.join(" ")),
                });
            }
            let confidence = if signals.len() > 1 || signal == "secrets_config" {
                Confidence::High
            } else {
                Confidence::Medium
            };
            let name = words(&path).join("_");
            let selector = if folder {
                format!("select folder {path};")
            } else {
                format!("select path {path};")
            };
            let zone = format!(
                "zone {} {{\n    criticality critical;\n    autonomy assisted;\n    {selector}\n}}\n",
                if name.is_empty() { "region".to_string() } else { name }
            );
            let entities = graph
                .entities
                .iter()
                .filter(|entity| files.contains(&entity.file))
                .map(|entity| entity.id.clone())
                .collect::<Vec<_>>();
            Candidate {
                id: format!("region:{signal}:{path}"),
                candidate: path.clone(),
                kind: "region",
                signals,
                confidence,
                suggested_rule: None,
                suggested_zone: Some(zone),
                note: Some("AgentScript rules target code items and this region holds no targetable item, so it is protected with a zone".into()),
                covered_by: Vec::new(),
                entities,
                files: files.iter().map(|file| snapshot.files[*file].path.clone()).collect(),
            }
        })
        .collect()
}

/** Find every candidate for protection, ordered deterministically: confidence (highest first),
 * then symbols before regions, then id
 * Input
    - inventory: &Inventory - inventory
    - zones: Option<&Resolution> - resolved zones, when Crane is initialized
 * Output
    - Vec<Candidate>
*/
pub(crate) fn candidates(inventory: &Inventory, zones: Option<&Resolution>) -> Vec<Candidate> {
    let mut candidates = symbol_candidates(inventory, zones);
    candidates.extend(region_candidates(inventory, zones));
    candidates.sort_by(|left, right| {
        right
            .confidence
            .cmp(&left.confidence)
            .then(left.kind.cmp(right.kind).reverse())
            .then(left.id.cmp(&right.id))
    });
    candidates
}
