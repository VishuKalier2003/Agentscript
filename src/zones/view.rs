// The agent's view of the zones: per file, every symbol with its effective criticality and
// autonomy, the decision an edit to it would get (OK, ASK, or DENY), and the contract markers on
// it. It is generated, read-only, and advisory: the decisions come from the same write_level the
// authority engine uses, but enforcement stays in authority.rs, so the view can never grant
// anything. It is written to .crane/runtime/zones.agent.md, which agents may read but not write.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use serde_json::{json, Value};

use super::model::{Autonomy, Criticality, SafetyState};
use super::{inspect, Zones};
use crate::agent_session::{Governance, SessionOptions};
use crate::authority::write_level;
use crate::inventory::graph::Graph;
use crate::repository::root;
use crate::session::ContractSession;
use crate::util::{io_error, now_unix};

/** The view file, relative to .crane */
pub(crate) const VIEW_FILE: &str = "runtime/zones.agent.md";

/** Most files the session-start context summarizes; the rest are only in the view file */
const CONTEXT_ROWS: usize = 15;

/** Whose view it is
 * Fields
    - governance: Governance - the frozen governance of the session, or a fresh one for new
      sessions
    - autonomy: Autonomy - the session's current autonomy mode
    - safety: SafetyState - the session's safety state
    - session: Option<String> - session id, None for new sessions
*/
pub(crate) struct Subject {
    pub(crate) governance: Governance,
    pub(crate) autonomy: Autonomy,
    pub(crate) safety: SafetyState,
    pub(crate) session: Option<String>,
}

impl Subject {
    /** The view for sessions started now without a task: the default autonomy mode (capped by
     * the organization) and the current zones
     * Input
        - None
     * Output
        - Result<Subject, String>
    */
    pub(crate) fn new_sessions() -> Result<Self, String> {
        let governance = SessionOptions {
            checkpoint: "baseline".into(),
            ..SessionOptions::default()
        }
        .governance(None)?;
        Ok(Self {
            autonomy: governance.autonomy,
            governance,
            safety: SafetyState::Active,
            session: None,
        })
    }

    /** The view for one contract session, with its frozen governance and current state
     * Input
        - session: &ContractSession - session
     * Output
        - Subject
    */
    pub(crate) fn of(session: &ContractSession) -> Self {
        let activity = session.activity();
        Self {
            governance: session.governance().clone(),
            autonomy: activity.state.autonomy,
            safety: activity.safety,
            session: Some(session.id().to_string()),
        }
    }
}

/** Turn "POLICY:RULE" coverage tags into contract markers
 * Input
    - tags: &[String] - tags of a symbol and its enclosing types
 * Output
    - Vec<String> such as "preserve(payments)" or "TARGET(task_pay_1830_v1)", sorted and unique
*/
fn markers(tags: &[String]) -> Vec<String> {
    tags.iter()
        .filter_map(|tag| tag.rsplit_once(':'))
        .map(|(policy, rule)| match rule {
            "target" => format!("TARGET({policy})"),
            other => format!("{other}({policy})"),
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/** Build the view: the files a zone touches, the task scope's files, and the files holding
 * contract-covered symbols (or one file), each with a row for the rest of the file and a row per
 * symbol; a symbol's row is the decision for an edit that changes it (and so its enclosing types)
 * Input
    - zones: &Zones - resolved zones and inventory
    - subject: &Subject - whose view
    - only: Option<&str> - repository-relative path of the one file to show
 * Output
    - Result<Value, String> {view_format, generated_at, session, autonomy, files: [{path, zones,
      rows: [{symbol, criticality, autonomy, decision, markers, reasons}]}]}
    - Error if the file is not in the repository
*/
pub(crate) fn build(zones: &Zones, subject: &Subject, only: Option<&str>) -> Result<Value, String> {
    let snapshot = &zones.inventory.snapshot;
    let graph = &zones.inventory.graph;
    let governance = &subject.governance;
    let index_of = snapshot
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let mut by_file: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (index, entity) in graph.entities.iter().enumerate() {
        by_file.entry(entity.file).or_default().push(index);
    }
    let files = match only {
        Some(path) => {
            let path = path.trim_start_matches("./").replace('\\', "/");
            vec![*index_of
                .get(path.as_str())
                .ok_or_else(|| format!("{path} is not a file of the repository inventory"))?]
        }
        None => governance
            .files
            .keys()
            .chain(governance.scope.iter().flatten())
            .filter_map(|path| index_of.get(path.as_str()).copied())
            .chain(
                graph
                    .entities
                    .iter()
                    .filter(|entity| !entity.contracts.is_empty())
                    .map(|entity| entity.file),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    };
    let row = |path: &str, symbol: Option<&str>, changed: BTreeSet<String>, tags: Vec<String>| {
        let written = vec![(path.to_string(), true, Some(changed.clone()))];
        let (level, mut reasons) =
            write_level(governance, subject.autonomy, subject.safety, "", &written);
        let criticality = governance
            .constraint_for(path, Some(&changed))
            .map_or(Criticality::Routine, |(constraint, _)| {
                constraint.criticality
            });
        let markers = markers(&tags);
        let preserved = markers.iter().any(|marker| marker.starts_with("preserve("));
        if preserved {
            reasons.insert(0, "a preserve clause protects it: a change is denied unless it restores the checkpoint version".into());
        }
        let decision = if preserved
            || level == Autonomy::Observe
            || subject.safety == SafetyState::Quarantined
        {
            "DENY"
        } else if level == Autonomy::Assisted {
            "ASK"
        } else {
            "OK"
        };
        json!({
            "symbol": symbol,
            "criticality": criticality.name(),
            "autonomy": level.name(),
            "decision": decision,
            "markers": markers,
            "reasons": reasons,
        })
    };
    let mut listed = Vec::new();
    for file in files {
        let path = snapshot.files[file].path.clone();
        let mut entities = by_file.get(&file).cloned().unwrap_or_default();
        entities.sort_by_key(|entity| Graph::symbol(snapshot, &graph.entities[*entity]).start_line);
        let names = entities
            .iter()
            .map(|entity| {
                Graph::symbol(snapshot, &graph.entities[*entity])
                    .qualified
                    .clone()
            })
            .collect::<BTreeSet<_>>();
        let tags_of = entities
            .iter()
            .map(|entity| {
                (
                    Graph::symbol(snapshot, &graph.entities[*entity])
                        .qualified
                        .clone(),
                    graph.entities[*entity].contracts.clone(),
                )
            })
            .fold(
                BTreeMap::<String, Vec<String>>::new(),
                |mut map, (name, tags)| {
                    map.entry(name).or_default().extend(tags);
                    map
                },
            );
        let mut rows = vec![row(&path, None, BTreeSet::new(), Vec::new())];
        for name in entities
            .iter()
            .map(|entity| {
                Graph::symbol(snapshot, &graph.entities[*entity])
                    .qualified
                    .clone()
            })
            .collect::<Vec<_>>()
            .into_iter()
            .fold(Vec::<String>::new(), |mut seen, name| {
                if !seen.contains(&name) {
                    seen.push(name);
                }
                seen
            })
        {
            // Changing a member changes every enclosing type that is a symbol of the file
            let parts = name.split('.').collect::<Vec<_>>();
            let changed = (1..=parts.len())
                .map(|count| parts[..count].join("."))
                .filter(|prefix| *prefix == name || names.contains(prefix))
                .collect::<BTreeSet<_>>();
            let tags = changed
                .iter()
                .flat_map(|prefix| tags_of.get(prefix).cloned().unwrap_or_default())
                .collect();
            rows.push(row(&path, Some(&name), changed, tags));
        }
        listed.push(json!({
            "path": path,
            "zones": governance.constraint(&path).map(|constraint| constraint.zones).unwrap_or_default(),
            "in_task_scope": governance.scope.as_ref().map(|scope| scope.contains(&path)),
            "rows": rows,
        }));
    }
    Ok(json!({
        "view_format": 1,
        "advisory": true,
        "generated_at": now_unix(),
        "session": subject.session,
        "task": subject.governance.task_contract.as_ref().map(|contract| contract["contract_id"].clone()),
        "autonomy": subject.autonomy.name(),
        "zone_set_version": zones.version,
        "files": listed,
    }))
}

/** Summarize one file of the view as a single line for the agent's context: the decision for
 * the rest of the file, then each symbol whose decision or values differ from it or that carries
 * a contract marker; None when everything in the file is plainly allowed
 * Input
    - file: &Value - file of the view
 * Output
    - Option<String> such as "pay.py: ASK (critical/assisted); Pay.charge: DENY preserve(core)"
*/
fn file_line(file: &Value) -> Option<String> {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    let rows = file["rows"].as_array()?;
    let rest = rows.first()?;
    let values = |row: &Value| format!("{}/{}", text(&row["criticality"]), text(&row["autonomy"]));
    let symbols = rows[1..]
        .iter()
        .filter_map(|row| {
            let markers = crate::human::strings(&row["markers"]).join(" ");
            let differs = row["decision"] != rest["decision"] || values(row) != values(rest);
            (differs || !markers.is_empty()).then(|| {
                format!(
                    "{}: {}{}{}",
                    text(&row["symbol"]),
                    text(&row["decision"]),
                    if values(row) == values(rest) {
                        String::new()
                    } else {
                        format!(" ({})", values(row))
                    },
                    if markers.is_empty() {
                        String::new()
                    } else {
                        format!(" {markers}")
                    }
                )
            })
        })
        .collect::<Vec<_>>();
    if rest["decision"] == "OK" && symbols.is_empty() {
        return None;
    }
    let mut line = format!(
        "{}: {} ({})",
        text(&file["path"]),
        text(&rest["decision"]),
        values(rest)
    );
    for symbol in symbols {
        line.push_str(&format!("; {symbol}"));
    }
    Some(line)
}

/** Render the view as Markdown
 * Input
    - view: &Value - result of build
 * Output
    - String
*/
pub(crate) fn markdown(view: &Value) -> String {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    let mut out = String::from("# Crane zone map (advisory, generated; do not edit)\n\n");
    out.push_str(&format!(
        "Generated {} for {}; autonomy mode {}; zone set {}.\n\n",
        crate::human::when(view["generated_at"].as_u64().unwrap_or(0)),
        view["session"]
            .as_str()
            .map_or("new sessions".to_string(), |session| format!(
                "session {session}"
            )),
        text(&view["autonomy"]),
        crate::human::code(view["zone_set_version"].as_str().unwrap_or_default())
    ));
    out.push_str("Crane enforces these decisions outside the model; this file only explains them. OK: the edit runs. ASK: a human approves the edit first. DENY: the edit is refused. A symbol's row is for an edit that changes that symbol; the first row is for the rest of the file. Files not listed are governed by the session's autonomy mode alone.\n");
    for file in view["files"].as_array().into_iter().flatten() {
        let zones = crate::human::strings(&file["zones"]);
        out.push_str(&format!(
            "\n## {}\nzones: {}{}\n\n| symbol | criticality | autonomy | decision | contract |\n|---|---|---|---|---|\n",
            text(&file["path"]),
            if zones.is_empty() { "none".to_string() } else { zones.join(", ") },
            if file["in_task_scope"] == true { "; in the task scope" } else { "" }
        ));
        for row in file["rows"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                row["symbol"].as_str().unwrap_or("(rest of file)"),
                text(&row["criticality"]),
                text(&row["autonomy"]),
                text(&row["decision"]),
                crate::human::strings(&row["markers"]).join(" ")
            ));
        }
    }
    out
}

/** Write the view to .crane/runtime/zones.agent.md
 * Input
    - view: &Value - result of build
 * Output
    - Result<(), String>
*/
pub(crate) fn save(view: &Value) -> Result<(), String> {
    let path = root()?.join(VIEW_FILE);
    let directory = path.parent().ok_or("invalid view path")?;
    fs::create_dir_all(directory).map_err(io_error)?;
    let ignore = directory.join(".gitignore");
    if !ignore.exists() {
        fs::write(ignore, "*\n").map_err(io_error)?;
    }
    let temporary = path.with_extension(format!("md.tmp{}", std::process::id()));
    fs::write(&temporary, markdown(view)).map_err(io_error)?;
    fs::rename(&temporary, &path).map_err(io_error)
}

/** Regenerate the view for new sessions after zones or contracts changed, when zones are in use
 * (a view exists or any zone file does); a failure is reported, never fatal, since the view is
 * advisory
 * Input
    - None
 * Output
    - None
*/
pub(crate) fn refresh() {
    let used = root().is_ok_and(|crane| {
        crane.join(VIEW_FILE).exists()
            || fs::read_dir(crane.join("zones")).is_ok_and(|entries| {
                entries.filter_map(|entry| entry.ok()).any(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "zone")
                })
            })
    });
    if !used {
        return;
    }
    let result = inspect().and_then(|zones| {
        let subject = Subject::new_sessions()?;
        save(&build(&zones, &subject, None)?)
    });
    if let Err(error) = result {
        eprintln!("crane: the zone map view could not be regenerated: {error}");
    }
}

/** Generate and save a session's view and return the lines its start context carries (after
 * POINTER): one line per file whose edits need approval, are denied, or touch contract
 * markers (of the task scope's files when a task is bound), at most CONTEXT_ROWS of them; None
 * when the session has neither zones nor a task
 * Input
    - session: &ContractSession - session
 * Output
    - Result<Option<Vec<String>>, String>
*/
pub(crate) fn for_session(session: &ContractSession) -> Result<Option<Vec<String>>, String> {
    let governance = session.governance();
    if governance.zone_set_version.is_none() && governance.scope.is_none() {
        return Ok(None);
    }
    let zones = inspect()?;
    let view = build(&zones, &Subject::of(session), None)?;
    save(&view)?;
    let files = view["files"].as_array().cloned().unwrap_or_default();
    let tasked = governance.scope.is_some();
    // One line per informative file, of the task scope's files when a task is bound
    let lines = files
        .iter()
        .filter(|file| !tasked || file["in_task_scope"] == true)
        .filter_map(file_line)
        .collect::<Vec<_>>();
    let mut out = lines
        .iter()
        .take(CONTEXT_ROWS)
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>();
    if lines.len() > CONTEXT_ROWS {
        out.push(format!(
            "- ({} more files are in the full view)",
            lines.len() - CONTEXT_ROWS
        ));
    }
    Ok(Some(out))
}

/** Point the agent at its zone view; Crane decides outside the model */
pub(crate) const POINTER: &str = "per-symbol decisions (advisory; OK runs, ASK needs a human, DENY is refused): .crane/runtime/zones.agent.md";
