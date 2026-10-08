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
        Some("contract") => contract_command(&args[1..]),
        Some("completions") => completions_command(&args[1..], json),
        Some(operation @ ("list" | "show" | "prepare" | "approve" | "launch")) => {
            intake_command(operation, args)
        }
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
        _ => Err(
            "unknown task operation; use list, show, prepare, approve, launch, plan, contract, ingest, sync, status, advance, or serve"
                .into(),
        ),
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

/** Handle the task intake commands, the path from a connected repository's tracker tasks to a
 * launched session: list, show, prepare (fetch, validate, and compile the contract), approve, and
 * launch (with the chosen agent)
 * Input
    - operation: &str - list, show, prepare, approve, or launch
    - args: &[String] - arguments after "task"
 * Output
    - Result<(), String>
*/
fn intake_command(operation: &str, args: &[String]) -> Result<(), String> {
    use crate::intake;
    let json = args.iter().any(|argument| argument == "--json");
    let task = || {
        positional(
            args,
            &[
                "--checkpoint",
                "--by",
                "--approver",
                "--confirm",
                "--agent",
                "--autonomy",
            ],
        )
        .into_iter()
        .next()
        .ok_or_else(|| format!("crane task {operation} requires a task id"))
    };
    let by = || option(args, "--by").unwrap_or_else(|| "a human".into());
    let value = match operation {
        "list" => intake::list()?,
        "show" => intake::show(&task()?)?,
        "prepare" => intake::prepare(&task()?, option(args, "--checkpoint"), &by())?,
        "approve" => {
            let value = intake::approve(
                &task()?,
                option(args, "--approver"),
                option(args, "--confirm"),
            )?;
            crate::zones::view::refresh();
            value
        }
        _ => intake::launch(
            &task()?,
            option(args, "--agent"),
            option(args, "--autonomy")
                .map(|value| crate::zones::model::Autonomy::parse(&value))
                .transpose()?,
            args.iter().any(|argument| argument == "--isolate"),
            &by(),
        )?,
    };
    // Planning a task that cannot be bounded is an error state, after the task is shown
    let unclear = (operation == "prepare" && value["state"] == "NEEDS_CLARIFICATION").then(|| {
        format!(
            "task {} needs clarification; no contract can be approved",
            text(&value["task_id"])
        )
    });
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
        );
        return unclear.map_or(Ok(()), Err);
    }
    let short = |value: &Value| {
        text(value)
            .trim_start_matches("sha256:")
            .chars()
            .take(12)
            .collect::<String>()
    };
    let line = |item: &Value| {
        format!(
            "{:<16} {:<26} {:<36} contract {:<14} agent {:<7} checkpoint {:<10} session {}",
            text(&item["task_id"]),
            if item["ready_to_run"] == true {
                "READY TO RUN".to_string()
            } else {
                text(&item["state"])
            },
            text(&item["title"]).chars().take(36).collect::<String>(),
            item["contract"]["contract_id"].as_str().unwrap_or("-"),
            item["agent"].as_str().unwrap_or("-"),
            item["checkpoint"]["name"].as_str().unwrap_or("-"),
            item["session"]["session_id"]
                .as_str()
                .map_or("-".to_string(), |id| format!(
                    "{id} ({})",
                    item["session"]["lifecycle"].as_str().unwrap_or("?")
                ))
        )
    };
    if operation == "list" {
        let tasks = value["tasks"].as_array().cloned().unwrap_or_default();
        if tasks.is_empty() {
            println!("No tasks for this repository; tracker snapshots go in .crane/sources/<source>/ and projects map to repositories in .crane/sources/config.json.");
        }
        for item in &tasks {
            println!("{}", line(item));
        }
        let unmapped = value["unmapped"].as_array().map_or(0, Vec::len);
        if unmapped > 0 {
            println!("({unmapped} tasks belong to projects not mapped to this repository)");
        }
        return Ok(());
    }
    println!("{}", line(&value));
    println!("  {}", text(&value["reason"]));
    if let Some(contract) = value["task_contract"].as_object() {
        let contract = Value::Object(contract.clone());
        print!(
            "{}",
            render_contract(&contract)
                .lines()
                .map(|line| format!("  {line}\n"))
                .collect::<String>()
        );
    }
    if let Some(launch) = value["launch"].as_object() {
        let binding = &launch["binding"];
        println!(
            "  launch binding {} ({}): task {}, contract {} {}, repository {}, checkpoint {} {}, agent {}, autonomy {}, budget {} actions / {} files, zones {}, policies {}",
            short(&launch["binding_digest"]),
            text(&value["launch_verified"]),
            text(&binding["task_id"]),
            text(&binding["contract"]["contract_id"]),
            short(&binding["contract"]["digest"]),
            binding["repository"]["name"].as_str().unwrap_or("unnamed"),
            text(&binding["checkpoint"]["name"]),
            short(&binding["checkpoint"]["sha"]),
            text(&binding["agent"]),
            text(&binding["autonomy"]["mode"]),
            binding["budget"]["mutating_actions"],
            binding["budget"]["files"],
            short(&binding["zones"]["zone_set_version"]),
            short(&binding["policy_version"])
        );
    }
    match value["state"].as_str().unwrap_or_default() {
        "AVAILABLE" => println!("  next: crane task prepare {}", text(&value["task_id"])),
        "CONTRACT_PENDING_APPROVAL" => println!(
            "  next: crane task approve {} --approver NAME --confirm {}",
            text(&value["task_id"]),
            short(&value["contract"]["digest"])
        ),
        "READY" if value["ready_to_run"] != true => println!(
            "  next: crane task launch {} --agent claude|codex",
            text(&value["task_id"])
        ),
        "READY" => println!(
            "  next: start {} in this repository with CRANE_SESSION={} (its hooks: 'crane agent install --profile {}', checked by 'crane agent hooks --profile {}')",
            text(&value["agent"]),
            text(&value["launch"]["session_id"]),
            text(&value["agent"]),
            text(&value["agent"])
        ),
        _ => {}
    }
    unclear.map_or(Ok(()), Err)
}

/** Handle the tracker completion commands: list, show an event, send the due events (--now
 * ignores the backoff), and reconcile after a restart (--send sends afterwards)
 * Input
    - args: &[String] - arguments after "task completions"
    - json: bool - print JSON
 * Output
    - Result<(), String>
*/
fn completions_command(args: &[String], json: bool) -> Result<(), String> {
    use crate::task_completion;
    let now = args.iter().any(|argument| argument == "--now");
    let value = match args.first().map(String::as_str).unwrap_or("list") {
        "list" => json!({"completions": task_completion::all()?.iter().map(task_completion::summary).collect::<Vec<_>>()}),
        "show" => {
            let id = args.get(1).filter(|value| !value.starts_with("--")).ok_or("crane task completions show requires an event id")?;
            task_completion::load(id)?.ok_or_else(|| format!("no completion event '{id}'"))?
        }
        "send" => json!({"sent": task_completion::dispatch(now, option(args, "--task").as_deref())?}),
        "reconcile" => task_completion::reconcile(args.iter().any(|argument| argument == "--send"))?,
        other => return Err(format!("unknown completions operation '{other}'; use list, show ID, send [--now] [--task ID], or reconcile [--send]")),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    let events = value["completions"]
        .as_array()
        .or(value["sent"].as_array())
        .cloned()
        .unwrap_or_default();
    if events.is_empty() && value["event_id"].is_null() && value["queued"].is_null() {
        println!(
            "{}",
            if value["sent"].is_array() {
                "No completion event is due."
            } else {
                "No completion events."
            }
        );
    }
    for event in &events {
        println!(
            "  {} {} {} {} merge {} attempts {}{}",
            text(&event["event_id"]),
            text(&event["task_id"]),
            text(&event["state"]),
            text(&event["status"]),
            text(&event["merge_sha"])
                .chars()
                .take(12)
                .collect::<String>(),
            event["attempts"],
            event["last_error"]
                .as_str()
                .map_or(String::new(), |error| format!(" ({error})"))
        );
        let summary = &event["summary"];
        println!("      {}", text(&summary["state"]));
        if let Some(attention) = summary["needs_attention"].as_str() {
            println!("      {attention}");
        }
        if let Some(send) = summary["send"].as_str() {
            println!("      next: {send}");
        }
    }
    if !value["event_id"].is_null() {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
        );
    }
    if !value["queued"].is_null() {
        println!(
            "Reconciled: {} queued again, {} recovered, {} confirmed, {} sent",
            value["queued"].as_array().map_or(0, Vec::len),
            value["recovered"].as_array().map_or(0, Vec::len),
            value["confirmed"].as_array().map_or(0, Vec::len),
            value["sent"].as_array().map_or(0, Vec::len)
        );
    }
    Ok(())
}

/** Usage of the task contract commands */
const CONTRACT_USAGE: &str = "crane task contract compile TASK [--checkpoint NAME] [--by NAME] [--json] | show TASK [--version N] [--json] | list [--json] | approve TASK --approver NAME --confirm DIGEST_PREFIX | reject TASK --approver NAME [--reason TEXT] | history TASK [--json]";

/** Handle the task contract commands: compile a task into a versioned, bound contract; show, list,
 * and trace contracts; approve or reject the current one (a human's decision)
 * Input
    - args: &[String] - arguments after "task contract"
 * Output
    - Result<(), String>
    - Error for invalid arguments, a failed operation, or a contract that grants no authority
      after compiling
*/
fn contract_command(args: &[String]) -> Result<(), String> {
    use crate::task_contracts;
    let json = args.iter().any(|argument| argument == "--json");
    let operation = args.first().map(String::as_str).unwrap_or("list");
    let task = || {
        positional(
            args,
            &[
                "--checkpoint",
                "--by",
                "--version",
                "--approver",
                "--confirm",
                "--reason",
            ],
        )
        .into_iter()
        .next()
        .ok_or_else(|| format!("a task id is required; use '{CONTRACT_USAGE}'"))
    };
    let value = match operation {
        "compile" => task_contracts::compile(
            &task()?,
            &option(args, "--checkpoint").unwrap_or_else(|| "baseline".into()),
            &option(args, "--by").map_or_else(
                || "crane task contract compile".to_string(),
                |by| format!("{by} (crane task contract compile)"),
            ),
        )?,
        "show" => {
            let id = task()?;
            let version = option(args, "--version")
                .map(|value| {
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("--version must be a number, not '{value}'"))
                })
                .transpose()?;
            task_contracts::load(&id, version)?.ok_or_else(|| {
                format!(
                    "task {id} has no contract; compile it with 'crane task contract compile {id}'"
                )
            })?
        }
        "list" => task_contracts::list()?,
        "approve" => {
            let value = task_contracts::approve(
                &task()?,
                option(args, "--approver"),
                option(args, "--confirm"),
            )?;
            crate::zones::view::refresh();
            value
        }
        "reject" => task_contracts::reject(
            &task()?,
            option(args, "--approver"),
            option(args, "--reason"),
        )?,
        "history" => task_contracts::history(&task()?)?,
        other => {
            return Err(format!(
                "unknown task contract operation '{other}'; use '{CONTRACT_USAGE}'"
            ))
        }
    };
    if json {
        let value = if value["task_contract_format"].is_u64() {
            task_contracts::with_summary(value.clone())
        } else {
            value.clone()
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
        );
    } else {
        match operation {
            "list" => {
                let contracts = value["contracts"].as_array().cloned().unwrap_or_default();
                if contracts.is_empty() {
                    println!(
                        "No task contracts; compile one with 'crane task contract compile TASK'."
                    );
                }
                for item in contracts {
                    if let Some(record) = item["task_id"]
                        .as_str()
                        .and_then(|task| task_contracts::load(task, None).ok().flatten())
                    {
                        let summary = task_contracts::summarize(&record);
                        println!("  {}", text(&summary["task"]));
                        println!("      {}", text(&summary["decision_needed"]));
                        if let Some(next) = ["approve", "start", "recompile"]
                            .iter()
                            .find_map(|key| summary[*key].as_str())
                        {
                            println!("      next: {next}");
                        }
                    }
                    println!(
                        "  {:<24} {:<24} {} {} must change, modules [{}]",
                        text(&item["contract_id"]),
                        text(&item["status"]),
                        text(&item["digest"])
                            .trim_start_matches("sha256:")
                            .chars()
                            .take(12)
                            .collect::<String>(),
                        item["must_change"],
                        item["modules"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(text)
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            "history" => {
                println!(
                    "Task {} contract history (audit chain {}):",
                    text(&value["task_id"]),
                    text(&value["chain"]["status"])
                );
                for version in value["versions"].as_array().into_iter().flatten() {
                    println!(
                        "  v{} {} {}",
                        version["version"],
                        text(&version["status"]),
                        text(&version["digest"])
                    );
                }
                for event in value["events"].as_array().into_iter().flatten() {
                    println!(
                        "  #{} {} v{} by {}",
                        event["seq"],
                        text(&event["event"]),
                        event["version"],
                        text(&event["by"])
                    );
                }
            }
            _ => print!("{}", render_contract(&value)),
        }
    }
    // Compiling a task that cannot be bounded is an error state, after the contract is shown
    if operation == "compile" {
        let status = value["status"].as_str().unwrap_or_default();
        if matches!(
            status,
            "clarification_required" | "conflicts_with_policy" | "unrelated"
        ) {
            return Err(format!(
                "task {} contract is {status}; it grants no authority",
                text(&value["task_id"])
            ));
        }
    }
    Ok(())
}

/** Render a task contract for review: status and authority, bindings, the six sections, what
 * needs clarification, and how to approve it
 * Input
    - record: &Value - contract
 * Output
    - String ending in a newline
*/
fn render_contract(record: &Value) -> String {
    let short = |value: &Value| {
        text(value)
            .trim_start_matches("sha256:")
            .chars()
            .take(12)
            .collect::<String>()
    };
    let join = |value: &Value| {
        value
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let summary = crate::task_contracts::summarize(record);
    let field = |key: &str| text(&summary[key]);
    let lines = |key: &str| {
        let list = crate::human::strings(&summary[key]);
        if list.is_empty() {
            "none".to_string()
        } else {
            list.join("; ")
        }
    };
    let mut out = format!(
        "Task {}\nStatus           {}\nDecision needed  {}\nMust change      {}\nNeeds a human    {}\nTests            {}\n\nDetails\n",
        field("task"),
        field("status"),
        field("decision_needed"),
        lines("must_change"),
        lines("needs_human_approval"),
        lines("tests")
    );
    out.push_str(&format!(
        "Task contract {} ({}): {}
Digest: {}
Authority: {}
",
        text(&record["contract_id"]),
        text(&record["status"]),
        text(&record["task"]["title"]),
        text(&record["digest"]),
        text(&record["authority"]["reason"])
    ));
    if record["unchanged"] == true {
        out.push_str(
            "Unchanged: the task, repository, and configuration compile to the same contract.\n",
        );
    }
    if record["already_approved"] == true {
        out.push_str("Already approved; nothing changed.\n");
    }
    if record["already_rejected"] == true {
        out.push_str("Already rejected; nothing changed.\n");
    }
    if let Some(message) = record["invalidation"]["message"].as_str() {
        out.push_str(&format!("Invalidated: {message}\n"));
    }
    let bindings = &record["bindings"];
    out.push_str(&format!(
        "Bound to: repository {} ({}), checkpoint {} {}, policies {}, zones {}, autonomy {} (max {}), budget {}, organization {}\n",
        bindings["repository"]["name"].as_str().unwrap_or("unnamed"),
        short(&bindings["repository"]["identity"]),
        text(&bindings["checkpoint"]["name"]),
        short(&bindings["checkpoint"]["sha"]),
        short(&bindings["policy_version"]),
        short(&bindings["zone_set_version"]),
        short(&bindings["autonomy"]["policy_version"]),
        text(&bindings["autonomy"]["max_autonomy"]),
        short(&bindings["budget"]["model_version"]),
        bindings["organization"]["organization"].as_str().unwrap_or("unspecified")
    ));
    for item in record["clarifications"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  {} [{}]: {}\n",
            if item["blocking"] == false {
                "advisory"
            } else {
                "CLARIFICATION REQUIRED"
            },
            text(&item["field"]),
            text(&item["message"])
        ));
    }
    for item in record["conflicts"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  CONFLICT [{}]: {}\n",
            text(&item["kind"]),
            text(&item["message"])
        ));
    }
    let contract = &record["contract"];
    for section in ["MUST_CHANGE", "MUST_NOT_CHANGE"] {
        let items = contract[section].as_array().cloned().unwrap_or_default();
        out.push_str(&format!("{section} ({}):\n", items.len()));
        for item in items {
            out.push_str(&format!(
                "  {} ({}) - {}\n",
                text(&item["qualified"]),
                text(&item["file"]),
                join(&item["because"])
            ));
        }
    }
    out.push_str(&format!(
        "MAY_CHANGE ({}): {}\n",
        contract["MAY_CHANGE"].as_array().map_or(0, Vec::len),
        join(&contract["MAY_CHANGE"])
    ));
    out.push_str(&format!(
        "REQUIRES_APPROVAL ({}):\n",
        contract["REQUIRES_APPROVAL"].as_array().map_or(0, Vec::len)
    ));
    for item in contract["REQUIRES_APPROVAL"]
        .as_array()
        .into_iter()
        .flatten()
    {
        out.push_str(&format!("  {}\n", text(&item["reason"])));
    }
    let scope = &contract["TASK_SCOPE"];
    out.push_str(&format!(
        "TASK_SCOPE: services [{}]; modules [{}]; files [{}]\n",
        join(&scope["services"]),
        join(&scope["modules"]),
        join(&scope["files"])
    ));
    out.push_str("EXPECTED_TESTS:\n");
    for item in contract["EXPECTED_TESTS"].as_array().into_iter().flatten() {
        if let Some(test) = item["test"].as_str() {
            out.push_str(&format!(
                "  run {test} (covers {})\n",
                text(&item["covers"])
            ));
        } else if let Some(file) = item["test_file"].as_str() {
            out.push_str(&format!("  run tests in {file}\n"));
        } else if item["add_regression_test"].is_array() {
            out.push_str(&format!(
                "  add a regression test for {}\n",
                join(&item["add_regression_test"])
            ));
        } else {
            out.push_str(&format!(
                "  add a test for {} ({})\n",
                text(&item["add_test_for"]),
                text(&item["reason"])
            ));
        }
    }
    if let Some(script) = record["agentscript"].as_str() {
        out.push_str("Executable contract (AgentScript, activated on approval):\n");
        for line in script.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    let next = [
        ("approve", "approve"),
        ("reject", "reject"),
        ("start", "start an agent"),
        ("recompile", "recompile"),
    ]
    .iter()
    .filter_map(|(key, label)| {
        summary[*key]
            .as_str()
            .map(|command| format!("  {:<15} {command}\n", format!("{label}:")))
    })
    .collect::<String>();
    if !next.is_empty() {
        out.push_str(&format!(
            "\nNext step, for a human (replace YOUR_NAME):\n{next}"
        ));
    }
    out
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
