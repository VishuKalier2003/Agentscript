// Flow discovery: a lexical, language-aware index of function definitions and the calls inside
// them, and the call graph reached from a flow's entry points. The analysis is deliberately
// honest about what it cannot know: a call whose name matches no definition is external or
// dynamic, a call matching several definitions is ambiguous and is not followed (unless exactly one
// candidate is in the caller's own file), and files in unsupported languages are counted as not
// analyzed. Every edge is INFERRED; nothing is fabricated to make a graph look complete.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;

/** Language families the analysis understands
 * Variants
    - Indented - Python: definitions by "def", bodies by indentation
    - Braced - C-family and similar languages: bodies delimited by braces
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Indented,
    Braced,
}

/** Words that are never function names in a call position */
const NOT_CALLS: &[&str] = &[
    "if",
    "for",
    "while",
    "switch",
    "catch",
    "return",
    "match",
    "elif",
    "and",
    "or",
    "not",
    "in",
    "sizeof",
    "typeof",
    "new",
    "await",
    "yield",
    "assert",
    "print",
    "lambda",
    "else",
    "with",
    "except",
    "fn",
    "def",
    "function",
    "func",
    "fun",
    "loop",
    "throw",
    "raise",
    "do",
    "case",
    "when",
    "foreach",
    "using",
    "lock",
    "synchronized",
    "super",
    "this",
    "self",
    "static",
    "async",
];

/** Select the language family of a file
 * Input
    - path: &str - repository-relative path
 * Output
    - Option<Family>, None for unsupported languages
*/
fn family(path: &str) -> Option<Family> {
    let extension = path.rsplit_once('.')?.1.to_ascii_lowercase();
    match extension.as_str() {
        "py" | "pyi" => Some(Family::Indented),
        "rs" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "java" | "kt" | "kts" | "go" | "c"
        | "h" | "cc" | "cpp" | "cxx" | "hpp" | "cs" | "swift" | "scala" | "dart" | "php" => {
            Some(Family::Braced)
        }
        _ => None,
    }
}

/** Report whether a file is in a language the analysis supports
 * Input
    - path: &str - repository-relative path
 * Output
    - bool
*/
pub(crate) fn supported(path: &str) -> bool {
    family(path).is_some()
}

/** A function definition found in a file
 * Fields
    - name: String - function name
    - qualified: String - Type.name inside a class, impl, or struct block, otherwise the name
    - path: String - file
    - start_line: usize - first line (1-based)
    - end_line: usize - last line
    - calls: Vec<String> - names called in the body, in order, deduplicated
*/
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Definition {
    pub(crate) name: String,
    pub(crate) qualified: String,
    pub(crate) path: String,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) calls: Vec<String>,
}

/** Check whether a character can be part of an identifier
 * Input
    - character: char - character
 * Output
    - bool
*/
fn ident_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

/** Blank out comments and string literals (keeping line structure), so braces and calls inside
 * them are ignored
 * Input
    - text: &str - source
    - family: Family - language family
 * Output
    - String of the same length in characters per line
*/
fn strip(text: &str, family: Family) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        let next = chars.get(index + 1).copied();
        let line_comment = match family {
            Family::Indented => character == '#',
            Family::Braced => character == '/' && next == Some('/'),
        };
        if line_comment {
            while index < chars.len() && chars[index] != '\n' {
                output.push(' ');
                index += 1;
            }
        } else if family == Family::Braced && character == '/' && next == Some('*') {
            while index < chars.len()
                && !(chars[index] == '*' && chars.get(index + 1) == Some(&'/'))
            {
                output.push(if chars[index] == '\n' { '\n' } else { ' ' });
                index += 1;
            }
            output.push_str("  ");
            index += 2;
        } else if matches!(character, '"' | '\'' | '`') {
            let quote = character;
            output.push(' ');
            index += 1;
            while index < chars.len() && chars[index] != quote {
                if chars[index] == '\\' {
                    output.push(' ');
                    index += 1;
                }
                if index < chars.len() {
                    output.push(if chars[index] == '\n' { '\n' } else { ' ' });
                    index += 1;
                }
            }
            output.push(' ');
            index += 1;
        } else {
            output.push(character);
            index += 1;
        }
    }
    output
}

/** Find the names called in some source text: identifiers followed by "(", excluding keywords
 * Input
    - text: &str - stripped source
 * Output
    - Vec<String> in order, deduplicated
*/
fn calls_in(text: &str) -> Vec<String> {
    let chars = text.chars().collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    let mut calls = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if ident_char(chars[index])
            && !chars[index].is_ascii_digit()
            && (index == 0 || !ident_char(chars[index - 1]))
        {
            let start = index;
            while index < chars.len() && ident_char(chars[index]) {
                index += 1;
            }
            let name = chars[start..index].iter().collect::<String>();
            let mut look = index;
            while look < chars.len() && chars[look] == ' ' {
                look += 1;
            }
            if chars.get(look) == Some(&'(')
                && !NOT_CALLS.contains(&name.as_str())
                && seen.insert(name.clone())
            {
                calls.push(name);
            }
        } else {
            index += 1;
        }
    }
    calls
}

/** Find the definition name on a line of a braced language, if the line declares a function
 * Input
    - line: &str - stripped line
 * Output
    - Option<String>
*/
fn braced_definition(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let words = trimmed
        .split(|character: char| !ident_char(character))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    for keyword in ["fn", "function", "func", "fun"] {
        if words.contains(&keyword) {
            let after = &trimmed[trimmed.find(keyword)? + keyword.len()..];
            let after = after.trim_start();
            // Go methods: func (receiver T) Name(
            let after = if keyword == "func" && after.starts_with('(') {
                after[after.find(')')? + 1..].trim_start()
            } else {
                after
            };
            let name = after
                .chars()
                .take_while(|character| ident_char(*character))
                .collect::<String>();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    // Methods and C-family functions: "... name(args) {" or "name(args)" before a "{" line
    let open = trimmed.find('(')?;
    let before = trimmed[..open].trim_end();
    let name = before
        .chars()
        .rev()
        .take_while(|character| ident_char(*character))
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    if name.is_empty()
        || NOT_CALLS.contains(&name.as_str())
        || name
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
    {
        return None;
    }
    let prefix = before[..before.len() - name.len()].trim_end();
    let statement = prefix.ends_with('=')
        || prefix.ends_with('.')
        || prefix.ends_with("return")
        || prefix.ends_with(',')
        || prefix.ends_with('(');
    let declared = trimmed.ends_with('{')
        || trimmed.ends_with(')')
        || trimmed.contains(") {")
        || trimmed.contains("):");
    (!statement && declared && !trimmed.ends_with(';')).then_some(name)
}

/** Find the type a block line opens (class, struct, impl, interface, trait, object)
 * Input
    - line: &str - stripped line
 * Output
    - Option<String>
*/
fn type_block(line: &str) -> Option<String> {
    let words = line
        .split(|character: char| !ident_char(character))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    let position = words.iter().position(|word| {
        matches!(
            *word,
            "class" | "struct" | "impl" | "interface" | "trait" | "object" | "enum"
        )
    })?;
    let mut name = words.get(position + 1)?.to_string();
    // impl Trait for Type
    if words[position] == "impl" {
        if let Some(index) = words.iter().position(|word| *word == "for") {
            name = words.get(index + 1)?.to_string();
        }
    }
    Some(name)
}

/** Index the function definitions of one file
 * Input
    - path: &str - repository-relative path
    - text: &str - content
 * Output
    - Option<Vec<Definition>>, None for unsupported languages
*/
pub(crate) fn definitions(path: &str, text: &str) -> Option<Vec<Definition>> {
    let family = family(path)?;
    let stripped = strip(text, family);
    let lines = stripped.lines().collect::<Vec<_>>();
    let mut found = Vec::new();
    match family {
        Family::Indented => {
            let indent = |line: &str| line.len() - line.trim_start().len();
            let mut classes: Vec<(usize, String)> = Vec::new();
            for (index, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.is_empty() {
                    continue;
                }
                let depth = indent(line);
                classes.retain(|(class_indent, _)| *class_indent < depth);
                if let Some(rest) = trimmed.strip_prefix("class ") {
                    let name = rest
                        .chars()
                        .take_while(|character| ident_char(*character))
                        .collect::<String>();
                    classes.push((depth, name));
                    continue;
                }
                let rest = trimmed.strip_prefix("async ").unwrap_or(trimmed);
                let Some(rest) = rest.strip_prefix("def ") else {
                    continue;
                };
                let name = rest
                    .chars()
                    .take_while(|character| ident_char(*character))
                    .collect::<String>();
                if name.is_empty() {
                    continue;
                }
                let mut end = index;
                for (offset, body) in lines.iter().enumerate().skip(index + 1) {
                    if body.trim().is_empty() {
                        continue;
                    }
                    if indent(body) <= depth {
                        break;
                    }
                    end = offset;
                }
                let body = lines[index..=end].join("\n");
                let body = body.split_once(':').map_or(body.as_str(), |(_, rest)| rest);
                let qualified = classes
                    .last()
                    .map_or(name.clone(), |(_, class)| format!("{class}.{name}"));
                found.push(Definition {
                    calls: calls_in(body)
                        .into_iter()
                        .filter(|call| *call != name)
                        .collect(),
                    name,
                    qualified,
                    path: path.into(),
                    start_line: index + 1,
                    end_line: end + 1,
                });
            }
        }
        Family::Braced => {
            let mut depth = 0usize;
            let mut types: Vec<(usize, String)> = Vec::new();
            let mut pending_type: Option<String> = None;
            let mut index = 0;
            while index < lines.len() {
                let line = lines[index];
                if let Some(name) = type_block(line) {
                    pending_type = Some(name);
                }
                if let Some(name) = braced_definition(line) {
                    // The body starts at the first "{" on this or a following line
                    let mut cursor = index;
                    let mut column = line.find('{');
                    while column.is_none() && cursor + 1 < lines.len() && cursor < index + 3 {
                        cursor += 1;
                        if lines[cursor].trim().is_empty() {
                            continue;
                        }
                        column = lines[cursor]
                            .trim_start()
                            .starts_with('{')
                            .then(|| lines[cursor].find('{').unwrap_or(0));
                        if column.is_none() {
                            break;
                        }
                    }
                    if let Some(column) = column {
                        let mut balance = 0i64;
                        let mut end = cursor;
                        let mut body = String::new();
                        'scan: for (offset, text) in lines.iter().enumerate().skip(cursor) {
                            let segment = if offset == cursor {
                                &text[column..]
                            } else {
                                text
                            };
                            for character in segment.chars() {
                                match character {
                                    '{' => balance += 1,
                                    '}' => {
                                        balance -= 1;
                                        if balance == 0 {
                                            end = offset;
                                            body.push_str(segment);
                                            break 'scan;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            body.push_str(segment);
                            body.push('\n');
                            end = offset;
                        }
                        let qualified = types
                            .iter()
                            .rev()
                            .find(|(type_depth, _)| *type_depth <= depth)
                            .map_or(name.clone(), |(_, owner)| format!("{owner}.{name}"));
                        found.push(Definition {
                            calls: calls_in(&body)
                                .into_iter()
                                .filter(|call| *call != name)
                                .collect(),
                            name,
                            qualified,
                            path: path.into(),
                            start_line: index + 1,
                            end_line: end + 1,
                        });
                        // Continue scanning inside the body for nested definitions
                    }
                }
                for character in line.chars() {
                    match character {
                        '{' => {
                            depth += 1;
                            if let Some(name) = pending_type.take() {
                                types.push((depth, name));
                            }
                        }
                        '}' => {
                            types.retain(|(type_depth, _)| *type_depth < depth);
                            depth = depth.saturating_sub(1);
                        }
                        _ => {}
                    }
                }
                index += 1;
            }
        }
    }
    Some(found)
}

/** A call-graph node
 * Fields
    - id: String - path:qualified
    - definition: Definition - the function
    - depth: usize - distance from the nearest entry point
*/
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Node {
    pub(crate) id: String,
    pub(crate) definition: Definition,
    pub(crate) depth: usize,
}

/** A discovered call graph
 * Fields
    - entry_points: Vec<String> - resolved entry node ids
    - nodes: Vec<Node> - reached functions
    - edges: Vec<(String, String, String)> - caller id, callee id, how it was resolved
    - unresolved: Vec<String> - entry points or calls that could not be resolved, with why
    - files: BTreeSet<String> - files holding reached functions
    - status: &'static str - RESOLVED, PARTIAL (some calls unresolved), or UNRESOLVED (an entry
      point could not be resolved)
*/
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Graph {
    pub(crate) entry_points: Vec<String>,
    pub(crate) nodes: Vec<Node>,
    pub(crate) edges: Vec<(String, String, String)>,
    pub(crate) unresolved: Vec<String>,
    pub(crate) files: BTreeSet<String>,
    pub(crate) status: &'static str,
}

/** The function definitions of a repository
 * Fields
    - definitions: Vec<Definition> - every definition found
    - analyzed: usize - files analyzed
    - unsupported: usize - source-like files in languages the analysis does not support
*/
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Index {
    pub(crate) definitions: Vec<Definition>,
    pub(crate) analyzed: usize,
    pub(crate) unsupported: usize,
}

impl Index {
    /** Build the index from file contents
     * Input
        - files: impl IntoIterator<Item = (String, String)> - path and content pairs
     * Output
        - Index
    */
    pub(crate) fn build(files: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut index = Self::default();
        for (path, text) in files {
            match definitions(&path, &text) {
                Some(found) => {
                    index.analyzed += 1;
                    index.definitions.extend(found);
                }
                None if crate::selection::anchors::language_for(&path).is_some() => {
                    index.unsupported += 1
                }
                None => {}
            }
        }
        index
    }

    /** Resolve an entry point written as SYMBOL, Type.SYMBOL, or path:SYMBOL
     * Input
        - entry: &str - entry point
     * Output
        - Result<&Definition, String> the single match, or why it is not unique
    */
    fn resolve_entry(&self, entry: &str) -> Result<&Definition, String> {
        let (path, symbol) = match entry.rsplit_once(':') {
            Some((path, symbol)) if !path.is_empty() => (Some(path), symbol),
            _ => (None, entry),
        };
        let matches = self
            .definitions
            .iter()
            .filter(|definition| {
                path.is_none_or(|path| {
                    definition.path == path || definition.path.ends_with(&format!("/{path}"))
                })
            })
            .filter(|definition| {
                definition.qualified == symbol
                    || definition.name == symbol
                    || definition.qualified.ends_with(&format!(".{symbol}"))
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [single] => Ok(single),
            [] => Err(format!(
                "entry point {entry} matches no function definition"
            )),
            many => Err(format!(
                "entry point {entry} is ambiguous ({} definitions: {}); qualify it as path:Symbol",
                many.len(),
                many.iter()
                    .map(|definition| format!("{}:{}", definition.path, definition.qualified))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /** Discover the call graph reached from entry points, following calls to a unique definition
     * (or the unique definition in the caller's own file) up to a depth
     * Input
        - entries: &[String] - entry points
        - max_depth: usize - deepest call chain followed
     * Output
        - Graph
    */
    pub(crate) fn discover(&self, entries: &[String], max_depth: usize) -> Graph {
        let id = |definition: &Definition| format!("{}:{}", definition.path, definition.qualified);
        let mut by_name: BTreeMap<&str, Vec<&Definition>> = BTreeMap::new();
        for definition in &self.definitions {
            by_name
                .entry(definition.name.as_str())
                .or_default()
                .push(definition);
        }
        let mut graph = Graph {
            entry_points: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            unresolved: Vec::new(),
            files: BTreeSet::new(),
            status: "RESOLVED",
        };
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::new();
        for entry in entries {
            match self.resolve_entry(entry) {
                Ok(definition) => {
                    graph.entry_points.push(id(definition));
                    if seen.insert(id(definition)) {
                        queue.push_back((definition, 0));
                    }
                }
                Err(reason) => {
                    graph.unresolved.push(reason);
                    graph.status = "UNRESOLVED";
                }
            }
        }
        let mut unresolved_calls = BTreeSet::new();
        while let Some((definition, depth)) = queue.pop_front() {
            graph.files.insert(definition.path.clone());
            graph.nodes.push(Node {
                id: id(definition),
                definition: definition.clone(),
                depth,
            });
            if depth >= max_depth {
                continue;
            }
            for call in &definition.calls {
                let candidates = by_name.get(call.as_str()).cloned().unwrap_or_default();
                let local = candidates
                    .iter()
                    .filter(|candidate| candidate.path == definition.path)
                    .collect::<Vec<_>>();
                let (target, how) = match (candidates.as_slice(), local.as_slice()) {
                    ([single], _) => (Some(*single), "unique name"),
                    (_, [single]) => (Some(**single), "same file"),
                    ([], _) => {
                        unresolved_calls.insert(format!("{} calls {call}: no definition in the repository (external or dynamic)", id(definition)));
                        (None, "")
                    }
                    (many, _) => {
                        unresolved_calls.insert(format!(
                            "{} calls {call}: ambiguous among {} definitions",
                            id(definition),
                            many.len()
                        ));
                        (None, "")
                    }
                };
                if let Some(target) = target {
                    graph.edges.push((id(definition), id(target), how.into()));
                    if seen.insert(id(target)) {
                        queue.push_back((target, depth + 1));
                    }
                }
            }
        }
        if !unresolved_calls.is_empty() && graph.status == "RESOLVED" {
            graph.status = "PARTIAL";
        }
        graph.unresolved.extend(unresolved_calls);
        graph
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /** Check definitions, qualification, and calls in Python and brace languages
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn indexes_definitions_and_calls() {
        let python = "class Service:\n    def submit(self, x):\n        # charge(x) in a comment\n        return self.charge(x) + fee(x)\n\n    def charge(self, x):\n        return x\n\ndef fee(x):\n    return round(x * 0.1)\n";
        let found = definitions("pay.py", python).unwrap();
        let names = found
            .iter()
            .map(|definition| definition.qualified.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Service.submit", "Service.charge", "fee"]);
        assert_eq!(found[0].calls, vec!["charge", "fee"]);
        assert_eq!((found[0].start_line, found[0].end_line), (2, 4));
        let rust = "struct A;\nimpl A {\n    pub fn run(&self) -> u32 {\n        helper(\"x(\")\n    }\n}\n\nfn helper(s: &str) -> u32 {\n    if s.len() > 0 { 1 } else { 0 }\n}\n";
        let found = definitions("a.rs", rust).unwrap();
        assert_eq!(
            found
                .iter()
                .map(|definition| definition.qualified.as_str())
                .collect::<Vec<_>>(),
            vec!["A.run", "helper"]
        );
        assert_eq!(found[0].calls, vec!["helper"]);
        let js = "export function submit(p) {\n  validate(p);\n  return api.post(p);\n}\nfunction validate(p) {\n  return true;\n}\n";
        let found = definitions("a.js", js).unwrap();
        assert_eq!(found[0].calls, vec!["validate", "post"]);
        assert!(definitions("a.json", "{}").is_none());
    }

    /** Check graph discovery, ambiguity, external calls, and unresolved entry points
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn discovers_graphs_honestly() {
        let index = Index::build([
            ("pay.py".to_string(), "def submit(x):\n    return charge(x) + log(x)\n\ndef charge(x):\n    return send(x)\n".to_string()),
            ("a.py".to_string(), "def log(x):\n    pass\n".to_string()),
            ("b.py".to_string(), "def log(x):\n    pass\n".to_string()),
        ]);
        let graph = index.discover(&["submit".into()], 8);
        assert_eq!(graph.status, "PARTIAL");
        assert_eq!(graph.nodes.len(), 2);
        assert!(graph
            .unresolved
            .iter()
            .any(|reason| reason.contains("log: ambiguous")));
        assert!(graph
            .unresolved
            .iter()
            .any(|reason| reason.contains("send: no definition")));
        let missing = index.discover(&["nothing".into()], 8);
        assert_eq!(missing.status, "UNRESOLVED");
        let ambiguous = index.discover(&["log".into()], 8);
        assert_eq!(ambiguous.status, "UNRESOLVED");
        let qualified = index.discover(&["a.py:log".into()], 8);
        assert_eq!(qualified.status, "RESOLVED");
    }
}
