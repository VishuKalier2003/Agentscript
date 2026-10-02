use serde_json::{json, Value};

use crate::autonomy::manage::{actor, apply, history, load_policy, status};
use crate::autonomy::{Actor, Condition, Trigger};
use crate::repository::{ensure_initialized, root};
use crate::session::{session_ids, ContractSession};
use crate::util::option;
use crate::zones::model::Autonomy;

/** Usage of crane autonomy */
const USAGE: &str = "crane autonomy status [SESSION_ID] [--json] | history SESSION_ID [--json] | budget SESSION_ID [--json] | promote|demote SESSION_ID --to MODE [--reason TEXT] | approve SESSION_ID [--reason TEXT] | refill SESSION_ID --amount N --reason TEXT --approver NAME --expires DURATION | credit SESSION_ID --event task_milestone|human_review|merge --reference REF --approver NAME";

/** Inspect and (as a human) change sessions' autonomy state: status and history only read;
 * promote, demote, and approve run the state machine with the caller as actor, so an agent's
 * attempt is rejected, journaled, and quarantines its session
 * Input
    - args: &[String] - subcommand, session id, and options
 * Output
    - Result<(), String>
    - Error for unknown subcommands or sessions, illegal transitions, or an invalid policy
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    let json = args.iter().any(|argument| argument == "--json");
    let positional = positional(args);
    let session = |index: usize| -> Result<ContractSession, String> {
        let id = positional
            .get(index)
            .ok_or_else(|| format!("a session id is required; use '{USAGE}'"))?;
        ContractSession::load(id)?.ok_or_else(|| format!("no contract session '{id}'"))
    };
    let reason = option(args, "--reason").unwrap_or_default();
    match positional.first().map(String::as_str) {
        Some("status") => {
            let value = match positional.get(1) {
                Some(_) => status(&session(1)?),
                None => overview()?,
            };
            print_value(&value, json, render_status)
        }
        Some("history") => {
            let session = session(1)?;
            let value = json!({"session_id": session.id(), "history": history(&session)});
            print_value(&value, json, render_history)
        }
        Some(change @ ("promote" | "demote")) => {
            let session = session(1)?;
            let to = Autonomy::parse(
                &option(args, "--to").ok_or_else(|| format!("{change} needs --to MODE"))?,
            )?;
            let trigger = if change == "promote" {
                Trigger::Promote(to)
            } else {
                Trigger::Demote(to)
            };
            change_state(&session, trigger, &reason)
        }
        Some("approve") => {
            let session = session(1)?;
            change_state(
                &session,
                Trigger::Evidence(Condition::HumanApproval),
                &reason,
            )
        }
        Some("budget") => {
            let session = session(1)?;
            print_value(
                &crate::budget::manage::status(&session),
                json,
                render_budget,
            )
        }
        Some("refill") => {
            let session = session(1)?;
            let required = |key: &str| {
                option(args, key).ok_or_else(|| format!("refill needs {key}; use '{USAGE}'"))
            };
            let amount = required("--amount")?
                .parse::<u64>()
                .map_err(|_| "--amount must be a whole number".to_string())?;
            let expires = crate::budget::manage::duration(&required("--expires")?)?;
            let state = crate::budget::manage::refill(
                &session,
                amount,
                &required("--reason")?,
                &required("--approver")?,
                expires,
            )?;
            println!(
                "Refilled contract session {}: budget {} of {} ({} available); the refill expires in {expires} s.",
                session.id(),
                state.current(),
                state.max,
                state.available()
            );
            Ok(())
        }
        Some("credit") => {
            let session = session(1)?;
            let required = |key: &str| {
                option(args, key).ok_or_else(|| format!("credit needs {key}; use '{USAGE}'"))
            };
            let credited = crate::budget::manage::credit(
                &session,
                &required("--event")?,
                &required("--reference")?,
                &required("--approver")?,
            )?;
            match credited {
                Some(points) => println!(
                    "Credited {points} points to contract session {} (never above its maximum).",
                    session.id()
                ),
                None => {
                    println!("Nothing credited: this event was already credited or earns nothing.")
                }
            }
            Ok(())
        }
        Some(other) => Err(format!("unknown autonomy command '{other}'; use '{USAGE}'")),
        None => Err(format!("use '{USAGE}'")),
    }
}

/** Return the positional arguments, skipping options and their values
 * Input
    - args: &[String] - arguments
 * Output
    - Vec<String>
*/
fn positional(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--json" => {}
            "--to" | "--reason" | "--amount" | "--approver" | "--expires" | "--event"
            | "--reference" => {
                arguments.next();
            }
            _ => out.push(argument.clone()),
        }
    }
    out
}

/** Apply a human's trigger to a session, refusing (after journaling the attempt and quarantining
 * the session) when it runs in an agent environment
 * Input
    - session: &ContractSession - session
    - trigger: Trigger - promote, demote, or human approval
    - reason: &str - why, for the history
 * Output
    - Result<(), String>
*/
fn change_state(session: &ContractSession, trigger: Trigger, reason: &str) -> Result<(), String> {
    if !session.resumable() {
        return Err(format!(
            "contract session {} is {}; its autonomy can no longer change",
            session.id(),
            session.lifecycle().name()
        ));
    }
    let actor = actor();
    let state = apply(session, trigger, actor, reason).map_err(|error| {
        if actor == Actor::Agent {
            format!("crane autonomy refuses to run in an agent environment ({error}); the session is quarantined")
        } else {
            error
        }
    })?;
    println!(
        "Contract session {}: autonomy {}, safety {}.",
        session.id(),
        state.autonomy.name(),
        state.safety.name()
    );
    if let Some(reason) = &state.reason {
        println!("Safety reason: {reason}");
    }
    Ok(())
}

/** Describe the autonomy policy on disk and every session's state
 * Input
    - None
 * Output
    - Result<Value, String>
    - Error if .crane/autonomy.json is invalid
*/
fn overview() -> Result<Value, String> {
    let policy = load_policy()?;
    let mut sessions = Vec::new();
    for id in session_ids()? {
        if let Ok(Some(session)) = ContractSession::load(&id) {
            let value = status(&session);
            sessions.push(json!({
                "session_id": id,
                "lifecycle": value["lifecycle"],
                "autonomy": value["autonomy"],
                "safety": value["safety"],
                "effective_autonomy": value["effective_autonomy"],
                "safety_reason": value["safety_reason"],
            }));
        }
    }
    Ok(json!({
        "policy": {
            "source": if root()?.join(crate::autonomy::POLICY_FILE).exists() { ".crane/autonomy.json" } else { "defaults" },
            "version": policy.version(),
            "settings": policy.to_json(),
            "transitions": policy.transitions(),
        },
        "sessions": sessions,
    }))
}

/** Print a value as JSON or rendered for a terminal
 * Input
    - value: &Value - value
    - json: bool - print JSON
    - render: fn(&Value) -> String - terminal renderer
 * Output
    - Result<(), String>
*/
fn print_value(value: &Value, json: bool, render: fn(&Value) -> String) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
        );
    } else {
        print!("{}", render(value));
    }
    Ok(())
}

/** Render a value's string field, or "-"
 * Input
    - value: &Value - field
 * Output
    - String
*/
fn text(value: &Value) -> String {
    value.as_str().unwrap_or("-").to_string()
}

/** Render autonomy status: the policy overview with every session, or one session in full
 * Input
    - value: &Value - from overview() or status()
 * Output
    - String
*/
fn render_status(value: &Value) -> String {
    let mut out = String::new();
    if let Some(policy) = value
        .get("policy")
        .filter(|_| value.get("sessions").is_some())
    {
        let settings = &policy["settings"];
        out.push_str(&format!(
            "AUTONOMY POLICY ({}, {})\n  max autonomy: {}; promotion: {}; violations to quarantine: {}\n  critical violations: {}\n  legal transitions:\n",
            text(&policy["source"]),
            text(&policy["version"]),
            text(&settings["max_autonomy"]),
            if settings["single_step_promotion"] == true { "one step at a time" } else { "any higher level" },
            settings["violations_to_quarantine"],
            settings["critical_violations"].as_array().into_iter().flatten().map(text).collect::<Vec<_>>().join(", "),
        ));
        for row in policy["transitions"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "    {:<8} {} -> {}  ({} by {}; requires {})\n",
                text(&row["dimension"]),
                text(&row["from"]),
                text(&row["to"]),
                text(&row["trigger"]),
                text(&row["by"]),
                text(&row["requires"])
            ));
        }
        for grant in settings["grants"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "  grant: restricted zone {} up to {} (approved by {}{})\n",
                text(&grant["zone"]),
                text(&grant["autonomy"]),
                text(&grant["approved_by"]),
                grant["task"]
                    .as_str()
                    .map_or(String::new(), |task| format!(", task {task}"))
            ));
        }
        out.push_str("\nSESSIONS\n");
        let sessions = value["sessions"].as_array().cloned().unwrap_or_default();
        if sessions.is_empty() {
            out.push_str("  none\n");
        }
        for session in sessions {
            out.push_str(&format!(
                "  {}  {}  autonomy {}  safety {}  effective {}\n",
                text(&session["session_id"]),
                text(&session["lifecycle"]),
                text(&session["autonomy"]),
                text(&session["safety"]),
                text(&session["effective_autonomy"])
            ));
        }
        return out;
    }
    out.push_str(&format!(
        "Contract session {} ({})\n  autonomy:  {} (started as {}; policy maximum {})\n  safety:    {}{}\n  effective: {}\n",
        text(&value["session_id"]),
        text(&value["lifecycle"]),
        text(&value["autonomy"]),
        text(&value["initial_autonomy"]),
        text(&value["policy"]["max_autonomy"]),
        text(&value["safety"]),
        value["safety_reason"].as_str().map_or(String::new(), |reason| format!(" ({reason})")),
        text(&value["effective_autonomy"]),
    ));
    if let Some(recovery) = value["recovery"].as_object() {
        out.push_str(&format!(
            "  incident:  {} violation(s); evidence so far: {}\n  recovery:  to {} with {}\n  missing:   {}\n",
            value["violations_in_incident"],
            value["evidence"].as_array().filter(|items| !items.is_empty()).map_or("none".into(), |items| items.iter().map(text).collect::<Vec<_>>().join(", ")),
            text(&recovery["to"]),
            text(&recovery["requires"]),
            recovery["missing"].as_array().into_iter().flatten().map(|set| set.as_array().into_iter().flatten().map(text).collect::<Vec<_>>().join(" + ")).collect::<Vec<_>>().join(" | "),
        ));
    }
    let promotion = &value["promotion"];
    out.push_str(&format!(
        "  promotion: {}\n",
        if let Some(to) = promotion["to"].as_str() {
            format!("a human may promote to {to}")
        } else if let Some(blocked) = promotion["blocked"].as_str() {
            format!("blocked ({blocked})")
        } else {
            "none (at the policy maximum)".into()
        }
    ));
    let budget = &value["budget"];
    out.push_str(&format!(
        "  budget:    {} of {} ({} reserved){}
",
        budget["budget_current"],
        budget["budget_max"],
        budget["budget_reserved"],
        if budget["exhausted"] == true {
            "; exhausted, refill requested"
        } else {
            ""
        }
    ));
    for grant in value["restricted_grants"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  grant:     restricted zone {} up to {} (approved by {})\n",
            text(&grant["zone"]),
            text(&grant["grant"]["autonomy"]),
            text(&grant["grant"]["approved_by"])
        ));
    }
    out.push_str("Authority precedence (an earlier check is never relaxed by a later one):\n");
    for (index, rule) in value["precedence"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        out.push_str(&format!("  {}. {}\n", index + 1, text(rule)));
    }
    out
}

/** Render a session's budget: the five budget quantities, refills, and every budget event
 * Input
    - value: &Value - from budget::manage::status
 * Output
    - String
*/
fn render_budget(value: &Value) -> String {
    let mut out = format!(
        "Autonomy budget of {}
  budget_current:  {} of budget_max {}
  budget_reserved: {} (available {})
  budget_consumed: {}
  regenerated {}, refilled {} ({} expired unused), lost to violations {}
",
        text(&value["session_id"]),
        value["budget_current"],
        value["budget_max"],
        value["budget_reserved"],
        value["budget_available"],
        value["budget_consumed"],
        value["regenerated"],
        value["refilled"],
        value["refill_expired"],
        value["penalized"],
    );
    if value["exhausted"] == true {
        out.push_str(
            "  EXHAUSTED: autonomous actions need human approval; a human refill was requested
",
        );
    }
    for refill in value["refills"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  refill: {} left, approved by {}, expires at {}
",
            refill["remaining"],
            text(&refill["approver"]),
            refill["expires_at"]
        ));
    }
    out.push_str(
        "budget_events:
",
    );
    for event in value["budget_events"].as_array().into_iter().flatten() {
        let detail = [
            "amount",
            "source",
            "reference",
            "violation",
            "approver",
            "reason",
        ]
        .iter()
        .filter(|key| !event[**key].is_null())
        .map(|key| {
            format!(
                "{key}={}",
                event[*key]
                    .as_str()
                    .map_or(event[*key].to_string(), String::from)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
        out.push_str(&format!(
            "  #{} {} {detail}
",
            event["seq"],
            text(&event["kind"])
        ));
    }
    out
}

/** Render a session's autonomy history, one line per entry
 * Input
    - value: &Value - {session_id, history}
 * Output
    - String
*/
fn render_history(value: &Value) -> String {
    let mut out = format!("Autonomy history of {}\n", text(&value["session_id"]));
    for entry in value["history"].as_array().into_iter().flatten() {
        let what = match entry["trigger"].as_str() {
            Some("promote" | "demote") => {
                format!("{} to {}", text(&entry["trigger"]), text(&entry["to"]))
            }
            Some("violation") => format!("violation {}", text(&entry["violation"])),
            Some("quarantine") => format!("quarantine ({})", text(&entry["reason"])),
            Some("evidence") => format!("evidence {}", text(&entry["condition"])),
            _ => String::new(),
        };
        let changes = entry["changes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|change| {
                format!(
                    "{} {} -> {}",
                    text(&change["dimension"]),
                    text(&change["from"]),
                    text(&change["to"])
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&match entry["kind"].as_str() {
            Some("initial") => format!(
                "  #0 initial: autonomy {}, safety {}\n",
                text(&entry["state"]["autonomy"]),
                text(&entry["state"]["safety"])
            ),
            Some("rejected") => format!(
                "  #{} rejected: {what} by {}: {}\n",
                entry["seq"],
                text(&entry["actor"]),
                text(&entry["error"])
            ),
            Some("legacy") => format!(
                "  #{} safety set to {}\n",
                entry["seq"],
                text(&entry["changes"][0]["to"])
            ),
            _ => format!(
                "  #{} {}: {what} by {}{}{}\n",
                entry["seq"],
                text(&entry["kind"]),
                text(&entry["actor"]),
                if changes.is_empty() {
                    String::new()
                } else {
                    format!(": {changes}")
                },
                entry["note"]
                    .as_str()
                    .filter(|note| !note.is_empty())
                    .map_or(String::new(), |note| format!(" ({note})"))
            ),
        });
    }
    out
}
