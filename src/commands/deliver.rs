use std::fs;

use serde_json::Value;

use crate::budget::manage::duration;
use crate::delivery::{decide, except, merge, render, run as deliver, slack_action, status};
use crate::repository::ensure_initialized;
use crate::util::option;

/** Usage of crane deliver */
const USAGE: &str = "crane deliver run|status SESSION_ID [--json] | approve|reject|request-changes SESSION_ID --approver NAME [--reason TEXT] | exception SESSION_ID --check NAME --approver NAME --reason TEXT [--expires DURATION] | merge SESSION_ID [--by NAME] | slack-action --body FILE --timestamp T --signature S";

/** Deliver sessions: run the delivery pipeline, report it, record human decisions and
 * exceptions, merge, and accept signed Slack actions
 * Input
    - args: &[String] - subcommand, session id, and options
 * Output
    - Result<(), String>
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    let json = args.iter().any(|argument| argument == "--json");
    let valued = [
        "--approver",
        "--reason",
        "--check",
        "--expires",
        "--by",
        "--body",
        "--timestamp",
        "--signature",
    ];
    let mut positional = Vec::new();
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        if valued.contains(&argument.as_str()) {
            arguments.next();
        } else if argument != "--json" {
            positional.push(argument.clone());
        }
    }
    let id = || {
        positional
            .get(1)
            .cloned()
            .ok_or_else(|| format!("a session id is required; use '{USAGE}'"))
    };
    let required =
        |key: &str| option(args, key).ok_or_else(|| format!("{key} is required; use '{USAGE}'"));
    let reason = option(args, "--reason").unwrap_or_default();
    let value: Value = match positional.first().map(String::as_str) {
        Some("run") => deliver(&id()?)?,
        Some("status") => status(&id()?)?,
        Some("approve") => decide(
            &id()?,
            "approval",
            &required("--approver")?,
            None,
            &reason,
            "cli",
        )?,
        Some("reject") => decide(
            &id()?,
            "rejection",
            &required("--approver")?,
            None,
            &reason,
            "cli",
        )?,
        Some("request-changes") => decide(
            &id()?,
            "changes_requested",
            &required("--approver")?,
            None,
            &reason,
            "cli",
        )?,
        Some("exception") => {
            let expires = option(args, "--expires")
                .map(|text| duration(&text))
                .transpose()?;
            except(
                &id()?,
                &required("--check")?,
                &required("--approver")?,
                None,
                &required("--reason")?,
                expires,
                "cli",
            )?
        }
        Some("merge") => merge(
            &id()?,
            &option(args, "--by").unwrap_or_else(|| "human".into()),
        )?,
        Some("slack-action") => {
            let file = required("--body")?;
            let body = fs::read_to_string(&file).map_err(|error| format!("{file}: {error}"))?;
            slack_action(
                body.trim_end_matches(['\r', '\n']),
                &required("--timestamp")?,
                &required("--signature")?,
            )?
        }
        Some(other) => return Err(format!("unknown deliver command '{other}'; use '{USAGE}'")),
        None => return Err(format!("use '{USAGE}'")),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
        );
    } else {
        print!("{}", render(&value));
    }
    Ok(())
}
