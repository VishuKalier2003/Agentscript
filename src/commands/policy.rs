use serde_json::json;

use crate::proposals::engine::Confidence;
use crate::proposals::store::Proposal;
use crate::repository::ensure_initialized;
use crate::util::option;

/** Handle the policy proposal commands: propose generates candidate AgentScript without
 * activating it; proposals and show read; edit, approve, reject, and regenerate review (and refuse
 * to run on behalf of an agent)
 * Input
    - args: &[String] - arguments after "policy"
 * Output
    - Result<(), String>
    - Error if not initialized, the operation is unknown, or it fails
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    let json = args.iter().any(|argument| argument == "--json");
    let operation = args.first().map(String::as_str).unwrap_or("proposals");
    let name = || {
        args.get(1)
            .filter(|value| !value.starts_with("--"))
            .cloned()
            .ok_or_else(|| format!("crane policy {operation} requires a proposal name"))
    };
    match operation {
        "propose" => {
            let minimum = Confidence::parse(&option(args, "--min-confidence").unwrap_or_else(|| "medium".into()))?;
            let proposal = Proposal::propose(
                &option(args, "--name").unwrap_or_else(|| "proposed".into()),
                &option(args, "--checkpoint").unwrap_or_else(|| "baseline".into()),
                minimum,
            )?;
            if json {
                println!("{}", pretty(&proposal.document)?);
            } else {
                print!("{}", render(&proposal));
            }
            Ok(())
        }
        "proposals" | "list" => {
            let proposals = Proposal::all()?;
            if json {
                let summary = proposals
                    .iter()
                    .map(|proposal| {
                        json!({
                            "proposal_id": proposal.name,
                            "status": proposal.status(),
                            "revision": proposal.document["revision"],
                            "policy_digest": proposal.digest(),
                        })
                    })
                    .collect::<Vec<_>>();
                println!("{}", pretty(&json!(summary))?);
            } else if proposals.is_empty() {
                println!("No policy proposals; create one with 'crane policy propose'.");
            } else {
                for proposal in proposals {
                    println!(
                        "{} {} revision {} {}",
                        proposal.name,
                        proposal.status(),
                        proposal.document["revision"],
                        proposal.digest()
                    );
                }
            }
            Ok(())
        }
        "show" => {
            let proposal = Proposal::load(&name()?)?;
            if json {
                println!("{}", pretty(&proposal.document)?);
            } else {
                print!("{}", render(&proposal));
            }
            Ok(())
        }
        "edit" => {
            let mut proposal = Proposal::load(&name()?)?;
            proposal.edit(option(args, "--file"), option(args, "--by"))?;
            println!(
                "Recorded the edit of proposal {} as revision {} ({}); it is still pending review.",
                proposal.name,
                proposal.document["revision"],
                proposal.digest()
            );
            Ok(())
        }
        "regenerate" => {
            let mut proposal = Proposal::load(&name()?)?;
            proposal.regenerate(option(args, "--by"))?;
            println!(
                "Regenerated proposal {} as revision {} ({}); it is pending review.",
                proposal.name,
                proposal.document["revision"],
                proposal.digest()
            );
            Ok(())
        }
        "reject" => {
            let mut proposal = Proposal::load(&name()?)?;
            proposal.reject(option(args, "--approver"), option(args, "--reason"))?;
            println!("Rejected proposal {}; nothing was activated.", proposal.name);
            Ok(())
        }
        "approve" => {
            let mut proposal = Proposal::load(&name()?)?;
            let path = proposal.approve(option(args, "--approver"), option(args, "--confirm"))?;
            println!(
                "Approved proposal {} and activated {}; running agent sessions keep their bound contract and report the drift.",
                proposal.name,
                path.display()
            );
            Ok(())
        }
        other => Err(format!(
            "unknown policy operation '{other}'; use propose, proposals, show, edit, approve, reject, or regenerate"
        )),
    }
}

/** Format JSON for output
 * Input
    - value: &serde_json::Value - JSON
 * Output
    - Result<String, String>
*/
fn pretty(value: &serde_json::Value) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|error| error.to_string())
}

/** Render a proposal for review: status, digest, source, the candidate policy, the candidates
 * with their reasons, zone suggestions, history, and how to approve it
 * Input
    - proposal: &Proposal - proposal
 * Output
    - String ending in a newline
*/
fn render(proposal: &Proposal) -> String {
    let document = &proposal.document;
    let mut out = format!(
        "Proposal {} ({}, revision {}){}\nPolicy digest: {}\n",
        proposal.name,
        proposal.status(),
        document["revision"],
        if document["active"] == true {
            " - active"
        } else {
            " - not active"
        },
        proposal.digest()
    );
    if let Some(marker) = document["source"]["generated_in_agent_environment"].as_str() {
        out.push_str(&format!(
            "Generated in an agent environment ({marker}); review it with extra care.\n"
        ));
    }
    out.push_str(&format!(
        "Candidate policy ({}):\n",
        document["policy_file"].as_str().unwrap_or_default()
    ));
    for line in document["policy"].as_str().unwrap_or_default().lines() {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str("Candidates (heuristic):\n");
    for candidate in document["candidates"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  [{}] {} {}\n    reason: {}\n",
            candidate["confidence"].as_str().unwrap_or_default(),
            candidate["candidate"].as_str().unwrap_or_default(),
            if candidate["included"] == true {
                "(in policy)"
            } else {
                "(not in policy)"
            },
            candidate["reason"].as_str().unwrap_or_default()
        ));
    }
    for zone in document["zone_suggestions"]
        .as_array()
        .into_iter()
        .flatten()
    {
        out.push_str(&format!(
            "  zone suggestion for {}: {}\n",
            zone["candidate"].as_str().unwrap_or_default(),
            zone["zone"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    out.push_str("History:\n");
    for entry in document["history"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  revision {} {} by {}\n",
            entry["revision"],
            entry["action"].as_str().unwrap_or_default(),
            entry["actor"].as_str().unwrap_or_default()
        ));
    }
    if proposal.status() == "pending" {
        let digest = proposal.digest().trim_start_matches("sha256:");
        out.push_str(&format!(
            "To activate after review (a human or trusted process): crane policy approve {} --approver NAME --confirm {}\n",
            proposal.name,
            &digest[..digest.len().min(12)]
        ));
    }
    out
}
