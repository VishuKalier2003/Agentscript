use std::fmt::Display;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn validate_identifier(value: &str) -> Result<(), String> {
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

pub(crate) fn validate_function_target(value: &str) -> Result<(), String> {
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

pub(crate) fn option(args: &[String], key: &str) -> Option<String> {
    args.windows(2)
        .find(|window| window[0] == key)
        .map(|window| window[1].clone())
}

pub(crate) fn sanitize(value: &str) -> String {
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

pub(crate) fn escape_json(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

pub(crate) fn json_field(value: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = value.find(&needle)?;
    let rest = &value[start + needle.len()..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim_start();
    if value.starts_with('"') {
        let quoted = &value[1..];
        let end = quoted.find('"')?;
        Some(quoted[..end].replace("\\\"", "\"").replace("\\\\", "\\"))
    } else {
        value
            .split(|character| character == ',' || character == '}')
            .next()
            .map(|number| number.trim().to_string())
    }
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) fn io_error<E: Display>(error: E) -> String {
    error.to_string()
}
