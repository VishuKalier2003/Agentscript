use std::fs;

use serde_json::Value;

use crate::evidence::{attest, export, otlp, reconstruct, records, render, verify_chain};
use crate::repository::ensure_initialized;
use crate::session::ContractSession;
use crate::util::option;

/** Usage of crane session */
const USAGE: &str = "crane session inspect SESSION_ID [--json] | crane session inspect --export FILE [--json] | crane session export SESSION_ID --json [--otlp]";

/** Inspect or export a session's evidence: the journal, the evidence records projected from it,
 * and the attestation derived from them; read-only
 * Input
    - args: &[String] - inspect or export, a session id or --export FILE, and options
 * Output
    - Result<(), String>
    - Error for unknown sessions or options, an unreadable export, or evidence that does not verify
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    let json = args.iter().any(|argument| argument == "--json");
    let id = args.iter().skip(1).find(|argument| {
        !argument.starts_with("--")
            && Some(argument.as_str()) != option(args, "--export").as_deref()
    });
    let load = || -> Result<ContractSession, String> {
        let id = id.ok_or_else(|| format!("a session id is required; use '{USAGE}'"))?;
        ContractSession::load(id)?.ok_or_else(|| format!("no contract session '{id}'"))
    };
    match args.first().map(String::as_str) {
        Some("inspect") => {
            if let Some(file) = option(args, "--export") {
                let text = fs::read_to_string(&file).map_err(|error| format!("{file}: {error}"))?;
                let exported: Value =
                    serde_json::from_str(&text).map_err(|error| format!("{file}: {error}"))?;
                let rebuilt = reconstruct(&exported)?;
                let lifecycle = exported["lifecycle"].as_str().unwrap_or("unknown");
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&rebuilt).map_err(|error| error.to_string())?
                    );
                } else {
                    print!(
                        "{}",
                        render(
                            &exported["session"],
                            lifecycle,
                            rebuilt["evidence"]
                                .as_array()
                                .map_or(&[][..], Vec::as_slice),
                            &rebuilt["attestation"],
                            &rebuilt["chain"]
                        )
                    );
                    println!(
                        "\nReconstructed from {file}: evidence {}, attestation {}",
                        if rebuilt["evidence_consistent"] == true {
                            "consistent"
                        } else {
                            "INCONSISTENT"
                        },
                        if rebuilt["attestation_consistent"] == true {
                            "consistent"
                        } else {
                            "INCONSISTENT"
                        }
                    );
                }
                let verified = rebuilt["chain"]["status"] != "broken"
                    && rebuilt["evidence_consistent"] == true
                    && rebuilt["attestation_consistent"] == true;
                return if verified {
                    Ok(())
                } else {
                    Err("the export does not verify against its own journal".into())
                };
            }
            let session = load()?;
            let binding = session.document();
            let events = session.events();
            let records = records(&binding, &events)?;
            let attestation = attest(&binding, &events)?;
            let chain = verify_chain(&events);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"chain": chain, "evidence": records, "attestation": attestation}))
                        .map_err(|error| error.to_string())?
                );
            } else {
                print!(
                    "{}",
                    render(
                        &binding,
                        session.lifecycle().name(),
                        &records,
                        &attestation,
                        &chain
                    )
                );
            }
            if chain["status"] == "broken" {
                Err("the session journal does not verify".into())
            } else {
                Ok(())
            }
        }
        Some("export") => {
            if !json && !args.iter().any(|argument| argument == "--otlp") {
                return Err(format!("choose a format; use '{USAGE}'"));
            }
            let session = load()?;
            let binding = session.document();
            let events = session.events();
            let value = if args.iter().any(|argument| argument == "--otlp") {
                otlp(&binding, &records(&binding, &events)?)
            } else {
                export(&binding, session.lifecycle().name(), &events)?
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        Some(other) => Err(format!("unknown session command '{other}'; use '{USAGE}'")),
        None => Err(format!("use '{USAGE}'")),
    }
}
