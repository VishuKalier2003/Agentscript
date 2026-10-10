// Human-readable rendering: ISO 8601 timestamps without a date library, and a YAML-like layout of
// JSON values for the task and session views.

use serde_json::Value;

/** Format Unix milliseconds as an ISO 8601 UTC timestamp, using the civil-from-days algorithm
 * Input
    - millis: u64 - Unix milliseconds
 * Output
    - String such as "2026-10-10T12:30:05Z"
*/
pub(crate) fn iso_time(millis: u64) -> String {
    let seconds = millis / 1000;
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
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
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/** Render a scalar for YAML-like output
 * Input
    - value: &Value - scalar
 * Output
    - String
*/
fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::String(text)
            if text.is_empty() || text.contains(": ") || text.starts_with(['-', '[', '{', '#']) =>
        {
            format!("{value}")
        }
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/** Render a JSON value as indented YAML-like text
 * Input
    - value: &Value - value
    - indent: usize - indentation in spaces
 * Output
    - String
*/
pub(crate) fn yaml(value: &Value, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut out = String::new();
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                match item {
                    Value::Object(inner) if !inner.is_empty() => {
                        out.push_str(&format!("{pad}{key}:\n{}", yaml(item, indent + 2)));
                    }
                    Value::Array(items) if !items.is_empty() => {
                        out.push_str(&format!("{pad}{key}:\n{}", yaml(item, indent + 2)));
                    }
                    Value::Object(_) => out.push_str(&format!("{pad}{key}: {{}}\n")),
                    Value::Array(_) => out.push_str(&format!("{pad}{key}: []\n")),
                    _ => out.push_str(&format!("{pad}{key}: {}\n", scalar(item))),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::Object(_) | Value::Array(_) => {
                        let inner = yaml(item, indent + 2);
                        let trimmed = inner.trim_start();
                        out.push_str(&format!("{pad}- {trimmed}"));
                    }
                    _ => out.push_str(&format!("{pad}- {}\n", scalar(item))),
                }
            }
        }
        other => out.push_str(&format!("{pad}{}\n", scalar(other))),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /** Check timestamps and YAML layout
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn renders_time_and_yaml() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_time(1_791_635_405_000), "2026-10-10T12:30:05Z");
        assert_eq!(iso_time(951_782_400_000), "2000-02-29T00:00:00Z");
        let text = yaml(&json!({"a": 1, "b": {"c": "x"}, "d": [1, {"e": null}]}), 0);
        assert_eq!(text, "a: 1\nb:\n  c: x\nd:\n  - 1\n  - e: null\n");
    }
}
