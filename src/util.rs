use std::fmt::Display;
use std::time::{SystemTime, UNIX_EPOCH};

/** Validate a policy or checkpoint name, by rejecting empty values and any character other than
 * ASCII letters, digits, underscore, or hyphen
 * Input
    - value: &str - candidate identifier
 * Output
    - Result<(), String>
    - Error if the identifier is empty or contains an unsafe character
*/
pub(crate) fn validate_identifier(value: &str) -> Result<(), String> {
    // Restrict policy and checkpoint names to portable repository-safe identifiers
    if value.is_empty()
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
    {
        Err(format!("invalid identifier '{value}'"))
    } else {
        Ok(())
    }
}

/** Validate a qualified function target, by first normalizing Rust-style :: separators to dots
 * and then requiring every dot-separated part to be a non-empty run of ASCII letters, digits, or _
 * Input
    - value: &str - target such as Type.method or Type::method
 * Output
    - Result<(), String>
    - Error if any segment is empty or contains an unsupported character
*/
pub(crate) fn validate_function_target(value: &str) -> Result<(), String> {
    // Accept dotted and Rust-style qualified names while rejecting ambiguous syntax
    let normalized = value.replace("::", ".");
    let parts: Vec<_> = normalized.split('.').collect();
    if parts.is_empty()
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
    {
        Err(format!(
            "invalid function target '{value}'; use a qualified name such as Type.method"
        ))
    } else {
        Ok(())
    }
}

/** Read a key-value CLI option such as --function TARGET, by sliding a two-item window across the
 * arguments and returning the value that follows the first matching key
 * Input
    - args: &[String] - command arguments
    - key: &str - option flag to look for
 * Output
    - Option<String>
    - None if the key is absent or has no following value
*/
pub(crate) fn option(args: &[String], key: &str) -> Option<String> {
    // Read simple key-value CLI options without introducing a second parser layer
    args.windows(2)
        .find(|window| window[0] == key)
        .map(|window| window[1].clone())
}

/** Turn a target into a filesystem-safe name, by mapping each character and replacing anything
 * that is not an ASCII letter or digit with an underscore
 * Input
    - value: &str - text to sanitize
 * Output
    - String with only letters, digits, and underscores
*/
pub(crate) fn sanitize(value: &str) -> String {
    // Derive a filesystem-safe default policy name from a target
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/** Escape a string for Crane's hand-built JSON output, by replacing backslashes first, then double
 * quotes, then newlines with their escaped forms
 * Input
    - value: &str - raw text
 * Output
    - String safe to place inside a JSON string literal
*/
pub(crate) fn escape_json(value: &str) -> String {
    // Escape the characters emitted by Crane's hand-built stable JSON contract
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/** Extract one top-level field from the small checkpoint JSON format, by first finding the quoted
 * key, skipping to the following colon, and then reading either a quoted string (unescaping quotes
 * and backslashes) or a bare value up to the next comma or closing brace
 * Input
    - value: &str - JSON text
    - key: &str - field name to read
 * Output
    - Option<String>
    - None if the key or its value cannot be found
*/
pub(crate) fn json_field(value: &str, key: &str) -> Option<String> {
    // Read the small fixed checkpoint format without accepting arbitrary config syntax
    let needle = format!("\"{key}\"");
    let start = value.find(&needle)?;
    let rest = &value[start + needle.len()..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim_start();
    if let Some(quoted) = value.strip_prefix('"') {
        let end = quoted.find('"')?;
        Some(quoted[..end].replace("\\\"", "\"").replace("\\\\", "\\"))
    } else {
        value
            .split([',', '}'])
            .next()
            .map(|number| number.trim().to_string())
    }
}

/** Get the current Unix timestamp in seconds, by measuring the system clock against UNIX_EPOCH and
 * falling back to 0 if the clock is before the epoch
 * Input
    - None
 * Output
    - u64 seconds since the Unix epoch
*/
pub(crate) fn now_unix() -> u64 {
    // Stamp checkpoint metadata while keeping clock failure non-fatal for this field
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/** Convert any displayable error into Crane's String error type, by calling to_string on it so it
 * can be used with map_err
 * Input
    - error: E - any error implementing Display
 * Output
    - String containing the error message
*/
pub(crate) fn io_error<E: Display>(error: E) -> String {
    // Normalize filesystem and process errors for the CLI error boundary
    error.to_string()
}
