use crate::repository::ensure_initialized;
use crate::zones::{inspect, Zones};

/** Number of entities listed per zone in the summary */
const LISTED: usize = 5;

/** Inspect the repository's zones, by resolving every zone in .crane/zones against the current
 * inventory and printing them as JSON or as a summary (one zone in full when a zone id is
 * given); zones are read-only here and can only restrict authorization, never grant it
 * Input
    - args: &[String] - an optional zone id, and --json
 * Output
    - Result<(), String>
    - Error if Crane is not initialized, an option or zone id is unknown, or a zone file is
      malformed (after printing everything that did resolve)
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    let json = args.iter().any(|argument| argument == "--json");
    let mut names = args.iter().filter(|argument| argument.as_str() != "--json");
    let only = names.next().map(String::as_str);
    if let Some(extra) = names.next() {
        return Err(format!(
            "unexpected argument '{extra}'; use 'crane zones [ZONE_ID] [--json]'"
        ));
    }
    if only.is_some_and(|name| name.starts_with('-')) {
        return Err(format!(
            "unknown zones option '{}'; use 'crane zones [ZONE_ID] [--json]'",
            only.unwrap_or_default()
        ));
    }
    let zones = inspect()?;
    if let Some(name) = only {
        if !zones
            .resolution
            .zones
            .iter()
            .any(|result| result.zone.zone_id == name)
        {
            return Err(format!("no zone '{name}' in .crane/zones"));
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&zones.to_json(only)).map_err(|error| error.to_string())?
        );
    } else {
        print!("{}", render(&zones, only));
    }
    if zones.problems.is_empty() {
        Ok(())
    } else {
        Err("one or more zone definitions are invalid".into())
    }
}

/** Render the zones for a terminal: each zone with its declared and effective values and its
 * selectors' resolution, then unresolved selectors and conflicts; with a zone id, every
 * entity and file of that zone with its combined constraints
 * Input
    - zones: &Zones - resolved zones
    - only: Option<&str> - zone to show in full
 * Output
    - String ending in a newline
*/
fn render(zones: &Zones, only: Option<&str>) -> String {
    let graph = &zones.inventory.graph;
    let snapshot = &zones.inventory.snapshot;
    let mut out = String::from(
        "Crane zones (an input to authorization: zones only restrict, they never grant)\n",
    );
    out.push_str(&format!("Zone set version: {}\n", zones.version));
    if zones.resolution.zones.is_empty() {
        out.push_str("No zones defined; add .zone files to .crane/zones (see LANGUAGE.md).\n");
    }
    for result in &zones.resolution.zones {
        let zone = &result.zone;
        if only.is_some_and(|name| name != zone.zone_id) {
            continue;
        }
        let declared = |effective: &str, declared: &str| {
            if effective == declared {
                effective.to_string()
            } else {
                format!("{effective} (declared {declared})")
            }
        };
        out.push_str(&format!(
            "\nzone {} [{}]\n  criticality {}, autonomy {}, state {}{}\n  {} entities, {} files; version {}\n",
            zone.zone_id,
            zone.source,
            zone.criticality.name(),
            declared(result.autonomy.name(), zone.default_autonomy.name()),
            declared(result.state.name(), zone.safety_state.name()),
            zone.policy_reference
                .as_ref()
                .map_or(String::new(), |policy| format!(", policy {policy}")),
            result.entities.len(),
            result.files.len(),
            zone.version
        ));
        for selector in &result.selectors {
            let mut line = format!("  select {}: {}", selector.selector.text(), selector.status);
            if !selector.targets.is_empty() {
                line.push_str(&format!(" -> {}", selector.targets.join(", ")));
            }
            if !selector.previous.is_empty() {
                line.push_str(&format!(" (was {})", selector.previous.join(", ")));
            }
            if !selector.candidates.is_empty() {
                line.push_str(&format!(
                    "; possibly renamed or moved to {}",
                    selector.candidates.join(", ")
                ));
            }
            if !selector.selector.kind.semantic() {
                line.push_str(" [layout-dependent]");
            }
            out.push_str(&line);
            out.push('\n');
        }
        let limit = if only.is_some() { usize::MAX } else { LISTED };
        for entity in result.entities.iter().take(limit) {
            let effective = &zones.resolution.entities[entity];
            out.push_str(&format!(
                "    {} ({}, {}, {})\n",
                graph.entities[*entity].id,
                effective.criticality.name(),
                effective.autonomy.name(),
                effective.state.name()
            ));
        }
        if result.entities.len() > limit {
            out.push_str(&format!(
                "    ... {} more (crane zones {})\n",
                result.entities.len() - limit,
                zone.zone_id
            ));
        }
        if only.is_some() {
            for file in &result.files {
                let effective = &zones.resolution.files[file];
                out.push_str(&format!(
                    "    file {} ({}, {}, {})\n",
                    snapshot.files[*file].path,
                    effective.criticality.name(),
                    effective.autonomy.name(),
                    effective.state.name()
                ));
            }
        }
    }
    let unresolved = zones
        .resolution
        .zones
        .iter()
        .filter(|result| only.is_none_or(|name| name == result.zone.zone_id))
        .flat_map(|result| {
            result
                .selectors
                .iter()
                .filter(|selector| selector.status != "resolved")
                .map(move |selector| (result, selector))
        })
        .collect::<Vec<_>>();
    out.push_str(&format!("\nUnresolved selectors ({}):\n", unresolved.len()));
    for (result, selector) in &unresolved {
        out.push_str(&format!(
            "  {}: select {} is {}\n",
            result.zone.zone_id,
            selector.selector.text(),
            selector.status
        ));
    }
    let conflicts = zones
        .resolution
        .conflicts
        .iter()
        .filter(|conflict| only.is_none_or(|name| conflict.zones.iter().any(|zone| zone == name)))
        .collect::<Vec<_>>();
    out.push_str(&format!("Conflicts ({}):\n", conflicts.len()));
    for conflict in conflicts {
        out.push_str(&format!("  {}: {}\n", conflict.kind, conflict.message));
    }
    for problem in &zones.problems {
        out.push_str(&format!("Invalid: {problem}\n"));
    }
    out
}
