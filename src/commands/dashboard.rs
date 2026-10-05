use serde_json::{json, Value};

use crate::dashboard::{connect, route, server::serve};
use crate::repository::ensure_initialized;
use crate::util::option;

/** Usage of crane dashboard */
const USAGE: &str = "crane dashboard [serve] [--addr HOST:PORT] [--once] | crane dashboard api GET|POST PATH [--body JSON]";

/** Serve the dashboard, or answer one API request from the command line through the same route
 * the dashboard uses
 * Input
    - args: &[String] - serve options, or api METHOD PATH [--body JSON]
 * Output
    - Result<(), String>
    - Error for an API status other than 2xx
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    match args.first().map(String::as_str) {
        None | Some("serve") | Some("--addr") | Some("--once") => serve(
            &option(args, "--addr").unwrap_or_else(|| "127.0.0.1:8790".into()),
            args.iter().any(|argument| argument == "--once"),
        ),
        Some("api") => {
            let method = args
                .get(1)
                .map(|method| method.to_ascii_uppercase())
                .ok_or_else(|| format!("use '{USAGE}'"))?;
            let path = args.get(2).ok_or_else(|| format!("use '{USAGE}'"))?;
            let body: Value = match option(args, "--body") {
                Some(text) => serde_json::from_str(&text)
                    .map_err(|error| format!("--body is not JSON: {error}"))?,
                None => json!({}),
            };
            let (status, value) = route(&method, path, &body);
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
            );
            if (200..300).contains(&status) {
                Ok(())
            } else {
                Err(format!("{method} {path} answered {status}"))
            }
        }
        Some(other) => Err(format!(
            "unknown dashboard command '{other}'; use '{USAGE}'"
        )),
    }
}

/** Connect the repository once (crane connect [--refresh] [--json])
 * Input
    - args: &[String] - options
 * Output
    - Result<(), String>
*/
pub(crate) fn connect_command(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    let value = connect(args.iter().any(|argument| argument == "--refresh"))?;
    if args.iter().any(|argument| argument == "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
        );
    } else if value["already_connected"] == true {
        println!(
            "Already connected to {} (repository {}); use --refresh to update its metadata.",
            value["remote"]
                .as_str()
                .unwrap_or(value["root"].as_str().unwrap_or_default()),
            value["repository_id"].as_str().unwrap_or_default()
        );
    } else {
        println!(
            "Connected {} (repository {}, default branch {}, {} files, {} symbols). Run 'crane dashboard' to open the control plane.",
            value["remote"].as_str().unwrap_or(value["root"].as_str().unwrap_or_default()),
            value["repository_id"].as_str().unwrap_or_default(),
            value["default_branch"].as_str().unwrap_or("unknown"),
            value["files"],
            value["symbols"]
        );
    }
    Ok(())
}

/** Run a policy pack (crane packs [list] | show PACK | propose payments [--name N] [--checkpoint C])
 * Input
    - args: &[String] - subcommand and options
 * Output
    - Result<(), String>
*/
pub(crate) fn packs_command(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    let checkpoint = option(args, "--checkpoint").unwrap_or_else(|| "baseline".into());
    let json = args.iter().any(|argument| argument == "--json");
    let value = match args.first().map(String::as_str) {
        None | Some("list") | Some("--json") => crate::packs::list(),
        Some("show") => crate::packs::run(
            args.get(1)
                .ok_or("crane packs show needs payments or testing")?,
            &checkpoint,
        )?,
        Some("propose") => {
            if args.get(1).map(String::as_str) != Some("payments") {
                return Err("only the payments pack proposes a policy; the testing pack recommends configuration to apply by hand".into());
            }
            let proposal = crate::packs::propose(
                &option(args, "--name").unwrap_or_else(|| "payments_pack".into()),
                &checkpoint,
            )?;
            json!({"proposal": proposal.name, "status": proposal.status(), "digest": proposal.digest(), "activated": false, "next": format!("review with 'crane policy show {}', then approve or reject", proposal.name)})
        }
        Some(other) => {
            return Err(format!(
                "unknown packs command '{other}'; use list, show PACK, or propose payments"
            ))
        }
    };
    if json || args.first().map(String::as_str) != Some("show") {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "{} ({}): recommendations only, nothing is activated",
            value["pack"].as_str().unwrap_or_default(),
            value["version"].as_str().unwrap_or_default()
        );
        println!(
            "{}",
            serde_json::to_string_pretty(&value["summary"]).map_err(|error| error.to_string())?
        );
        for item in value["recommendations"].as_array().into_iter().flatten() {
            println!(
                "  [{}] {} {} — {}",
                item["status"].as_str().unwrap_or_default(),
                item["category"].as_str().unwrap_or_default(),
                item["entity"]
                    .as_str()
                    .or(item["directory"].as_str())
                    .or(item["framework"].as_str())
                    .unwrap_or_default(),
                item["suggestion"]
                    .as_object()
                    .and_then(|suggestion| suggestion.values().next())
                    .map(|value| value.as_str().map_or(value.to_string(), String::from))
                    .unwrap_or_default()
                    .trim()
            );
        }
        if let Some(policy) = value["proposed_policy"].as_str() {
            println!("\nProposed policy (run 'crane packs propose payments' to make it a pending proposal):\n{policy}");
        }
    }
    Ok(())
}
