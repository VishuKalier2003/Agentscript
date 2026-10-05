// The visual policy editor's model: a draft is a policy as structured rows (rule, item kind,
// target, scope, change type) that the dashboard edits; AgentScript is generated from it and
// validated by the same parser that compiles active policies, so the preview is exactly what
// would be proposed. AgentScript stays available as the Advanced editor and converts back.

use serde_json::{json, Value};

use crate::model::{ItemKind, Rule, Scope};
use crate::policy::parse;
use crate::util::validate_identifier;

/** Turn a draft into AgentScript, one canonical line per rule (block scope is left implicit)
 * Input
    - draft: &Value - {"name", "checkpoint", "rules": [{"rule", "kind", "target", "scope",
      "change_type"}]}
 * Output
    - Result<String, Vec<String>> the policy text, or every problem found (row by row)
*/
pub(crate) fn agentscript(draft: &Value) -> Result<String, Vec<String>> {
    let mut problems = Vec::new();
    let name = draft["name"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if let Err(error) = validate_identifier(&name) {
        problems.push(format!("name: {error}"));
    }
    let checkpoint = draft["checkpoint"]
        .as_str()
        .unwrap_or("baseline")
        .trim()
        .to_string();
    if let Err(error) = validate_identifier(&checkpoint) {
        problems.push(format!("checkpoint: {error}"));
    }
    let rows = draft["rules"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        problems.push("a policy needs at least one rule".into());
    }
    let mut lines = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let at = format!("rule {}", index + 1);
        let rule = row["rule"].as_str().unwrap_or_default();
        if !matches!(rule, "preserve" | "target") {
            problems.push(format!("{at}: rule must be preserve or target"));
            continue;
        }
        let kind = row["kind"].as_str().unwrap_or("function");
        let Some(flag) = ItemKind::ALL
            .iter()
            .find(|item| item.noun() == kind)
            .map(|item| item.flag())
        else {
            problems.push(format!("{at}: unknown item kind '{kind}'"));
            continue;
        };
        let target = row["target"].as_str().unwrap_or_default().trim();
        if target.is_empty()
            || target
                .chars()
                .any(|character| character.is_whitespace() || ";{}".contains(character))
        {
            problems.push(format!(
                "{at}: target must be one name such as PaymentService.charge"
            ));
            continue;
        }
        let mut line = format!("    {rule} {flag} {target}");
        let scope = row["scope"].as_str().unwrap_or("block");
        if Scope::parse(scope).is_err() {
            problems.push(format!("{at}: unknown scope '{scope}'"));
            continue;
        }
        if scope != "block" {
            line.push_str(&format!(" scope {scope}"));
        }
        if let Some(change_type) = row["change_type"]
            .as_str()
            .filter(|value| !value.is_empty() && *value != "any")
        {
            if rule != "target" {
                problems.push(format!("{at}: only target rules take a change type"));
                continue;
            }
            line.push_str(&format!(" change_type {change_type}"));
        }
        line.push(';');
        lines.push(line);
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let text = format!(
        "policy {name} {{\n    checkpoint {checkpoint};\n{}\n}}\n",
        lines.join("\n")
    );
    // The compiler's own parser is the authority on what the text means
    parse(&text).map_err(|error| vec![error])?;
    Ok(text)
}

/** Turn AgentScript into a draft, for the visual editor to show an existing or hand-written policy
 * Input
    - text: &str - policy text
 * Output
    - Result<Value, String> the draft
*/
pub(crate) fn draft(text: &str) -> Result<Value, String> {
    let policy = parse(text)?;
    let rules = policy
        .rules
        .iter()
        .map(|rule| match rule {
            Rule::Preserve { kind, target, scope } => json!({"rule": "preserve", "kind": kind.noun(), "target": target, "scope": scope.name(), "change_type": null}),
            Rule::Target { kind, target, scope, change_type } => json!({"rule": "target", "kind": kind.noun(), "target": target, "scope": scope.name(), "change_type": change_type.as_ref().map(|value| value.name())}),
        })
        .collect::<Vec<_>>();
    Ok(json!({"name": policy.name, "checkpoint": policy.checkpoint, "rules": rules}))
}
