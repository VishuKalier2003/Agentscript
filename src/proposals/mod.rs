// Policy discovery and proposals: heuristic candidates for protection derived from the inventory
// and the zones, and reviewable proposal files holding candidate AgentScript that only a human or
// a trusted organization process can activate. Nothing here is enforced until approved.

pub(crate) mod engine;
pub(crate) mod store;

#[cfg(test)]
mod tests;

use serde_json::{json, Value};

use engine::{heuristics, Candidate, Confidence};

/** Version of the policy discovery JSON layout */
const DISCOVERY_FORMAT: u64 = 1;

/** Number of candidates listed per confidence level in the human summary */
const LISTED: usize = 15;

/** Serialize candidates as the output of crane discover --policies --json; it holds no times or
 * paths outside the repository, so equal repositories give equal output
 * Input
    - candidates: &[Candidate] - candidates in order
 * Output
    - Value JSON object
*/
pub(crate) fn discovery_json(candidates: &[Candidate]) -> Value {
    json!({
        "policy_discovery_format": DISCOVERY_FORMAT,
        "advisory": true,
        "heuristics": heuristics(),
        "candidates": candidates.iter().map(Candidate::to_json).collect::<Vec<_>>(),
    })
}

/** Render candidates for a terminal, grouped by confidence
 * Input
    - candidates: &[Candidate] - candidates in order
 * Output
    - String ending in a newline
*/
pub(crate) fn render(candidates: &[Candidate]) -> String {
    let mut out = String::from(
        "Crane policy discovery (advisory: heuristic signals from names, paths, zones, the call graph, tests, and owners; nothing is enforced)\n",
    );
    for level in [Confidence::High, Confidence::Medium, Confidence::Low] {
        let group = candidates
            .iter()
            .filter(|candidate| candidate.confidence == level)
            .collect::<Vec<_>>();
        out.push_str(&format!(
            "\n{} confidence ({}):\n",
            level.name(),
            group.len()
        ));
        for candidate in group.iter().take(LISTED) {
            out.push_str(&format!(
                "  {} [{}]\n    reason: {}\n",
                candidate.candidate,
                candidate.id,
                candidate.reason()
            ));
            if !candidate.covered_by.is_empty() {
                out.push_str(&format!(
                    "    already covered by {}\n",
                    candidate.covered_by.join(", ")
                ));
            }
            match (&candidate.suggested_rule, &candidate.suggested_zone) {
                (Some(rule), _) => out.push_str(&format!("    suggested rule: {rule}\n")),
                (None, Some(zone)) => out.push_str(&format!(
                    "    suggested zone: {}\n",
                    zone.lines().map(str::trim).collect::<Vec<_>>().join(" ")
                )),
                (None, None) => {}
            }
            if candidate.suggested_rule.is_none() {
                if let Some(note) = &candidate.note {
                    out.push_str(&format!("    note: {note}\n"));
                }
            }
            out.push_str(&format!(
                "    affects {} entities in {} files\n",
                candidate.entities.len(),
                candidate.files.len()
            ));
        }
        if group.len() > LISTED {
            out.push_str(&format!(
                "  ... {} more (use --json)\n",
                group.len() - LISTED
            ));
        }
    }
    out.push_str("\nRun 'crane policy propose' to write the rules as a reviewable proposal; nothing is activated without approval.\n");
    out
}
