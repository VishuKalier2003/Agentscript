use crate::flow::{advance, audit, render, status};
use crate::repository::ensure_initialized;
use crate::util::option;

/** Usage of crane flow */
const USAGE: &str = "crane flow [status] [TASK_ID] [--json] | crane flow advance TASK_ID [--now] [--by NAME] [--json] | crane flow audit [TASK_ID|repository] [--json]";

/** Show and advance the golden path: status shows every layer from the repository to the task
 * completion with the top-level stage; advance runs the steps that need no human decision; audit
 * shows the recorded stage changes
 * Input
    - args: &[String] - arguments after "flow"
 * Output
    - Result<(), String>
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let json = args.iter().any(|argument| argument == "--json");
    let positional = args
        .iter()
        .enumerate()
        .filter(|(index, argument)| {
            !argument.starts_with("--") && (*index == 0 || args[index - 1] != "--by")
        })
        .map(|(_, argument)| argument.as_str())
        .collect::<Vec<_>>();
    let (operation, target) = match positional.as_slice() {
        [] => ("status", None),
        ["status" | "advance" | "audit", rest @ ..] => (positional[0], rest.first().copied()),
        [task, ..] => ("status", Some(*task)),
    };
    // Status works before connection (it reports CONNECT_REPOSITORY); the rest needs Crane
    if operation != "status" {
        ensure_initialized()?;
    }
    let value = match operation {
        "advance" => advance(
            target.ok_or_else(|| format!("crane flow advance needs a task id; use '{USAGE}'"))?,
            args.iter().any(|argument| argument == "--now"),
            &option(args, "--by").unwrap_or_else(|| "a human (crane flow advance)".into()),
        )?,
        "audit" => audit(target)?,
        _ => status(target)?,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    match operation {
        "advance" => {
            for step in value["steps"].as_array().into_iter().flatten() {
                println!(
                    "{} -> {}: {}",
                    step["from"].as_str().unwrap_or_default(),
                    step["to"].as_str().unwrap_or_default(),
                    step["action"].as_str().unwrap_or_default()
                );
            }
            if value["steps"].as_array().is_none_or(Vec::is_empty) {
                println!("Nothing to advance automatically.");
            }
            print!("{}", render(&value["status"]));
        }
        "audit" => {
            println!(
                "Flow audit (chain {}):",
                value["chain"]["status"].as_str().unwrap_or_default()
            );
            for event in value["events"].as_array().into_iter().flatten() {
                println!(
                    "  #{} {} {} -> {}: {}",
                    event["seq"],
                    event["scope"].as_str().unwrap_or_default(),
                    event["from"].as_str().unwrap_or("(start)"),
                    event["to"].as_str().unwrap_or_default(),
                    event["reason"].as_str().unwrap_or_default()
                );
            }
        }
        _ => print!("{}", render(&value)),
    }
    Ok(())
}
