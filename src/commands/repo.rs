use serde_json::Value;

use crate::repo::{connect, disconnect, inspect, status, ConnectOptions};
use crate::util::option;

/** Usage of crane repo */
const USAGE: &str = "crane repo connect [--provider github|git|local] [--checkpoint NAME] [--default-branch NAME] [--refresh] [--json] | status [--json] | inspect [--json] | disconnect [--reason TEXT] [--forget]";

/** Connect, report, inspect, or disconnect the repository (crane connect is crane repo connect)
 * Input
    - args: &[String] - subcommand and options
 * Output
    - Result<(), String>
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let json = args.iter().any(|argument| argument == "--json");
    let value = match args.first().map(String::as_str) {
        Some("connect") => connect_with(&args[1..])?,
        Some("status") | None => status()?,
        Some("inspect") => inspect()?,
        Some("disconnect") => disconnect(
            option(args, "--reason"),
            args.iter().any(|argument| argument == "--forget"),
        )?,
        Some(other) => return Err(format!("unknown repo command '{other}'; use '{USAGE}'")),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
        );
    } else if args.first().map(String::as_str) == Some("connect") {
        // The first line says what connecting did; the rest is the live status
        let summary = render("connect", &value);
        print!(
            "{}",
            summary.lines().next().unwrap_or_default().to_string() + "\n"
        );
        print!("{}", render("status", &status()?));
    } else {
        print!(
            "{}",
            render(args.first().map(String::as_str).unwrap_or("status"), &value)
        );
    }
    Ok(())
}

/** Connect with command-line options (shared by crane connect)
 * Input
    - args: &[String] - options
 * Output
    - Result<Value, String>
*/
pub(crate) fn connect_with(args: &[String]) -> Result<Value, String> {
    connect(&ConnectOptions {
        refresh: args.iter().any(|argument| argument == "--refresh"),
        provider: option(args, "--provider"),
        checkpoint: option(args, "--checkpoint"),
        default_branch: option(args, "--default-branch"),
    })
}

/** Render a connection for a terminal
 * Input
    - command: &str - the subcommand
    - value: &Value - record or status
 * Output
    - String
*/
pub(crate) fn render(command: &str, value: &Value) -> String {
    let text = |value: &Value| {
        value.as_str().map_or_else(
            || {
                if value.is_null() {
                    "-".to_string()
                } else {
                    value.to_string()
                }
            },
            String::from,
        )
    };
    if value.is_null() {
        return "Forgot the repository connection; Crane configuration in .crane is kept.\n".into();
    }
    if value["connection_status"] == "not_connected" {
        return "Not connected. Run 'crane repo connect' in the repository.\n".into();
    }
    let mut out = String::new();
    match command {
        "connect" if value["already_connected"] == true => out.push_str("Already connected (nothing changed; use --refresh to update the metadata).\n"),
        "connect" => out.push_str(&format!("Connected {} ({}).\n", text(&value["full_name"]), text(&value["provider"]))),
        "disconnect" => out.push_str("Disconnected; the record and all Crane configuration are kept. 'crane repo connect' reconnects.\n"),
        _ => {}
    }
    let checkpoint = if value["trusted_checkpoint"].is_object() {
        format!(
            "{} {} ({})",
            text(&value["trusted_checkpoint"]["name"]),
            text(&value["trusted_checkpoint"]["commit"])
                .chars()
                .take(12)
                .collect::<String>(),
            text(&value["trusted_checkpoint"]["status"])
        )
    } else {
        text(&value["trusted_checkpoint"])
    };
    out.push_str(&format!(
        "Repository {}  [{}]\n  provider: {}{}\n  remote: {}\n  default branch: {}  current branch: {}\n  trusted checkpoint: {checkpoint}\n  discovery: {}\n  repository id: {}\n",
        text(&value["full_name"]),
        text(&value["connection_status"]),
        text(&value["provider"]),
        value["web_url"].as_str().map_or(String::new(), |url| format!("  {url}")),
        text(&value["remote"]),
        text(&value["default_branch"]),
        text(&value["current_branch"]),
        if value["discovery"].is_object() { format!("{} ({} files, {} symbols)", text(&value["discovery"]["state"]), value["discovery"]["last"]["files"], value["discovery"]["last"]["symbols"]) } else { format!("{} files, {} symbols", value["files"], value["symbols"]) },
        text(&value["repository_id"]),
    ));
    if value["policy_version"]["changed"] == true || value["zone_version"]["changed"] == true {
        out.push_str("  policies or zones changed since connecting (crane repo connect --refresh records them)\n");
    }
    if let Some(capabilities) = value["capabilities"].as_object() {
        out.push_str(&format!(
            "  ready: {}{}\n  branches: {}\n  worktrees: {}\n  agents: claude {}, codex {}\n  configured: {}\n",
            if capabilities["ready"] == true { "yes" } else { "no" },
            capabilities["missing"].as_array().filter(|missing| !missing.is_empty()).map_or(String::new(), |missing| format!(" ({})", missing.iter().map(text).collect::<Vec<_>>().join("; "))),
            value["branches"].as_array().map_or(0, Vec::len),
            value["worktrees"].as_array().map_or(0, Vec::len),
            if capabilities["agents"]["claude"] == true { "hooks installed" } else { "not installed" },
            if capabilities["agents"]["codex"] == true { "hooks installed" } else { "not installed" },
            capabilities["configuration"].as_object().map_or(String::new(), |configuration| configuration.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join(" ")),
        ));
    }
    out
}
