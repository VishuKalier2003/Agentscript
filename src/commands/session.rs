use std::fs;

use serde_json::Value;

use crate::evidence::{attest, export, otlp, reconstruct, records, render, verify_chain};
use crate::repository::ensure_initialized;
use crate::session::ContractSession;
use crate::util::option;

/** Usage of crane session */
const USAGE: &str = "crane session run TASK_ID --agent claude|codex|generic [--actions FILE] [--approve] [--autonomy MODE] [--detach] [--by NAME] [--json] [-- AGENT_COMMAND...] | crane session finish SESSION_ID [--json] | crane session lifecycle SESSION_ID [--json] | crane session inspect SESSION_ID [--json] | crane session inspect --export FILE [--json] | crane session export SESSION_ID --json [--otlp]";

/** Run, finish, or show a governed session (the session orchestrator)
 * Input
    - operation: &str - run, finish, or lifecycle
    - args: &[String] - arguments after "session"
    - json: bool - print JSON
 * Output
    - Result<(), String>
    - Error when the session ends FAILED (after it is shown)
*/
fn governed(operation: &str, args: &[String], json: bool) -> Result<(), String> {
    use crate::session_orchestrator::{self, Driver};
    // Everything after "--" is the agent command
    let split = args.iter().position(|argument| argument == "--");
    let (own, command) = match split {
        Some(index) => (&args[..index], args[index + 1..].to_vec()),
        None => (args, Vec::new()),
    };
    let target = own
        .iter()
        .skip(1)
        .enumerate()
        .find(|(index, argument)| {
            !argument.starts_with("--")
                && !own.get(*index).is_some_and(|previous| {
                    ["--agent", "--actions", "--autonomy", "--by"].contains(&previous.as_str())
                })
        })
        .map(|(_, argument)| argument.clone())
        .ok_or_else(|| format!("a task or session id is required; use '{USAGE}'"))?;
    let by = option(own, "--by").unwrap_or_else(|| "a human".into());
    let value = match operation {
        "run" => {
            let driver = if let Some(file) = option(own, "--actions") {
                let text = fs::read_to_string(&file).map_err(|error| format!("{file}: {error}"))?;
                let actions = match serde_json::from_str::<Value>(&text) {
                    Ok(Value::Array(actions)) => actions,
                    _ => text
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(|line| {
                            serde_json::from_str(line).map_err(|error| format!("{file}: {error}"))
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                };
                Driver::Script(actions)
            } else if !command.is_empty() {
                Driver::Command(command)
            } else if own.iter().any(|argument| argument == "--detach") {
                Driver::Detach
            } else {
                return Err("crane session run needs --actions FILE, an agent command after --, or --detach".into());
            };
            session_orchestrator::run(
                &target,
                option(own, "--agent"),
                option(own, "--autonomy")
                    .map(|value| crate::zones::model::Autonomy::parse(&value))
                    .transpose()?,
                driver,
                own.iter().any(|argument| argument == "--approve"),
                &by,
            )?
        }
        "finish" => {
            serde_json::json!({"session_id": target, "record": session_orchestrator::terminate(&target, &by)?})
        }
        _ => {
            serde_json::json!({"session_id": target, "record": session_orchestrator::load(&target)?.ok_or_else(|| format!("session {target} is not governed by the orchestrator"))?})
        }
    };
    let record = &value["record"];
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "Session {}: {}",
            value["session_id"].as_str().unwrap_or_default(),
            record["phase"].as_str().unwrap_or_default()
        );
        println!(
            "  lifecycle: {}",
            std::iter::once(
                record["history"][0]["from"]
                    .as_str()
                    .unwrap_or("TASK_READY")
            )
            .chain(
                record["history"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| entry["to"].as_str())
            )
            .collect::<Vec<_>>()
            .join(" -> ")
        );
        for line in value["actions"].as_array().into_iter().flatten() {
            if line["claim"].is_string() {
                println!(
                    "  agent claim (not trusted): {}",
                    line["claim"].as_str().unwrap_or_default()
                );
            } else {
                println!(
                    "  {} {} -> {}{}",
                    line["tool"].as_str().unwrap_or_default(),
                    line["operation"].as_str().unwrap_or_default(),
                    line["outcome"].as_str().unwrap_or_default(),
                    if line["executed"] == true {
                        " (executed)"
                    } else {
                        ""
                    }
                );
            }
        }
        for check in record["termination"]["checks"]
            .as_array()
            .into_iter()
            .flatten()
        {
            println!(
                "  check {}: {}",
                check["check"].as_str().unwrap_or_default(),
                if check["passed"] == true {
                    "passed"
                } else {
                    "FAILED"
                }
            );
        }
        if value["detached"] == true {
            println!(
                "  start the agent with CRANE_SESSION={} and finish with 'crane session finish {}'",
                value["session_id"].as_str().unwrap_or_default(),
                value["session_id"].as_str().unwrap_or_default()
            );
        }
    }
    match record["phase"].as_str() {
        Some("FAILED") if operation != "lifecycle" => Err(format!(
            "session {} FAILED: {}",
            value["session_id"].as_str().unwrap_or_default(),
            record["termination"]["failures"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        )),
        _ => Ok(()),
    }
}

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
    if let Some(operation @ ("run" | "finish" | "lifecycle")) = args.first().map(String::as_str) {
        return governed(operation, args, json);
    }
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
