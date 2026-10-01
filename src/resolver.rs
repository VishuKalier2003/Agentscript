use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::Path;

use tree_sitter::{Node, Parser, Tree};

use crate::model::{ItemKind, SourceTarget};
use crate::repository::{ensure_commit, git};
use crate::util::io_error;

/** Outcome of looking up a target in the worktree or a checkpoint commit
 * Variants
    - Found(SourceTarget) - exactly one matching definition
    - Missing - no match in any supported file
    - Duplicate(usize) - more than one match, with the count
    - Unsupported - no match, and only unsupported source languages were present
    - ParseFailure(String) - a supported file failed to parse, with the file and error
*/
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolution {
    Found(SourceTarget),
    Missing,
    Duplicate(usize),
    Unsupported,
    ParseFailure(String),
}

/** Select the tree-sitter grammar for a source file, by reading the path's extension, lowercasing
 * it, and matching it against the supported languages
 * Input
    - path: &str - source file path
 * Output
    - Option<tree_sitter::Language>
    - None if the extension is missing or unsupported
*/
pub(crate) fn language(path: &str) -> Option<tree_sitter::Language> {
    // Select a parser from the source extension and fail closed for unknown languages
    match Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase()
        .as_str()
    {
        // Define the tree sitter we can use for the language
        "java" => Some(tree_sitter_java::language()),
        "js" | "jsx" | "mjs" | "cjs" => Some(tree_sitter_javascript::language()),
        "py" => Some(tree_sitter_python::language()),
        "rs" => Some(tree_sitter_rust::language()),
        _ => None,
    }
}

/** Return the prefixes that start a comment in a source file's language, by matching the path's
 * extension: hash for Python, and double slash (line) or slash-star (block, including doc
 * comments) for Java, JavaScript, and Rust; a syntax node is only treated as a comment if its
 * text starts with one of these, so a grammar node merely named like a comment can never hide code
 * Input
    - path: &str - source file path
 * Output
    - &'static [&'static str] of comment prefixes, empty for unsupported languages
*/
pub(crate) fn comment_markers(path: &str) -> &'static [&'static str] {
    match Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("py") => &["#"],
        Some("java" | "js" | "jsx" | "mjs" | "cjs" | "rs") => &["//", "/*"],
        _ => &[],
    }
}

/** List the supported source extensions for diagnostics, by returning a fixed string kept in sync
 * with language
 * Input
    - None
 * Output
    - &'static str of comma-separated extensions
*/
pub(crate) fn supported_extensions() -> &'static str {
    // Keep supported-language diagnostics aligned with parser selection
    ".java, .js, .jsx, .mjs, .cjs, .py, .rs"
}

/** Find every item of a kind matching a target in one source file, by parsing the source with
 * the grammar for its extension (rejecting trees with parse errors), walking every node, keeping
 * the nodes match_item accepts whose name and enclosing types match the target, and reducing each
 * to canonical source text (comments removed, layout kept)
 * Input
    - source: &str - file contents
    - path: &str - file path, used to pick the language
    - kind: ItemKind - function, data, variable, class, or interface
    - target: &str - qualified target such as Type.method or Type::method
 * Output
    - Result<Vec<String>, String> with one canonical snippet per match
    - Error if the language is unsupported or the source does not parse
*/
pub(crate) fn extract_items(
    source: &str,
    path: &str,
    kind: ItemKind,
    target: &str,
) -> Result<Vec<String>, String> {
    let tree = parse_tree(source, path)?;
    let markers = comment_markers(path);
    let bytes = source.as_bytes();
    let mut output = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if let Some((name, region)) = match_item(node, bytes, kind) {
            let mut qualified = enclosing_types(node, bytes);
            qualified.push(name.clone());
            if matches_target(&name, &qualified.join("."), target) {
                output.push((
                    node.start_byte(),
                    canonical_source_node(region, bytes, markers),
                ));
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    output.sort_by_key(|(start, _)| *start);
    Ok(output.into_iter().map(|(_, snippet)| snippet).collect())
}

/** Check whether an item is the requested target: a single-segment target matches by name, a
 * qualified target must match the enclosing types and name exactly (Rust :: is treated like .)
 * Input
    - name: &str - the item's own name
    - qualified: &str - enclosing type names and the name joined with "."
    - target: &str - qualified target from a rule
 * Output
    - bool, true if the item is the target
*/
pub(crate) fn matches_target(name: &str, qualified: &str, target: &str) -> bool {
    let normalized = target.replace("::", ".");
    if normalized.contains('.') {
        qualified == normalized
    } else {
        name == normalized
    }
}

/** Syntax node kinds that define a class-like item: classes, and Rust structs and enums */
pub(crate) const CLASS_KINDS: &[&str] = &[
    "class_declaration",
    "class_definition",
    "struct_item",
    "enum_item",
];

/** Syntax node kinds that define an interface-like item: interfaces, and Rust traits */
const INTERFACE_KINDS: &[&str] = &["interface_declaration", "trait_item"];

/** Syntax node kinds whose parent statement holds a variable's type and modifiers */
const DECLARATION_PARENTS: &[&str] = &[
    "field_declaration",
    "local_variable_declaration",
    "lexical_declaration",
    "variable_declaration",
];

/** Decide whether a node is an item of the given kind, and if so return its name and the node
 * whose code the rule covers: the definition itself for functions, classes, and interfaces; for
 * variables the whole declaration (Java field or local, JavaScript let/const/var, Python
 * assignment to a plain name, Rust let/const/static); and for data only the assigned value
 * Input
    - node: Node - candidate syntax node
    - bytes: &[u8] - source bytes
    - kind: ItemKind - kind being looked for
 * Output
    - Option<(String, Node)> name and covered node, None if the node is not such an item
*/
pub(crate) fn match_item<'tree>(
    node: Node<'tree>,
    bytes: &[u8],
    kind: ItemKind,
) -> Option<(String, Node<'tree>)> {
    let text = |node: Node| node.utf8_text(bytes).unwrap_or_default().to_string();
    let kinds = match kind {
        ItemKind::Function => DEFINITION_KINDS,
        ItemKind::Class => CLASS_KINDS,
        ItemKind::Interface => INTERFACE_KINDS,
        ItemKind::Data | ItemKind::Variable => {
            // A variable is a declarator (Java, JavaScript), a Python assignment, or a Rust binding
            let (name, value) = match node.kind() {
                "variable_declarator" | "const_item" | "static_item" => (
                    node.child_by_field_name("name")?,
                    node.child_by_field_name("value"),
                ),
                "assignment" => (
                    node.child_by_field_name("left")?,
                    node.child_by_field_name("right"),
                ),
                "let_declaration" => (
                    node.child_by_field_name("pattern")?,
                    node.child_by_field_name("value"),
                ),
                _ => return None,
            };
            if !name.kind().contains("identifier") {
                return None; // destructuring and attribute targets are not named variables
            }
            let region = match kind {
                ItemKind::Data => value?,
                _ => node
                    .parent()
                    .filter(|parent| DECLARATION_PARENTS.contains(&parent.kind()))
                    .unwrap_or(node),
            };
            return Some((text(name), region));
        }
    };
    if !kinds.contains(&node.kind()) {
        return None;
    }
    Some((text(node.child_by_field_name("name")?), node))
}

thread_local! {
    /** One configured tree-sitter parser per file extension, reused for every file in a run */
    static PARSERS: RefCell<HashMap<String, Parser>> = RefCell::new(HashMap::new());
}

/** Parse a source file into a syntax tree, by selecting the grammar from the path's extension,
 * reusing (or creating once) the parser for that extension, running tree-sitter, and rejecting
 * any tree that contains a parse error
 * Input
    - source: &str - file contents
    - path: &str - file path, used to pick the language
 * Output
    - Result<Tree, String>
    - Error if the language is unsupported or the source does not parse
*/
fn parse_tree(source: &str, path: &str) -> Result<Tree, String> {
    let language = language(path).ok_or_else(|| "unsupported source language".to_string())?;
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let tree = parse_with(source, &extension, language)?;
    if tree.root_node().has_error() {
        return Err("source contains a parser error".into());
    }
    Ok(tree)
}

/** Parse source text with a given grammar through the shared per-thread parser cache, without
 * judging parse errors; enforcement goes through parse_tree, which rejects them, while discovery
 * tolerates them and only reports them
 * Input
    - source: &str - file contents
    - key: &str - parser cache key, the lowercase file extension (one grammar per extension)
    - language: tree_sitter::Language - grammar to use
 * Output
    - Result<Tree, String>
    - Error if the grammar cannot be loaded or tree-sitter returns no tree
*/
pub(crate) fn parse_with(
    source: &str,
    key: &str,
    language: tree_sitter::Language,
) -> Result<Tree, String> {
    PARSERS.with(|parsers| -> Result<Tree, String> {
        let mut parsers = parsers.borrow_mut();
        let parser = match parsers.entry(key.to_string()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let mut parser = Parser::new();
                parser
                    .set_language(language)
                    .map_err(|error| error.to_string())?;
                entry.insert(parser)
            }
        };
        parser
            .parse(source, None)
            .ok_or_else(|| "parser returned no syntax tree".to_string())
    })
}

/** List the names of the types enclosing a node, outermost first, by climbing its ancestors and
 * keeping the name (or, for Rust impl blocks, the type) of every class, struct, or impl
 * Input
    - node: Node - definition node
    - bytes: &[u8] - source bytes
 * Output
    - Vec<String> of enclosing type names
*/
pub(crate) fn enclosing_types(node: Node, bytes: &[u8]) -> Vec<String> {
    let mut qualified = Vec::new();
    let mut parent = node.parent();
    while let Some(ancestor) = parent {
        let named = ancestor
            .child_by_field_name("name")
            .or_else(|| ancestor.child_by_field_name("type"));
        if let Some(name) = named {
            if ancestor.kind().contains("class")
                || ancestor.kind().contains("struct")
                || ancestor.kind().contains("impl")
                || ancestor.kind().contains("interface")
                || ancestor.kind().contains("trait")
            {
                qualified.push(name.utf8_text(bytes).unwrap_or_default().to_string());
            }
        }
        parent = ancestor.parent();
    }
    qualified.reverse();
    qualified
}

/** An item (function, data, variable, class, or interface) found in a source file, used to
 * resolve scopes and trace flows
 * Fields
    - kind: ItemKind - what the item is
    - name: String - the item's own name
    - qualified: String - enclosing type names and the item name joined with "."
    - calls: Vec<String> - names of every function called inside the item
    - uses: Vec<String> - every identifier mentioned inside a function, so flows can find the
      functions that read a variable or use a class (empty for other kinds)
    - snippet: String - canonical source text of the item
    - features: Features - measurements used to classify changes for target rules
*/
pub(crate) struct Definition {
    pub(crate) kind: ItemKind,
    pub(crate) name: String,
    pub(crate) qualified: String,
    pub(crate) calls: Vec<String>,
    pub(crate) uses: Vec<String>,
    pub(crate) snippet: String,
    pub(crate) features: Features,
}

/** Syntax node kinds that define a callable function or method in the supported grammars */
pub(crate) const DEFINITION_KINDS: &[&str] = &[
    "method_declaration",
    "constructor_declaration",
    "function_declaration",
    "generator_function_declaration",
    "method_definition",
    "function_definition",
    "function_item",
    "function_signature_item",
];

/** List every item of every kind defined in a source file, by parsing it and walking the tree,
 * recording for each node match_item accepts its kind, qualified name, the names it calls (and,
 * for functions, the identifiers it uses), its canonical text, and its measurements
 * Input
    - source: &str - file contents
    - path: &str - file path, used to pick the language
 * Output
    - Result<Vec<Definition>, String> in source order
    - Error if the language is unsupported or the source does not parse
*/
pub(crate) fn definitions(source: &str, path: &str) -> Result<Vec<Definition>, String> {
    let tree = parse_tree(source, path)?;
    let markers = comment_markers(path);
    let bytes = source.as_bytes();
    let mut output = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        for kind in ItemKind::ALL {
            let Some((name, region)) = match_item(node, bytes, kind) else {
                continue;
            };
            let mut qualified = enclosing_types(node, bytes);
            qualified.push(name.clone());
            let mut calls = Vec::new();
            collect_calls(region, bytes, &mut calls);
            calls.sort();
            calls.dedup();
            let mut uses = Vec::new();
            if kind == ItemKind::Function {
                collect_identifiers(region, bytes, &mut uses);
                uses.sort();
                uses.dedup();
            }
            output.push((
                node.start_byte(),
                Definition {
                    kind,
                    name,
                    qualified: qualified.join("."),
                    calls,
                    uses,
                    snippet: canonical_source_node(region, bytes, markers),
                    features: features_of(region, bytes, markers),
                },
            ));
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    output.sort_by_key(|(start, _)| *start);
    Ok(output
        .into_iter()
        .map(|(_, definition)| definition)
        .collect())
}

/** Collect every identifier mentioned inside a node, by walking its subtree and recording the
 * text of each identifier leaf, so a flow can find the functions that use a variable or class
 * Input
    - node: Node - subtree root
    - bytes: &[u8] - source bytes
    - output: &mut Vec<String> - collected identifier names
 * Output
    - None (appends to output)
*/
fn collect_identifiers(node: Node, bytes: &[u8], output: &mut Vec<String>) {
    if node.child_count() == 0 && node.kind().contains("identifier") {
        output.push(node.utf8_text(bytes).unwrap_or_default().to_string());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifiers(child, bytes, output);
    }
}

/** Collect the names of functions called inside a node, by walking its subtree and reading the
 * callee name of each call: Java method invocations, JavaScript and Rust call expressions, and
 * Python calls, taking the last segment of member, attribute, field, and path callees
 * Input
    - node: Node - subtree root
    - bytes: &[u8] - source bytes
    - output: &mut Vec<String> - collected callee names
 * Output
    - None (appends to output)
*/
pub(crate) fn collect_calls(node: Node, bytes: &[u8], output: &mut Vec<String>) {
    let callee = match node.kind() {
        "method_invocation" => node.child_by_field_name("name"),
        "call_expression" | "call" => node.child_by_field_name("function").and_then(callee_name),
        _ => None,
    };
    if let Some(callee) = callee {
        output.push(callee.utf8_text(bytes).unwrap_or_default().to_string());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_calls(child, bytes, output);
    }
}

/** Find the node naming the called function in a callee expression, by returning a plain
 * identifier as is and otherwise taking the member/attribute/field/path name, unwrapping Rust
 * turbofish calls first
 * Input
    - node: Node - the call's function expression
 * Output
    - Option<Node> of the name, None for callees such as closures or computed members
*/
fn callee_name(node: Node) -> Option<Node> {
    match node.kind() {
        "identifier" => Some(node),
        "member_expression" => node.child_by_field_name("property"),
        "attribute" => node.child_by_field_name("attribute"),
        "field_expression" => node.child_by_field_name("field"),
        "scoped_identifier" => node.child_by_field_name("name"),
        "generic_function" => node.child_by_field_name("function").and_then(callee_name),
        _ => None,
    }
}

/** Canonicalize a whole file for file, folder, and all scopes, by stripping comments with the same
 * rules as function comparison for supported source languages, and only normalizing CRLF line
 * endings for every other file
 * Input
    - source: &str - file contents
    - path: &str - file path, used to pick the language
 * Output
    - Result<String, String> canonical file text
    - Error if a supported source file does not parse
*/
pub(crate) fn canonical_file(source: &str, path: &str) -> Result<String, String> {
    if language(path).is_none() {
        return Ok(source.replace("\r\n", "\n"));
    }
    let tree = parse_tree(source, path)?;
    Ok(canonical_source_node(
        tree.root_node(),
        source.as_bytes(),
        comment_markers(path),
    ))
}

/** Measurements of a piece of code that let a target rule classify how it changed
 * Fields
    - text: String - the source text with line endings normalized and trailing whitespace
      trimmed; any difference means the code changed (comments and layout included)
    - shape: Vec<String> - leaf tokens in order with comments removed and every identifier
      replaced by "<idN>", numbered by first appearance; differs when code is added, removed, or
      reordered, but not when names are consistently renamed or comments or layout change
    - business: BTreeMap<String, usize> - multiset of literals, operators, and called function
      names; differs when what the code computes or calls changes
    - complexity: Complexity - loop and branch counts
*/
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Features {
    pub(crate) text: String,
    pub(crate) shape: Vec<String>,
    pub(crate) business: BTreeMap<String, usize>,
    pub(crate) complexity: Complexity,
}

/** Control-flow measurements of a piece of code
 * Fields
    - loops: usize - number of loops, including comprehensions
    - max_loop_depth: usize - deepest nesting of loops inside each other
    - branches: usize - number of conditionals, match/switch arms, and exception handlers
*/
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Complexity {
    pub(crate) loops: usize,
    pub(crate) max_loop_depth: usize,
    pub(crate) branches: usize,
}

/** Syntax node kinds that are loops in the supported grammars */
pub(crate) const LOOP_KINDS: &[&str] = &[
    "for_statement",
    "enhanced_for_statement",
    "while_statement",
    "do_statement",
    "for_in_statement",
    "for_expression",
    "while_expression",
    "loop_expression",
    "list_comprehension",
    "set_comprehension",
    "dictionary_comprehension",
    "generator_expression",
];

/** Syntax node kinds that branch control flow in the supported grammars */
pub(crate) const BRANCH_KINDS: &[&str] = &[
    "if_statement",
    "if_expression",
    "elif_clause",
    "conditional_expression",
    "ternary_expression",
    "switch_label",
    "switch_case",
    "match_arm",
    "case_clause",
    "catch_clause",
    "except_clause",
];

/** Operator tokens counted as business logic; "=" is included so adding or removing an assignment
 * counts, and Python's word operators are included */
const OPERATORS: &[&str] = &[
    "+",
    "-",
    "*",
    "/",
    "%",
    "**",
    "//",
    "==",
    "!=",
    "===",
    "!==",
    "<",
    ">",
    "<=",
    ">=",
    "&&",
    "||",
    "!",
    "&",
    "|",
    "^",
    "~",
    "<<",
    ">>",
    ">>>",
    "=",
    "+=",
    "-=",
    "*=",
    "/=",
    "%=",
    "**=",
    "//=",
    "&=",
    "|=",
    "^=",
    "<<=",
    ">>=",
    "++",
    "--",
    "??",
    "and",
    "or",
    "not",
    "in",
    "is",
    "instanceof",
];

/** Check whether a node kind is a literal value, by matching the literal kinds of the supported
 * grammars (Java and Rust "*_literal", JavaScript and Python number, string, and constant kinds)
 * Input
    - kind: &str - syntax node kind
 * Output
    - bool, true for literals
*/
fn is_literal(kind: &str) -> bool {
    kind.ends_with("_literal")
        || matches!(
            kind,
            "number"
                | "string"
                | "template_string"
                | "concatenated_string"
                | "integer"
                | "float"
                | "true"
                | "false"
                | "null"
                | "none"
                | "undefined"
                | "regex"
        )
}

/** Check whether a node is a comment in its language, by requiring the grammar kind to name a
 * comment and the text to start with one of the language's comment markers
 * Input
    - node: Node - syntax node
    - bytes: &[u8] - source bytes
    - markers: &[&str] - comment prefixes for the file's language
 * Output
    - bool, true for comments
*/
fn is_comment(node: Node, bytes: &[u8], markers: &[&str]) -> bool {
    let text = &bytes[node.byte_range()];
    node.kind().contains("comment")
        && markers
            .iter()
            .any(|marker| text.starts_with(marker.as_bytes()))
}

/** Measure a syntax node for change classification, by recording its normalized text and walking
 * its subtree once to build the masked shape, the business multiset, and the complexity counts
 * Input
    - node: Node - definition or file root node
    - bytes: &[u8] - source bytes
    - markers: &[&str] - comment prefixes for the file's language
 * Output
    - Features of the node
*/
fn features_of(node: Node, bytes: &[u8], markers: &[&str]) -> Features {
    let mut features = Features {
        text: normalize_text(&String::from_utf8_lossy(&bytes[node.byte_range()])),
        ..Features::default()
    };
    measure(node, bytes, markers, 0, &mut features);
    // Number identifiers by first appearance: consistent renames keep the shape, reorders do not
    let mut numbers = HashMap::new();
    for token in &mut features.shape {
        if let Some(name) = token.strip_prefix(IDENTIFIER_MARK) {
            let next = numbers.len();
            let number = *numbers.entry(name.to_string()).or_insert(next);
            *token = format!("<id{number}>");
        }
    }
    features
}

/** Prefix marking an identifier token in a shape until features_of numbers it */
const IDENTIFIER_MARK: char = '\u{1}';

/** Walk one node for features_of, by skipping comments, recording literals whole (with their
 * text in both shape and business), counting loops (with nesting depth) and branches, recording
 * each call's callee name, and at leaf tokens adding the marked identifier name (numbered later
 * by features_of) or the token text otherwise (counting operator tokens as business)
 * Input
    - node: Node - current syntax node
    - bytes: &[u8] - source bytes
    - markers: &[&str] - comment prefixes for the file's language
    - loop_depth: usize - number of loops enclosing this node
    - features: &mut Features - measurements being built
 * Output
    - None (updates features)
*/
fn measure(node: Node, bytes: &[u8], markers: &[&str], loop_depth: usize, features: &mut Features) {
    if is_comment(node, bytes, markers) {
        return;
    }
    let kind = node.kind();
    let text = || {
        String::from_utf8_lossy(&bytes[node.byte_range()])
            .replace("\r\n", "\n")
            .to_string()
    };
    if node.is_named() && is_literal(kind) {
        let literal = text();
        *features
            .business
            .entry(format!("literal {literal}"))
            .or_default() += 1;
        features.shape.push(literal);
        return;
    }
    let mut depth = loop_depth;
    if LOOP_KINDS.contains(&kind) {
        depth += 1;
        features.complexity.loops += 1;
        features.complexity.max_loop_depth = features.complexity.max_loop_depth.max(depth);
    }
    if BRANCH_KINDS.contains(&kind) {
        features.complexity.branches += 1;
    }
    let callee = match kind {
        "method_invocation" => node.child_by_field_name("name"),
        "call_expression" | "call" => node.child_by_field_name("function").and_then(callee_name),
        _ => None,
    };
    if let Some(callee) = callee {
        let name = callee.utf8_text(bytes).unwrap_or_default();
        *features.business.entry(format!("call {name}")).or_default() += 1;
    }
    if node.child_count() == 0 {
        if kind.contains("identifier") {
            features.shape.push(format!("{IDENTIFIER_MARK}{}", text()));
        } else {
            let token = text();
            if !node.is_named() && OPERATORS.contains(&token.as_str()) {
                *features
                    .business
                    .entry(format!("operator {token}"))
                    .or_default() += 1;
            }
            features.shape.push(token);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        measure(child, bytes, markers, depth, features);
    }
}

/** Normalize source text for change detection, by converting CRLF to LF and trimming trailing
 * whitespace from every line
 * Input
    - text: &str - raw source text
 * Output
    - String normalized text
*/
fn normalize_text(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/** Measure a whole file for file, folder, and all target scopes, by parsing supported source files
 * and measuring the root node, and for any other file recording only its normalized text (so a
 * change to documentation or data counts as a wording change)
 * Input
    - source: &str - file contents
    - path: &str - file path, used to pick the language
 * Output
    - Result<Features, String>
    - Error if a supported source file does not parse
*/
pub(crate) fn file_features(source: &str, path: &str) -> Result<Features, String> {
    if language(path).is_none() {
        return Ok(Features {
            text: normalize_text(source),
            ..Features::default()
        });
    }
    let tree = parse_tree(source, path)?;
    Ok(features_of(
        tree.root_node(),
        source.as_bytes(),
        comment_markers(path),
    ))
}

/** Reduce a definition to its source text with only comments removed, by first collecting the byte
 * ranges of comment nodes (grammar comment nodes whose text starts with the language's comment
 * marker), then copying the definition's text while skipping those ranges together with the spaces
 * that separated them from code, and finally dropping trailing whitespace and blank lines;
 * indentation, spacing within lines, and line breaks are kept, so layout changes (which alter
 * meaning in Python) are detected
 * Input
    - node: Node - matched definition node
    - bytes: &[u8] - source bytes
    - markers: &[&str] - comment prefixes for the file's language
 * Output
    - String of the definition's lines without comments, joined by \n
*/
fn canonical_source_node(node: Node, bytes: &[u8], markers: &[&str]) -> String {
    let mut comments = Vec::new();
    collect_comments(node, bytes, markers, &mut comments);

    let mut text = String::new();
    let mut position = node.start_byte();
    for (start, end) in comments {
        text.push_str(&String::from_utf8_lossy(&bytes[position..start]));
        let line_prefix = text.rsplit('\n').next().unwrap_or_default();
        let mut resume = end;
        if line_prefix.trim().is_empty() {
            // Comment starts its line: keep the indentation, drop the spaces after the comment
            while matches!(bytes.get(resume), Some(b' ' | b'\t')) && resume < node.end_byte() {
                resume += 1;
            }
        } else {
            // Comment follows code: drop the spaces that separated it from that code
            text.truncate(text.trim_end_matches([' ', '\t']).len());
        }
        position = resume;
    }
    text.push_str(&String::from_utf8_lossy(&bytes[position..node.end_byte()]));

    // Trailing spaces, \r from CRLF checkouts, and lines left empty by removed comments carry no code
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/** Collect the byte ranges of comments inside a node, by walking the subtree in source order and
 * recording any node whose grammar kind names it a comment and whose text starts with one of the
 * language's comment markers, without descending into recorded comments
 * Input
    - node: Node - subtree root
    - bytes: &[u8] - source bytes
    - markers: &[&str] - comment prefixes for the file's language
    - output: &mut Vec<(usize, usize)> - collected (start, end) byte ranges
 * Output
    - None (appends to output)
*/
fn collect_comments(node: Node, bytes: &[u8], markers: &[&str], output: &mut Vec<(usize, usize)>) {
    if is_comment(node, bytes, markers) {
        output.push((node.start_byte(), node.end_byte()));
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_comments(child, bytes, markers, output);
    }
}

/** Check whether Crane can parse a file, by asking language for a grammar for its path
 * Input
    - path: &str - file path
 * Output
    - bool, true if a grammar exists
*/
pub(crate) fn supported(path: &str) -> bool {
    // Identify files that Crane can parse for target resolution
    language(path).is_some()
}

/** Check whether a file looks like source code, supported or not, by matching its extension
 * against a wider list of common languages so unsupported sources can be reported instead of
 * silently treated as missing
 * Input
    - path: &str - file path
 * Output
    - bool, true if the extension is a known source language
*/
fn source_candidate(path: &str) -> bool {
    // Track recognizable but unsupported source files for fail-closed diagnostics
    matches!(
        Path::new(path).extension().and_then(|value| value.to_str()),
        Some(
            "java"
                | "js"
                | "jsx"
                | "mjs"
                | "cjs"
                | "py"
                | "rs"
                | "ts"
                | "tsx"
                | "go"
                | "rb"
                | "php"
                | "c"
                | "h"
                | "cpp"
                | "cc"
        )
    )
}

/** Resolve a target in the current working tree, by walking directories with a stack from the
 * current directory (skipping .git, .crane, and target), extracting matches from every supported
 * file, and finally classifying the result as found, missing, duplicate, or unsupported
 * Input
    - kind: ItemKind - function, data, variable, class, or interface
    - target: &str - qualified target
 * Output
    - Result<Resolution, String>
    - ParseFailure resolution if a supported file does not parse
    - Error if a directory or file cannot be read
*/
pub(crate) fn resolve_worktree(kind: ItemKind, target: &str) -> Result<Resolution, String> {
    // Scan the current worktree while excluding generated and Crane metadata
    let mut stack = vec![env::current_dir().map_err(io_error)?];
    let mut unsupported = false;
    let mut supported_seen = false;
    let mut matches = Vec::new();
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(directory).map_err(io_error)? {
            let path = entry.map_err(io_error)?.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("");
                if name != ".git" && name != ".crane" && name != "target" {
                    stack.push(path);
                }
            } else if supported(&path.to_string_lossy()) {
                supported_seen = true;
                let source = fs::read_to_string(&path).map_err(io_error)?;
                match extract_items(&source, &path.to_string_lossy(), kind, target) {
                    Ok(found) => {
                        matches.extend(found.into_iter().map(|snippet| SourceTarget { snippet }))
                    }
                    Err(error) => {
                        return Ok(Resolution::ParseFailure(format!(
                            "{}: {error}",
                            path.display()
                        )))
                    }
                }
            } else if source_candidate(&path.to_string_lossy()) {
                unsupported = true;
            }
        }
    }
    match matches.len() {
        0 if unsupported && !supported_seen => Ok(Resolution::Unsupported),
        0 => Ok(Resolution::Missing),
        1 => Ok(Resolution::Found(matches.remove(0))),
        count => Ok(Resolution::Duplicate(count)),
    }
}

/** Resolve what a protected target looked like at a checkpoint commit, by first confirming the
 * commit exists, then listing its files with git ls-tree, reading each supported file with git
 * show, extracting matches, and finally classifying the result like resolve_worktree
 * Input
    - commit: &str - checkpoint commit SHA
    - kind: ItemKind - function, data, variable, class, or interface
    - target: &str - qualified target
 * Output
    - Result<Resolution, String>
    - ParseFailure resolution if a supported file does not parse
    - Error if the commit is missing or a Git command fails
*/
pub(crate) fn resolve_git(
    commit: &str,
    kind: ItemKind,
    target: &str,
) -> Result<Resolution, String> {
    // Resolve the same target from an immutable Git commit for baseline comparison
    ensure_commit(commit)?;
    let files = git(&["ls-tree", "-r", "--name-only", commit])?;
    let mut unsupported = false;
    let mut supported_seen = false;
    let mut matches = Vec::new();
    for path in files.lines().filter(|path| supported(path)) {
        supported_seen = true;
        let source = git(&["show", &format!("{commit}:{path}")])?;
        match extract_items(&source, path, kind, target) {
            Ok(found) => matches.extend(found.into_iter().map(|snippet| SourceTarget { snippet })),
            Err(error) => return Ok(Resolution::ParseFailure(format!("{path}: {error}"))),
        }
    }
    if files.lines().any(source_candidate)
        && files
            .lines()
            .any(|path| source_candidate(path) && !supported(path))
    {
        unsupported = true;
    }
    match matches.len() {
        0 if unsupported && !supported_seen => Ok(Resolution::Unsupported),
        0 => Ok(Resolution::Missing),
        1 => Ok(Resolution::Found(matches.remove(0))),
        count => Ok(Resolution::Duplicate(count)),
    }
}
