use crate::inventory::{discover, Inventory, Options};
use crate::proposals::engine::candidates;
use crate::proposals::{discovery_json, render as render_candidates};
use crate::repository::root;

/** Number of critical candidates, services, and modules listed in the human summary */
const LISTED: usize = 10;

/** Discover the repository's semantic inventory and print it, by building (or incrementally
 * refreshing) the inventory and printing either the full JSON form or a human summary; discovery
 * is advisory and never creates, changes, or enforces a policy
 * Input
    - args: &[String] - command arguments: --json for the machine-readable form, --full to
      ignore the cache, --policies for heuristic protection candidates instead of the inventory
 * Output
    - Result<(), String>
    - Error if the current directory is not in a Git repository or git fails
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    if let Some(unknown) = args
        .iter()
        .find(|argument| !matches!(argument.as_str(), "--json" | "--full" | "--policies"))
    {
        return Err(format!(
            "unknown discover option '{unknown}'; use 'crane discover [--json] [--full] [--policies]'"
        ));
    }
    let json = args.iter().any(|argument| argument == "--json");
    if args.iter().any(|argument| argument == "--policies") {
        // With Crane initialized the zones (an explicit organization statement) are signals too
        let found = if root().is_ok() {
            let zones = crate::zones::inspect()?;
            candidates(&zones.inventory, Some(&zones.resolution))
        } else {
            let inventory = discover(&Options {
                full: args.iter().any(|argument| argument == "--full"),
            })?;
            candidates(&inventory, None)
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&discovery_json(&found))
                    .map_err(|error| error.to_string())?
            );
        } else {
            print!("{}", render_candidates(&found));
        }
        return Ok(());
    }
    let inventory = discover(&Options {
        full: args.iter().any(|argument| argument == "--full"),
    })?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&inventory.to_json()).map_err(|error| error.to_string())?
        );
    } else {
        print!("{}", render(&inventory));
    }
    Ok(())
}

/** Render the human summary: repository size, languages, services and modules, critical
 * candidates with suggested rules, existing contract coverage, checkpoints, and index statistics
 * Input
    - inventory: &Inventory - discovery result
 * Output
    - String ending in a newline
*/
pub(crate) fn render(inventory: &Inventory) -> String {
    let graph = &inventory.graph;
    let mut out = String::from("Crane discover (advisory: nothing here is enforced)\n");
    let lines = inventory
        .snapshot
        .files
        .iter()
        .map(|file| file.outline.lines)
        .sum::<usize>();
    let head = inventory
        .head
        .as_deref()
        .map_or("no commits".to_string(), |head| {
            head.chars().take(12).collect()
        });
    out.push_str(&format!(
        "Repository: {} @ {head}\nSize: {} files ({} excluded as vendored or metadata), {lines} lines, {} symbols, {} modules\n",
        inventory.root.display(),
        inventory.snapshot.files.len(),
        inventory.snapshot.excluded,
        graph.entities.len(),
        graph.modules.len()
    ));

    out.push_str("\nLanguages:\n");
    for (name, totals) in inventory.languages() {
        let support = if totals.enforceable {
            "enforceable"
        } else if totals.parsed + totals.partial > 0 {
            "discovery only"
        } else {
            "counted"
        };
        let partial = if totals.partial > 0 {
            format!(", {} with syntax errors", totals.partial)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "  {name:<12} {:>6} files {:>9} lines {:>7} symbols  {support}{partial}\n",
            totals.files, totals.lines, totals.symbols
        ));
    }

    let mut services = graph
        .services
        .iter()
        .enumerate()
        .filter(|(_, service)| service.files > 0)
        .collect::<Vec<_>>();
    services.sort_by(|left, right| {
        right
            .1
            .files
            .cmp(&left.1.files)
            .then(left.1.path.cmp(&right.1.path))
    });
    out.push_str(&format!("\nServices ({}):\n", services.len()));
    for (_, service) in services.iter().take(LISTED) {
        let depends = service
            .depends_on
            .keys()
            .map(|target| graph.services[*target].id.as_str())
            .collect::<Vec<_>>();
        out.push_str(&format!(
            "  {} [{}] {} files, {} symbols{}\n",
            service.id,
            service.evidence.join(", "),
            service.files,
            service.symbols,
            if depends.is_empty() {
                String::new()
            } else {
                format!("; calls {}", depends.join(", "))
            }
        ));
    }
    let mut modules = graph.modules.iter().collect::<Vec<_>>();
    modules.sort_by(|left, right| {
        right
            .symbols
            .cmp(&left.symbols)
            .then(left.id.cmp(&right.id))
    });
    out.push_str(&format!("Modules ({}, largest first):\n", modules.len()));
    for module in modules.iter().take(LISTED) {
        out.push_str(&format!(
            "  {} {} files, {} symbols\n",
            module.id, module.files, module.symbols
        ));
    }

    let critical = inventory.critical();
    out.push_str(&format!("\nCritical candidates ({}):\n", critical.len()));
    if critical.is_empty() {
        out.push_str("  none found\n");
    }
    for index in critical.iter().take(LISTED) {
        let entity = &graph.entities[*index];
        let coverage = if entity.contracts.is_empty() {
            match inventory.suggestion(entity) {
                Ok(rule) => format!("uncovered; suggested rule: {rule}"),
                Err(note) => format!("uncovered; {note}"),
            }
        } else {
            format!("covered by {}", entity.contracts.join(", "))
        };
        out.push_str(&format!(
            "  [{}] {} ({})\n      {coverage}\n",
            entity.score,
            entity.id,
            entity.risk.join(", ")
        ));
    }

    out.push_str("\nContract coverage:\n");
    if !inventory.initialized {
        out.push_str("  Crane is not initialized here; run 'crane init' to write policies (discovery works without it)\n");
    } else {
        let coverage = inventory.coverage();
        let resolved = inventory
            .contracts
            .iter()
            .filter(|clause| clause.status == "resolved")
            .count();
        out.push_str(&format!(
            "  {} clauses ({resolved} resolved in the worktree), {} malformed policies\n",
            inventory.contracts.len(),
            inventory.malformed.len()
        ));
        for clause in inventory
            .contracts
            .iter()
            .filter(|clause| clause.status != "resolved")
        {
            out.push_str(&format!(
                "  {} {} {} {}: target is {}\n",
                clause.policy_id,
                clause.rule,
                clause.kind.flag(),
                clause.target,
                clause.status
            ));
        }
        out.push_str(&format!(
            "  functions covered: {} of {}; symbols covered: {} of {}; critical candidates covered: {} of {}\n",
            coverage.callables_covered,
            coverage.callables,
            coverage.entities_covered,
            coverage.entities,
            coverage.critical_covered,
            coverage.critical
        ));
        for checkpoint in &inventory.checkpoints {
            let since = checkpoint
                .commits_since
                .map_or("unknown".to_string(), |count| count.to_string());
            out.push_str(&format!(
                "  checkpoint {} {} ({}; used by {}): {since} commits behind HEAD, {} files changed, {} covered symbols in changed files\n",
                checkpoint.name,
                checkpoint.commit.chars().take(12).collect::<String>(),
                checkpoint.status,
                if checkpoint.policies.is_empty() { "no policy".to_string() } else { checkpoint.policies.join(", ") },
                checkpoint.changed_files.len(),
                checkpoint.covered_changed
            ));
        }
    }

    out.push_str(&format!(
        "\nIndex: {} files parsed, {} reused from cache, {} symbols relinked{}\n",
        inventory.snapshot.parsed,
        inventory.snapshot.reused,
        graph.relinked,
        match &inventory.cache {
            Some(path) => format!(" (cache {})", path.display()),
            None => " (no cache without 'crane init')".into(),
        }
    ));
    for warning in &inventory.warnings {
        out.push_str(&format!("Warning: {warning}\n"));
    }
    out.push_str("Use 'crane discover --json' for the full inventory.\n");
    out
}
