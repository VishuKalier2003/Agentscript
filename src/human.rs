// Plain-language wording for the records people review: what a criticality or an autonomy mode
// means, how a selector or a discovery signal reads, readable times, and the JSON writer that puts
// a record's human summary first. Summaries are derived from the record's own fields and never
// used for decisions, so digests and validation stay exactly as strict.

use serde_json::Value;

/** Format Unix seconds as a readable UTC time
 * Input
    - seconds: u64 - Unix seconds
 * Output
    - String such as "2026-10-07 14:27 UTC"
*/
pub(crate) fn when(seconds: u64) -> String {
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3_600,
        (rest % 3_600) / 60
    )
}

/** Format an optional JSON timestamp
 * Input
    - value: &Value - Unix seconds or null
 * Output
    - Option<String>
*/
pub(crate) fn when_value(value: &Value) -> Option<String> {
    value.as_u64().map(when)
}

/** Say what a criticality means
 * Input
    - criticality: &str - routine, sensitive, critical, or restricted
 * Output
    - &'static str
*/
pub(crate) fn criticality(criticality: &str) -> &'static str {
    match criticality {
        "routine" => "Routine: ordinary code",
        "sensitive" => "Sensitive: mistakes here are costly",
        "critical" => "Critical: mistakes here cost money or trust",
        "restricted" => "Restricted: closed to AI agents unless the organization grants access",
        _ => "unknown criticality",
    }
}

/** Say what an autonomy mode lets AI agents do
 * Input
    - autonomy: &str - observe, assisted, delegated, or autonomous
 * Output
    - &'static str
*/
pub(crate) fn autonomy(autonomy: &str) -> &'static str {
    match autonomy {
        "observe" => "AI agents may only read this code",
        "assisted" => "AI agents can change this code only after a human approves each change",
        "delegated" => "AI agents may change this code within their task's scope",
        "autonomous" => "AI agents may change this code on their own",
        _ => "unknown autonomy",
    }
}

/** Describe a zone selector in plain words
 * Input
    - selector: &str - such as "module python:payments.service" or "folder migrations"
 * Output
    - String
*/
pub(crate) fn selector(selector: &str) -> String {
    let (kind, target) = selector.split_once(' ').unwrap_or((selector, ""));
    let target = target.trim();
    match kind {
        "module" => match target.split_once(':') {
            Some((language, name)) => format!("the {name} module ({})", language_name(language)),
            None => format!("the {target} module"),
        },
        "folder" => format!("everything in the {target} folder"),
        "path" => format!("files matching {target}"),
        "tests" => "the test code".to_string(),
        "service" => format!("the {target} service"),
        "subsystem" => format!("the {target} subsystem"),
        "symbol" => format!("the code item {target}"),
        "policy" => format!("the code protected by policy {target}"),
        _ => selector.to_string(),
    }
}

/** Name a language for people
 * Input
    - language: &str - inventory language id
 * Output
    - String
*/
fn language_name(language: &str) -> String {
    match language {
        "python" => "Python".into(),
        "java" => "Java".into(),
        "javascript" => "JavaScript".into(),
        "typescript" => "TypeScript".into(),
        "rust" => "Rust".into(),
        "go" => "Go".into(),
        "cpp" => "C++".into(),
        "kotlin" => "Kotlin".into(),
        other => other.into(),
    }
}

/** Describe a discovery signal in plain words
 * Input
    - signal: &str - such as "pack:payment_processing" or "persistence"
 * Output
    - String
*/
pub(crate) fn signal(signal: &str) -> String {
    let words = |text: &str| text.replace('_', " ");
    if let Some(rest) = signal.strip_prefix("pack:") {
        return format!("matches the Payments pack: {}", words(rest));
    }
    if let Some((domain, word)) = signal.split_once("_name:") {
        return format!("names mention \"{word}\" ({domain} vocabulary)");
    }
    match signal {
        "persistence" => "reads or writes stored data".into(),
        "entrypoint" => "is an entry point (requests or commands come in here)".into(),
        "network" => "calls other services over the network".into(),
        "high_fan_in" => "is used by many other parts of the code".into(),
        "cross_module_callers" => "is called from several other modules".into(),
        "task_history" => "earlier agent sessions changed it".into(),
        "test_files" => "test files".into(),
        other if other.starts_with("region:") => {
            format!("is a protected region: {}", words(&other[7..]))
        }
        other => words(other),
    }
}

/** Name where evidence came from
 * Input
    - source: &str - such as "pack:payments@1" or "inventory:risk"
 * Output
    - String
*/
pub(crate) fn source(source: &str) -> String {
    match source {
        "pack:payments@1" => "the Payments pack".into(),
        "pack:testing@1" => "the Testing pack".into(),
        "proposals:engine" => "the discovery engine".into(),
        "inventory:risk" => "code risk signals".into(),
        "task_history" => "past agent sessions".into(),
        other => other.into(),
    }
}

/** Return the strings of a JSON array
 * Input
    - value: &Value - array
 * Output
    - Vec<String>
*/
pub(crate) fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(String::from))
        .collect()
}

/** Name a code item for people: "Qualified.name (path)" from an inventory id and its file, or the
 * id's last part when nothing better is known
 * Input
    - id: &str - such as "symbol:python:payments.service.PaymentService.charge"
 * Output
    - String
*/
pub(crate) fn symbol(id: &str) -> String {
    id.strip_prefix("symbol:")
        .and_then(|rest| rest.split_once(':').map(|(_, name)| name.to_string()))
        .unwrap_or_else(|| id.to_string())
}

/** Join list items for a sentence: "a, b and c"
 * Input
    - items: &[String] - items
 * Output
    - String
*/
pub(crate) fn list(items: &[String]) -> String {
    match items {
        [] => "nothing".into(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/** Return the first 12 characters of a digest, the approval code people quote
 * Input
    - digest: &str - "sha256:..."
 * Output
    - String
*/
pub(crate) fn code(digest: &str) -> String {
    digest
        .trim_start_matches("sha256:")
        .chars()
        .take(12)
        .collect()
}

/** Serialize a record for its file with its "summary" first and every other field after it in
 * the usual order (serde_json orders keys alphabetically, so the summary is placed explicitly)
 * Input
    - record: &Value - record, with or without a summary
 * Output
    - Result<String, String> pretty JSON ending in a newline
*/
pub(crate) fn pretty(record: &Value) -> Result<String, String> {
    let mut rest = record.clone();
    let summary = rest
        .as_object_mut()
        .and_then(|object| object.remove("summary"));
    let body = serde_json::to_string_pretty(&rest).map_err(|error| error.to_string())?;
    let Some(summary) = summary else {
        return Ok(body + "\n");
    };
    let summary = serde_json::to_string_pretty(&summary)
        .map_err(|error| error.to_string())?
        .replace('\n', "\n  ");
    let fields = body.trim_start_matches('{').trim_start();
    Ok(if fields.starts_with('}') {
        format!("{{\n  \"summary\": {summary}\n}}\n")
    } else {
        format!("{{\n  \"summary\": {summary},\n  {fields}\n")
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    /** Times read as UTC dates */
    #[test]
    fn times_are_readable() {
        assert_eq!(super::when(0), "1970-01-01 00:00 UTC");
        assert_eq!(super::when(1_791_373_657), "2026-10-07 11:47 UTC");
        assert_eq!(super::when(951_782_400), "2000-02-29 00:00 UTC");
    }

    /** The summary comes first and the record still parses to the same value */
    #[test]
    fn summary_is_written_first() {
        let record =
            json!({"active": false, "summary": {"decision_needed": "x"}, "zone_id": "payments"});
        let text = super::pretty(&record).unwrap();
        assert!(text.starts_with("{\n  \"summary\": {"), "{text}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            record
        );
        let empty = json!({"summary": {"a": 1}});
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&super::pretty(&empty).unwrap()).unwrap(),
            empty
        );
    }

    /** Selectors, signals, and code items read as words */
    #[test]
    fn wording() {
        assert_eq!(
            super::selector("module python:payments.service"),
            "the payments.service module (Python)"
        );
        assert_eq!(
            super::signal("pack:payment_processing"),
            "matches the Payments pack: payment processing"
        );
        assert_eq!(
            super::signal("payment_name:payment"),
            "names mention \"payment\" (payment vocabulary)"
        );
        assert_eq!(
            super::symbol("symbol:python:payments.service.PaymentService.charge"),
            "payments.service.PaymentService.charge"
        );
        assert_eq!(
            super::list(&["a".into(), "b".into(), "c".into()]),
            "a, b and c"
        );
        assert_eq!(super::code("sha256:6ad0bf876575d833"), "6ad0bf876575");
    }
}
