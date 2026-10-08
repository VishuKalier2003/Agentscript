use std::fs;
use std::path::Path;

use serde_json::json;

use crate::util::{io_error, sha256, validate_identifier};

/** How much an agent may do on its own inside a zone, from least to most; zones only ever lower
 * it (the derived ordering is from most restrictive to least)
 * Variants
    - Observe - the agent may read but should not change anything
    - Assisted - the agent proposes and a human approves each change
    - Delegated - the agent changes within its task, a human reviews the result
    - Autonomous - the agent changes on its own within its contracts
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Autonomy {
    Observe,
    Assisted,
    Delegated,
    Autonomous,
}

/** How important the resources in a zone are, from least to most
 * Variants
    - Routine - ordinary code, such as tests or tooling
    - Sensitive - code whose mistakes are costly
    - Critical - code the business depends on, such as payments
    - Restricted - code agents must not change on their own, such as authentication
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Criticality {
    Routine,
    Sensitive,
    Critical,
    Restricted,
}

/** Whether a zone is healthy, from best to worst
 * Variants
    - Active - normal operation
    - Degraded - something is wrong (declared by a human, or the zone does not fully resolve);
      autonomy is capped at Assisted
    - Quarantined - declared off limits; autonomy is capped at Observe
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum SafetyState {
    Active,
    Degraded,
    Quarantined,
}

/** Define name and parse for a keyword enum
 * Input
    - enum type, and its variants with their keywords
 * Output
    - impl block with name, parse, and ALL
*/
macro_rules! keywords {
    ($type:ident { $($variant:ident => $word:literal),+ $(,)? }) => {
        impl $type {
            /** Every value, in order */
            pub(crate) const ALL: &'static [$type] = &[$($type::$variant),+];

            /** Return the keyword written in zone files and output
             * Input
                - None (uses self)
             * Output
                - &'static str
            */
            pub(crate) fn name(self) -> &'static str {
                match self {
                    $($type::$variant => $word),+
                }
            }

            /** Parse a keyword, ignoring case
             * Input
                - value: &str - keyword
             * Output
                - Result<Self, String>
                - Error naming the accepted keywords
            */
            pub(crate) fn parse(value: &str) -> Result<Self, String> {
                let lower = value.to_ascii_lowercase();
                Self::ALL
                    .iter()
                    .copied()
                    .find(|item| item.name() == lower)
                    .ok_or_else(|| {
                        format!(
                            "invalid {} '{value}'; expected {}",
                            stringify!($type).to_ascii_lowercase(),
                            Self::ALL.iter().map(|item| item.name()).collect::<Vec<_>>().join(", ")
                        )
                    })
            }
        }
    };
}

keywords!(Autonomy {
    Observe => "observe",
    Assisted => "assisted",
    Delegated => "delegated",
    Autonomous => "autonomous",
});

keywords!(Criticality {
    Routine => "routine",
    Sensitive => "sensitive",
    Critical => "critical",
    Restricted => "restricted",
});

keywords!(SafetyState {
    Active => "active",
    Degraded => "degraded",
    Quarantined => "quarantined",
});

impl Criticality {
    /** Return the highest autonomy this criticality allows, whatever a zone declares
     * Input
        - None (uses self)
     * Output
        - Autonomy
    */
    pub(crate) fn autonomy_cap(self) -> Autonomy {
        match self {
            Self::Routine => Autonomy::Autonomous,
            Self::Sensitive => Autonomy::Delegated,
            Self::Critical => Autonomy::Assisted,
            Self::Restricted => Autonomy::Observe,
        }
    }
}

impl SafetyState {
    /** Return the highest autonomy this state allows
     * Input
        - None (uses self)
     * Output
        - Autonomy
    */
    pub(crate) fn autonomy_cap(self) -> Autonomy {
        match self {
            Self::Active => Autonomy::Autonomous,
            Self::Degraded => Autonomy::Assisted,
            Self::Quarantined => Autonomy::Observe,
        }
    }
}

/** What a selector names; semantic kinds survive file moves, path kinds do not
 * Variants
    - Symbol - a symbol by id ("java:com.acme.Pay.charge"), by file and qualified name
      ("payments/service.py::PaymentService.charge", or "...::PaymentService.*" for its members),
      or by qualified name ("Pay.charge"); it restricts changes to that symbol only
    - Module - a module by id ("java:com.acme.payments") or name ("com.acme.payments")
    - Service - a service by path ("services/payments") or name ("payments")
    - Subsystem - every service, module, and folder with a name segment matching ("payments")
    - Policy - entities covered by a policy ("payments") or one of its rules ("payments:target")
    - Tests - every test file and test symbol
    - Folder - a folder and everything below it (layout-dependent)
    - Path - files matching a glob (layout-dependent)
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum SelectorKind {
    Symbol,
    Module,
    Service,
    Subsystem,
    Policy,
    Tests,
    Folder,
    Path,
}

keywords!(SelectorKind {
    Symbol => "symbol",
    Module => "module",
    Service => "service",
    Subsystem => "subsystem",
    Policy => "policy",
    Tests => "tests",
    Folder => "folder",
    Path => "path",
});

impl SelectorKind {
    /** Check whether the selector names things by meaning rather than by location
     * Input
        - None (uses self)
     * Output
        - bool, false for folder and path selectors
    */
    pub(crate) fn semantic(self) -> bool {
        !matches!(self, Self::Folder | Self::Path)
    }
}

/** One selector of a zone
 * Fields
    - kind: SelectorKind - what it names
    - value: String - the name or pattern ("*" is a wildcard), empty for tests
*/
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Selector {
    pub(crate) kind: SelectorKind,
    pub(crate) value: String,
}

impl Selector {
    /** Return the selector as written in a zone file, its stable key
     * Input
        - None (uses self)
     * Output
        - String such as "subsystem payments"
    */
    pub(crate) fn text(&self) -> String {
        if self.value.is_empty() {
            self.kind.name().into()
        } else {
            format!("{} {}", self.kind.name(), self.value)
        }
    }
}

/** A zone: a named, persistent classification of part of the repository, defined by selectors
 * rather than by file layout and resolved again against the inventory on every run; it is an
 * input to authorization that can only restrict, never a grant
 * Fields
    - zone_id: String - zone name, unique in the repository
    - selectors: Vec<Selector> - what the zone covers, sorted and unique
    - criticality: Criticality - how important its resources are
    - default_autonomy: Autonomy - autonomy declared for it (capped by criticality and state)
    - safety_state: SafetyState - declared state
    - policy_reference: Option<String> - policy that governs it, if any
    - version: String - SHA-256 of the canonical definition, so reformatting a zone file keeps it
    - source: String - file the zone was read from
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Zone {
    pub(crate) zone_id: String,
    pub(crate) selectors: Vec<Selector>,
    pub(crate) criticality: Criticality,
    pub(crate) default_autonomy: Autonomy,
    pub(crate) safety_state: SafetyState,
    pub(crate) policy_reference: Option<String>,
    pub(crate) version: String,
    pub(crate) source: String,
}

/** Compute a zone's version from its canonical definition (sorted selectors, keywords)
 * Input
    - zone: &Zone - zone with every field but version set
 * Output
    - String SHA-256 digest
*/
fn version_of(zone: &Zone) -> String {
    let canonical = json!({
        "zone_id": zone.zone_id,
        "selectors": zone.selectors.iter().map(Selector::text).collect::<Vec<_>>(),
        "criticality": zone.criticality.name(),
        "default_autonomy": zone.default_autonomy.name(),
        "safety_state": zone.safety_state.name(),
        "policy_reference": zone.policy_reference,
    });
    sha256(canonical.to_string().as_bytes())
}

/** One statement inside a zone block: its line number, keyword, and value */
type Statement = (usize, String, String);

/** Parse zone file text: any number of blocks of the form
 *     zone NAME {
 *         criticality routine|sensitive|critical|restricted;
 *         autonomy observe|assisted|delegated|autonomous;
 *         state active|degraded|quarantined;     (optional, default active)
 *         policy POLICY;                         (optional)
 *         select KIND VALUE;                     (one or more)
 *     }
 * where blank lines, lines starting with "#" or "//", and comments after whitespace are ignored,
 * and every statement ends with ";"
 * Input
    - content: &str - file text
    - source: &str - file name, recorded on each zone and used in errors
 * Output
    - Result<Vec<Zone>, String>
    - Error with the line number for any malformed or incomplete zone
*/
pub(crate) fn parse(content: &str, source: &str) -> Result<Vec<Zone>, String> {
    let mut zones = Vec::new();
    let mut current: Option<(usize, String, Vec<Statement>)> = None;
    for (index, raw) in content.lines().enumerate() {
        let number = index + 1;
        let line = [" #", "	#", " //", "	//"]
            .iter()
            .filter_map(|marker| raw.find(marker))
            .min()
            .map_or(raw, |end| &raw[..end])
            .trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        let error = |message: String| format!("{source}: line {number}: {message}");
        match current.as_mut() {
            None => {
                let name = line
                    .strip_prefix("zone ")
                    .and_then(|rest| rest.strip_suffix('{'))
                    .map(str::trim)
                    .ok_or_else(|| error("expected 'zone NAME {'".into()))?;
                validate_identifier(name).map_err(error)?;
                current = Some((number, name.into(), Vec::new()));
            }
            Some(_) if line == "}" => {
                let (start, name, statements) = current.take().unwrap_or_default();
                zones.push(build(start, name, statements, source)?);
            }
            Some((_, _, statements)) => {
                let statement = line
                    .strip_suffix(';')
                    .ok_or_else(|| error("statements must end with ';'".into()))?
                    .trim();
                let (keyword, value) = statement
                    .split_once(char::is_whitespace)
                    .map(|(keyword, value)| (keyword, value.trim()))
                    .unwrap_or((statement, ""));
                statements.push((number, keyword.into(), value.into()));
            }
        }
    }
    if let Some((start, name, _)) = current {
        return Err(format!("{source}: line {start}: zone {name} is not closed"));
    }
    Ok(zones)
}

/** Build one zone from its statements, rejecting unknown, repeated, or missing statements and
 * malformed values
 * Input
    - start: usize - line of the zone declaration
    - zone_id: String - zone name
    - statements: Vec<Statement> - line, keyword, and value of each statement
    - source: &str - file name
 * Output
    - Result<Zone, String>
    - Error with the line number
*/
fn build(
    start: usize,
    zone_id: String,
    statements: Vec<Statement>,
    source: &str,
) -> Result<Zone, String> {
    let mut criticality = None;
    let mut autonomy = None;
    let mut state = None;
    let mut policy = None;
    let mut selectors = Vec::new();
    for (number, keyword, value) in statements {
        let error = |message: String| format!("{source}: line {number}: {message}");
        let once = |seen: bool| {
            if seen {
                Err(error(format!("'{keyword}' is given twice")))
            } else {
                Ok(())
            }
        };
        match keyword.as_str() {
            "criticality" => {
                once(criticality.is_some())?;
                criticality = Some(Criticality::parse(&value).map_err(error)?);
            }
            "autonomy" => {
                once(autonomy.is_some())?;
                autonomy = Some(Autonomy::parse(&value).map_err(error)?);
            }
            "state" => {
                once(state.is_some())?;
                state = Some(SafetyState::parse(&value).map_err(error)?);
            }
            "policy" => {
                once(policy.is_some())?;
                validate_identifier(&value).map_err(error)?;
                policy = Some(value);
            }
            "select" => {
                let (kind, pattern) = value
                    .split_once(char::is_whitespace)
                    .map(|(kind, pattern)| (kind, pattern.trim()))
                    .unwrap_or((value.as_str(), ""));
                let kind = SelectorKind::parse(kind).map_err(error)?;
                if (kind == SelectorKind::Tests) != pattern.is_empty() {
                    return Err(error(if kind == SelectorKind::Tests {
                        "'select tests' takes no value".into()
                    } else {
                        format!("'select {}' needs a value", kind.name())
                    }));
                }
                if pattern.chars().any(char::is_whitespace) {
                    return Err(error("a selector value cannot contain spaces".into()));
                }
                selectors.push(Selector {
                    kind,
                    value: pattern.into(),
                });
            }
            other => {
                return Err(error(format!(
                    "unknown statement '{other}'; expected criticality, autonomy, state, policy, or select"
                )))
            }
        }
    }
    let missing = |what: &str| format!("{source}: line {start}: zone {zone_id} has no {what}");
    selectors.sort();
    selectors.dedup();
    if selectors.is_empty() {
        return Err(missing("select statement"));
    }
    let mut zone = Zone {
        criticality: criticality.ok_or_else(|| missing("criticality"))?,
        default_autonomy: autonomy.ok_or_else(|| missing("autonomy"))?,
        safety_state: state.unwrap_or(SafetyState::Active),
        policy_reference: policy,
        selectors,
        zone_id,
        version: String::new(),
        source: source.into(),
    };
    zone.version = version_of(&zone);
    Ok(zone)
}

/** Load every zone in the .zone files of .crane/zones, in file name order; a malformed file is reported and
 * contributes no zones, and a zone id defined twice keeps its first definition
 * Input
    - crane: &Path - the .crane directory
 * Output
    - Result<(Vec<Zone>, Vec<String>), String> zones and problems (malformed files, duplicates)
    - Error if the zones directory exists but cannot be read
*/
pub(crate) fn load(crane: &Path) -> Result<(Vec<Zone>, Vec<String>), String> {
    let directory = crane.join("zones");
    if !directory.is_dir() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut paths = fs::read_dir(&directory)
        .map_err(io_error)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("zone"))
        .collect::<Vec<_>>();
    paths.sort();
    let mut zones: Vec<Zone> = Vec::new();
    let mut problems = Vec::new();
    for path in paths {
        let source = format!(
            "zones/{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
        );
        let parsed = fs::read_to_string(&path)
            .map_err(|error| format!("{source}: {}", io_error(error)))
            .and_then(|content| parse(&content, &source));
        match parsed {
            Err(problem) => problems.push(problem),
            Ok(found) => {
                for zone in found {
                    if let Some(first) = zones.iter().find(|other| other.zone_id == zone.zone_id) {
                        problems.push(format!(
                            "{}: zone {} is already defined in {}; this definition is ignored",
                            zone.source, zone.zone_id, first.source
                        ));
                    } else {
                        zones.push(zone);
                    }
                }
            }
        }
    }
    Ok((zones, problems))
}

/** Compute the version of a zone set from every zone's id and version
 * Input
    - zones: &[Zone] - zones
 * Output
    - String SHA-256 digest
*/
pub(crate) fn set_version(zones: &[Zone]) -> String {
    let mut manifest = String::from("crane-zones 1\n");
    let mut lines = zones
        .iter()
        .map(|zone| format!("zone {} {}\n", zone.zone_id, zone.version))
        .collect::<Vec<_>>();
    lines.sort();
    manifest.extend(lines);
    sha256(manifest.as_bytes())
}
