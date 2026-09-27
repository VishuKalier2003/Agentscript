use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::model::{ChangeType, ItemKind, Policy, Rule, Scope};
use crate::util::{io_error, validate_function_target, validate_identifier};

/** Parse a single .crane policy file from disk, by first reading the whole file into a string and
 * then handing the content to parse, prefixing any parse error with the file path for diagnostics
 * Input
    - path: &Path - location of the .crane policy file
 * Output
    - Result<Policy, String>
    - Error if the file cannot be read or its content is not a valid policy
*/
pub(crate) fn parse_file(path: &Path) -> Result<Policy, String> {
    // Read one policy file and preserve its path in parse errors
    parse(&fs::read_to_string(path).map_err(io_error)?)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/** Parse policy source text into a Policy, by walking the lines in order while skipping blanks,
 * requiring the first line to be "policy NAME {", then accepting one "checkpoint NAME;" statement
 * and any number of "preserve --KIND TARGET [scope SCOPE];" and "target --KIND TARGET
 * [scope SCOPE] [change_type CHANGE_TYPE];" rules until the closing "}" (every
 * statement must end with ';', while the policy line and "}" must not), and finally rejecting
 * unclosed blocks, trailing content, unknown statements, empty rule lists, and a missing checkpoint
 * Input
    - content: &str - full text of a policy file
 * Output
    - Result<Policy, String>
    - Error with the offending line number if the policy is malformed
*/
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
            continue;
        }
        // Every statement inside the block must be terminated by exactly one ';'
        let statement = line
            .strip_suffix(';')
            .ok_or_else(|| format!("line {}: statement must end with ';'", number + 1))?
            .trim_end();
        if statement == "}" {
            return Err(format!(
                "line {}: ';' is not allowed after '}}'",
                number + 1
            ));
        }
        if let Some(value) = statement.strip_prefix("checkpoint ") {
            if checkpoint.is_some() {
                return Err(format!("line {}: duplicate checkpoint", number + 1));
            }
            validate_identifier(value.trim())?;
            checkpoint = Some(value.trim().into());
        } else if let Some(value) = statement.strip_prefix("preserve ") {
            // For preserve --function/--data/... added a rule, with an optional trailing scope
            rules.push(
                parse_preserve(value).map_err(|error| format!("line {}: {error}", number + 1))?,
            );
        } else if let Some(value) = statement.strip_prefix("target ") {
            // For target --function/--data/... added a rule, with optional scope and change_type
            rules.push(
                parse_target(value).map_err(|error| format!("line {}: {error}", number + 1))?,
            );
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

/** Print a parsed policy for the parse command, by first writing the policy name and checkpoint
 * and then looping through the rules to print each rule's keyword, item kind, target, scope, and
 * change type
 * Input
    - policy: &Policy - an already parsed policy
 * Output
    - None (writes to stdout)
*/
pub(crate) fn print(policy: &Policy) {
    // Display parsed policy fields for the parse command
    println!("Policy: {}\nCheckpoint: {}", policy.name, policy.checkpoint);
    for rule in &policy.rules {
        match rule {
            Rule::Preserve {
                kind,
                target,
                scope,
            } => {
                println!(
                    "Rule: preserve\nKind: {}\nTarget: {target}\nScope: {}",
                    kind.noun(),
                    scope.name()
                );
            }
            Rule::Target {
                kind,
                target,
                scope,
                change_type,
            } => {
                println!(
                    "Rule: target\nKind: {}\nTarget: {target}\nScope: {}\nChange type: {}",
                    kind.noun(),
                    scope.name(),
                    change_type.map_or("any", ChangeType::name)
                );
            }
        }
    }
}

/** Parse the arguments of a preserve statement, by reading the item flag and target and allowing
 * only the optional "scope VALUE" option (block when absent)
 * Input
    - value: &str - text after "preserve ", without the terminating ';'
 * Output
    - Result<Rule, String> a Preserve rule
    - Error if the item flag, target, or scope is invalid or other words are present
*/
pub(crate) fn parse_preserve(value: &str) -> Result<Rule, String> {
    let (kind, target, options) =
        parse_rule_options(value, &["scope"], "preserve --KIND TARGET [scope SCOPE];")?;
    let scope = options
        .get("scope")
        .map_or(Ok(Scope::Block), |value| Scope::parse(value))?;
    Ok(Rule::Preserve {
        kind,
        target,
        scope,
    })
}

/** Parse the arguments of a target statement, by reading the item flag and target and the
 * optional "scope VALUE" (block when absent) and "change_type VALUE" (any change when absent)
 * options, which may appear in either order
 * Input
    - value: &str - text after "target ", without the terminating ';'
 * Output
    - Result<Rule, String> a Target rule
    - Error if the item flag, target, scope, or change type is invalid, or other words are present
*/
pub(crate) fn parse_target(value: &str) -> Result<Rule, String> {
    let (kind, target, options) = parse_rule_options(
        value,
        &["scope", "change_type"],
        "target --KIND TARGET [scope SCOPE] [change_type CHANGE_TYPE];",
    )?;
    let scope = options
        .get("scope")
        .map_or(Ok(Scope::Block), |value| Scope::parse(value))?;
    let change_type = options
        .get("change_type")
        .map(|value| ChangeType::parse(value))
        .transpose()?;
    Ok(Rule::Target {
        kind,
        target,
        scope,
        change_type,
    })
}

/** Split rule arguments into the item kind, the target, and its "KEY VALUE" options, by reading
 * the item flag (--function, --data, --variable, --class, or --interface), validating the next
 * word as a qualified target, and then reading the remaining words in pairs, rejecting unknown
 * keys, repeated keys, and keys without a value
 * Input
    - value: &str - rule arguments after the keyword
    - allowed: &[&str] - option keys this rule accepts
    - usage: &str - syntax shown in error messages
 * Output
    - Result<(ItemKind, String, HashMap<String, String>), String> kind, target, and options
    - Error describing the first invalid word
*/
fn parse_rule_options(
    value: &str,
    allowed: &[&str],
    usage: &str,
) -> Result<(ItemKind, String, HashMap<String, String>), String> {
    let mut words = value.split_whitespace();
    let flag = words.next().unwrap_or_default();
    let kind = ItemKind::from_flag(flag).ok_or_else(|| {
        format!(
            "unsupported item '{flag}'; expected {} before the target",
            ItemKind::FLAGS
        )
    })?;
    let target = words
        .next()
        .ok_or_else(|| format!("missing {} target; expected '{usage}'", kind.noun()))?;
    validate_function_target(target)?;
    let mut options = HashMap::new();
    while let Some(key) = words.next() {
        if !allowed.contains(&key) {
            return Err(format!("unexpected '{key}'; expected '{usage}'"));
        }
        let Some(option) = words.next() else {
            return Err(match key {
                "scope" => "scope requires a value: block, file, flow, folder, or all".into(),
                _ => format!(
                    "{key} requires a value: logical_bn, logical_cn, logical_sn, or semantic"
                ),
            });
        };
        if options
            .insert(key.to_string(), option.to_string())
            .is_some()
        {
            return Err(format!("duplicate {key}"));
        }
    }
    Ok((kind, target.into(), options))
}
