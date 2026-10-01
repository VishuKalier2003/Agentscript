use serde_json::{json, Value};

use crate::orchestration::server::serve;
use crate::orchestration::{advance, ingest, read_deliveries, status, sync, TaskState};
use crate::proposals::store::Proposal;
use crate::repository::ensure_initialized;
use crate::tasks::{load, plan, proposal_content, render};
use crate::util::option;

/** Handle the task commands: plan derives a task contract from a task file; ingest, sync,
 * advance, status, and serve run the orchestration lifecycle for tasks from Jira and Asana
 * Input
    - args: &[String] - arguments after "task"
 * Output
    - Result<(), String>
    - Error if not initialized, the arguments are invalid, or the operation fails
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    let json = args.iter().any(|argument| argument == "--json");
    match args.first().map(String::as_str) {
        Some("plan") => plan_task(args),
        Some("ingest") => {
            let source =
                option(args, "--source").ok_or("crane task ingest requires --source jira|asana")?;
            let delivery = option(args, "--delivery");
            let files = positional(args, &["--source", "--delivery"]);
            let mut results = Vec::new();
            for (label, body) in read_deliveries(&files)? {
                let outcomes = ingest(&source, &body, delivery.as_deref())
                    .map_err(|error| format!("{label}: {error}"))?;
                results.extend(outcomes);
            }
            print_records(&results, json, |result| {
                format!(
                    "{} {} ({}): {} -> {}",
                    text(&result["task_id"]),
                    text(&result["event"]),
                    text(&result["detail"]),
                    text(&result["result"]),
                    result["state"].as_str().unwrap_or("no task")
                )
            })
        }
        Some("sync") => {
            let id = positional(args, &[]).into_iter().next();
            let records = sync(id.as_deref())?;
            print_records(&records, json, summary)
        }
        Some("status") => {
            let id = positional(args, &[]).into_iter().next();
            let records = status(id.as_deref())?;
            if records.is_empty() && !json {
                println!("No orchestrated tasks; replay events with 'crane task ingest'.");
                return Ok(());
            }
            print_records(&records, json, summary)
        }
        Some("advance") => {
            let id = positional(args, &["--to", "--reason"])
                .into_iter()
                .next()
                .ok_or("crane task advance requires a task id")?;
            let to = TaskState::parse(
                &option(args, "--to").ok_or("crane task advance requires --to STATE")?,
            )?;
            let record = advance(&id, to, option(args, "--reason"))?;
            print_records(&[record], json, summary)
        }
        Some("serve") => serve(
            &option(args, "--addr").unwrap_or_else(|| "127.0.0.1:8787".into()),
            args.iter().any(|argument| argument == "--once"),
        ),
        _ => {
            Err("unknown task operation; use plan, ingest, sync, status, advance, or serve".into())
        }
    }
}

/** Return the arguments after the operation that are neither flags nor flag values
 * Input
    - args: &[String] - arguments after "task"
    - valued: &[&str] - flags that take a value
 * Output
    - Vec<String>
*/
fn positional(args: &[String], valued: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    let mut skip = false;
    for argument in args.iter().skip(1) {
        if skip {
            skip = false;
        } else if valued.contains(&argument.as_str()) {
            skip = true;
        } else if !argument.starts_with("--") {
            found.push(argument.clone());
        }
    }
    found
}

/** Return a JSON string value, or an empty string
 * Input
    - value: &Value - JSON value
 * Output
    - String
*/
fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/** Summarize a task record in one line
 * Input
    - record: &Value - task record
 * Output
    - String
*/
fn summary(record: &Value) -> String {
    format!(
        "{} [{}] {} contract v{} {}{}",
        text(&record["task_id"]),
        text(&record["source"]),
        text(&record["state"]),
        record["contract_version"],
        record["proposal"].as_str().unwrap_or("-"),
        record["reason"]
            .as_str()
            .map_or(String::new(), |reason| format!(": {reason}"))
    )
}

/** Print records as JSON or one line each
 * Input
    - records: &[Value] - records or outcomes
    - json: bool - print JSON
    - line: impl Fn(&Value) -> String - one-line form
 * Output
    - Result<(), String>
*/
fn print_records(
    records: &[Value],
    json: bool,
    line: impl Fn(&Value) -> String,
) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&records).map_err(|error| error.to_string())?
        );
    } else {
        for record in records {
            println!("{}", line(record));
        }
    }
    Ok(())
}

/** Plan a task file: derive the task contract and print it; --propose also stores it as a
 * pending policy proposal that only a human or trusted process can approve
 * Input
    - args: &[String] - plan TASK [--json] [--checkpoint NAME] [--propose]
 * Output
    - Result<(), String>
    - Error if the task cannot be planned (the plan is printed first)
*/
fn plan_task(args: &[String]) -> Result<(), String> {
    let spec = args
        .get(1)
        .filter(|value| !value.starts_with("--"))
        .ok_or("crane task plan requires a task id or task file")?;
    let json = args.iter().any(|argument| argument == "--json");
    let checkpoint = option(args, "--checkpoint").unwrap_or_else(|| "baseline".into());
    let (task, digest) = load(spec)?;
    let mut plan = plan(&task, &digest, &checkpoint)?;
    let status = plan["status"].as_str().unwrap_or_default().to_string();
    if args.iter().any(|argument| argument == "--propose") && status == "planned" {
        let name = plan["policy_name"].as_str().unwrap_or_default().to_string();
        Proposal::ensure_free(&name)?;
        let content = proposal_content(&task.task_id, &name, &checkpoint)?;
        let proposal = Proposal::create(
            &name,
            &checkpoint,
            json!({"kind": "task", "task_id": task.task_id}),
            content,
            "crane task plan --propose",
        )?;
        plan["proposal"] = json!({
            "proposal_id": proposal.name,
            "status": proposal.status(),
            "policy_digest": proposal.digest(),
        });
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&plan).map_err(|error| error.to_string())?
        );
    } else {
        print!("{}", render(&plan));
        if let Some(name) = plan["proposal"]["proposal_id"].as_str() {
            println!(
                "Stored as pending proposal {name}; review it with 'crane policy show {name}'."
            );
        }
    }
    if status == "planned" {
        Ok(())
    } else {
        Err(format!("task {} is {status}", task.task_id))
    }
}
