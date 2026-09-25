use std::fs;
use std::path::Path;

use crate::model::{Policy, Rule};
use crate::util::{io_error, validate_function_target, validate_identifier};

pub(crate) fn parse_file(path: &Path) -> Result<Policy, String> {
    // Read one policy file and preserve its path in parse errors
    parse(&fs::read_to_string(path).map_err(io_error)?)
        .map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn parse(content: &str) -> Result<Policy, String> {
    // Parse the intentionally small v0.1.5 language and reject unknown statements
    let mut name = None;
    let mut checkpoint = None;
    let mut rules = Vec::new();
    let mut started = false;
    let mut closed = false;

    for (number, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if !started {
            let rest = line
                .strip_prefix("policy ")
                .ok_or_else(|| format!("line {}: expected policy declaration", number + 1))?;
            if !rest.ends_with('{') {
                return Err(format!("line {}: policy must end with '{{'", number + 1));
            }
            let identifier = rest.trim_end_matches('{').trim();
            validate_identifier(identifier)?;
            name = Some(identifier.into());
            started = true;
            continue;
        }
        if closed {
            return Err(format!(
                "line {}: content after '}}' is not allowed",
                number + 1
            ));
        }
        if line == "}" {
            closed = true;
        } else if let Some(value) = line.strip_prefix("checkpoint ") {
            if checkpoint.is_some() {
                return Err(format!("line {}: duplicate checkpoint", number + 1));
            }
            validate_identifier(value.trim())?;
            checkpoint = Some(value.trim().into());
        } else if let Some(value) = line.strip_prefix("preserve --function ") {
            validate_function_target(value.trim())?;
            rules.push(Rule::PreserveFunction {
                target: value.trim().into(),
            });
        } else {
            return Err(format!(
                "line {}: unsupported statement '{line}'",
                number + 1
            ));
        }
    }
    if !started || !closed {
        return Err("policy block must be closed with '}'".into());
    }
    if rules.is_empty() {
        return Err("policy must contain at least one rule".into());
    }
    Ok(Policy {
        name: name.unwrap(),
        checkpoint: checkpoint.ok_or("MVP requires an explicit checkpoint")?,
        rules,
    })
}

pub(crate) fn print(policy: &Policy) {
    // Display parsed policy fields for the parse command
    println!("Policy: {}\nCheckpoint: {}", policy.name, policy.checkpoint);
    for rule in &policy.rules {
        let Rule::PreserveFunction { target } = rule;
        println!("Rule: preserve\nTarget: {target}");
    }
}
