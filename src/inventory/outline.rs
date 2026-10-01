use std::path::Path;

use serde_json::{json, Value};
use tree_sitter::{Language, Node};

use crate::model::ItemKind;
use crate::resolver::{
    collect_calls, enclosing_types, match_item, parse_with, BRANCH_KINDS, DEFINITION_KINDS,
    LOOP_KINDS,
};

/** Version of the extraction rules; outlines cached by another version are extracted again (2
 * added a content digest per symbol, used to detect symbol-level effects)
*/
pub(crate) const EXTRACTOR_VERSION: u64 = 2;

/** Largest file discovery parses; bigger files are usually generated, minified, or data */
const MAX_PARSE_BYTES: usize = 1_000_000;

/** How symbols are read from a language's syntax tree
 * Variants
    - Shared - Java, JavaScript, Python, and Rust: the resolver's own item matcher, so names are
      exactly what policies resolve
    - TypeScript - the resolver's matcher plus TypeScript-only declarations
    - Go - Go declarations, with methods qualified by their receiver type
    - Cpp - C and C++ definitions, with namespaces and Class::method definitions
    - Kotlin - Kotlin declarations
    - Text - no grammar; the file is counted but not parsed
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Shared,
    TypeScript,
    Go,
    Cpp,
    Kotlin,
    Text,
}

/** A language discovery recognizes
 * Fields
    - name: &'static str - language name used in the inventory and in symbol ids
    - grammar: Option<fn() -> Language> - tree-sitter grammar, None for counted-only languages
    - enforceable: bool - whether policies can target its items today (the resolver's languages)
    - family: Family - how its symbols are read
*/
pub(crate) struct LanguageSpec {
    pub(crate) name: &'static str,
    grammar: Option<fn() -> Language>,
    pub(crate) enforceable: bool,
    family: Family,
}

/** Build a language entry that discovery parses
 * Input
    - name: &'static str - language name
    - grammar: fn() -> Language - tree-sitter grammar
    - enforceable: bool - whether policies can target it
    - family: Family - how symbols are read
 * Output
    - LanguageSpec
*/
const fn parsed(
    name: &'static str,
    grammar: fn() -> Language,
    enforceable: bool,
    family: Family,
) -> LanguageSpec {
    LanguageSpec {
        name,
        grammar: Some(grammar),
        enforceable,
        family,
    }
}

/** Build a language entry that discovery only counts
 * Input
    - name: &'static str - language name
 * Output
    - LanguageSpec without a grammar
*/
const fn counted(name: &'static str) -> LanguageSpec {
    LanguageSpec {
        name,
        grammar: None,
        enforceable: false,
        family: Family::Text,
    }
}

/** Return the TypeScript grammar (the crate exposes two grammars, so it needs a wrapper)
 * Input
    - None
 * Output
    - Language
*/
fn typescript() -> Language {
    tree_sitter_typescript::language_typescript()
}

/** Return the TSX grammar
 * Input
    - None
 * Output
    - Language
*/
fn tsx() -> Language {
    tree_sitter_typescript::language_tsx()
}

static JAVA: LanguageSpec = parsed("java", tree_sitter_java::language, true, Family::Shared);
static JAVASCRIPT: LanguageSpec = parsed(
    "javascript",
    tree_sitter_javascript::language,
    true,
    Family::Shared,
);
static PYTHON: LanguageSpec = parsed("python", tree_sitter_python::language, true, Family::Shared);
static RUST: LanguageSpec = parsed("rust", tree_sitter_rust::language, true, Family::Shared);
static TYPESCRIPT: LanguageSpec = parsed("typescript", typescript, false, Family::TypeScript);
static TSX: LanguageSpec = parsed("typescript", tsx, false, Family::TypeScript);
static GO: LanguageSpec = parsed("go", tree_sitter_go::language, false, Family::Go);
static C: LanguageSpec = parsed("c", tree_sitter_cpp::language, false, Family::Cpp);
static CPP: LanguageSpec = parsed("cpp", tree_sitter_cpp::language, false, Family::Cpp);
static KOTLIN: LanguageSpec = parsed(
    "kotlin",
    tree_sitter_kotlin::language,
    false,
    Family::Kotlin,
);
static COUNTED: [LanguageSpec; 19] = [
    counted("ruby"),
    counted("php"),
    counted("csharp"),
    counted("swift"),
    counted("scala"),
    counted("shell"),
    counted("sql"),
    counted("markdown"),
    counted("yaml"),
    counted("json"),
    counted("toml"),
    counted("xml"),
    counted("html"),
    counted("css"),
    counted("protobuf"),
    counted("terraform"),
    counted("groovy"),
    counted("dockerfile"),
    counted("make"),
];

/** Find the language of a file from its extension (or, for build files, its name)
 * Input
    - path: &str - repository-relative path
 * Output
    - Option<&'static LanguageSpec>, None for unknown files
*/
pub(crate) fn language_of(path: &str) -> Option<&'static LanguageSpec> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name == "Dockerfile" || name.starts_with("Dockerfile.") {
        return Some(&COUNTED[17]);
    }
    if name == "Makefile" || name == "CMakeLists.txt" {
        return Some(&COUNTED[18]);
    }
    let extension = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    let index = |language: &str| COUNTED.iter().position(|spec| spec.name == language);
    Some(match extension.as_str() {
        "java" => &JAVA,
        "js" | "jsx" | "mjs" | "cjs" => &JAVASCRIPT,
        "py" => &PYTHON,
        "rs" => &RUST,
        "ts" | "mts" | "cts" => &TYPESCRIPT,
        "tsx" => &TSX,
        "go" => &GO,
        "c" | "h" => &C,
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" => &CPP,
        "kt" | "kts" => &KOTLIN,
        "rb" => &COUNTED[index("ruby")?],
        "php" => &COUNTED[index("php")?],
        "cs" => &COUNTED[index("csharp")?],
        "swift" => &COUNTED[index("swift")?],
        "scala" => &COUNTED[index("scala")?],
        "sh" | "bash" | "zsh" | "ps1" => &COUNTED[index("shell")?],
        "sql" => &COUNTED[index("sql")?],
        "md" | "markdown" => &COUNTED[index("markdown")?],
        "yml" | "yaml" => &COUNTED[index("yaml")?],
        "json" => &COUNTED[index("json")?],
        "toml" => &COUNTED[index("toml")?],
        "xml" => &COUNTED[index("xml")?],
        "html" | "htm" => &COUNTED[index("html")?],
        "css" | "scss" | "less" => &COUNTED[index("css")?],
        "proto" => &COUNTED[index("protobuf")?],
        "tf" => &COUNTED[index("terraform")?],
        "gradle" | "groovy" => &COUNTED[index("groovy")?],
        _ => return None,
    })
}

/** What a symbol is
 * Variants
    - Function - a free function
    - Method - a function inside a type
    - Class - a class or object
    - Struct - a struct
    - Enum - an enum
    - Interface - an interface or trait
    - Type - a type alias or other named type
    - Variable - a module- or type-level variable
    - Data - a module- or type-level constant (const or static items, Go constants, Kotlin const
      val, C++ const, or an UPPER_SNAKE_CASE name)
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Type,
    Variable,
    Data,
}

impl SymbolKind {
    /** Every kind, in a fixed order */
    const ALL: [SymbolKind; 9] = [
        Self::Function,
        Self::Method,
        Self::Class,
        Self::Struct,
        Self::Enum,
        Self::Interface,
        Self::Type,
        Self::Variable,
        Self::Data,
    ];

    /** Return the kind's name used in the inventory
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Interface => "interface",
            Self::Type => "type",
            Self::Variable => "variable",
            Self::Data => "data",
        }
    }

    /** Parse a kind name written by name
     * Input
        - value: &str - kind name
     * Output
        - Option<SymbolKind>
    */
    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == value)
    }

    /** Return the policy item kind that would target a symbol of this kind
     * Input
        - None (uses self)
     * Output
        - Option<ItemKind>, None for kinds no policy flag targets
    */
    pub(crate) fn item_kind(self) -> Option<ItemKind> {
        match self {
            Self::Function | Self::Method => Some(ItemKind::Function),
            Self::Class | Self::Struct | Self::Enum => Some(ItemKind::Class),
            Self::Interface => Some(ItemKind::Interface),
            Self::Variable => Some(ItemKind::Variable),
            Self::Data => Some(ItemKind::Data),
            Self::Type => None,
        }
    }

    /** Check whether symbols of this kind run code (and so have calls)
     * Input
        - None (uses self)
     * Output
        - bool
    */
    pub(crate) fn callable(self) -> bool {
        matches!(self, Self::Function | Self::Method)
    }
}

/** One symbol found in a file, holding only what the file's content determines (so it can be
 * cached by content hash and reused after the file moves)
 * Fields
    - kind: SymbolKind - what the symbol is
    - name: String - its own name
    - qualified: String - enclosing type names and the name joined with ".", the form policy
      targets use
    - namespace: Option<String> - enclosing C++ namespace, joined with "."
    - start_line: usize - first line, 1-based
    - end_line: usize - last line, 1-based
    - calls: Vec<String> - names of functions it calls, sorted and unique (callables only)
    - loops: usize - loops inside it
    - branches: usize - branches inside it
    - test: bool - marked as a test by its language's convention (@Test, #[test], test_*, Test*)
    - targetable: bool - found by the resolver's own matcher in an enforceable language, so a
      policy rule could target it
    - digest: String - SHA-256 of the symbol's text (line endings normalized, trailing spaces
      trimmed), which changes whenever the symbol's code does
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Symbol {
    pub(crate) kind: SymbolKind,
    pub(crate) name: String,
    pub(crate) qualified: String,
    pub(crate) namespace: Option<String>,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) calls: Vec<String>,
    pub(crate) loops: usize,
    pub(crate) branches: usize,
    pub(crate) test: bool,
    pub(crate) targetable: bool,
    pub(crate) digest: String,
}

/** Everything discovery reads from one file's content
 * Fields
    - lines: usize - number of lines
    - status: String - "parsed", "partial" (parsed with syntax errors), "unsupported" (no
      grammar), or "skipped: REASON"
    - package: Option<String> - declared package (Java, Kotlin, Go)
    - imports: Vec<String> - imported modules, packages, or headers, in source order
    - calls: Vec<String> - every called name in the file, sorted and unique
    - symbols: Vec<Symbol> - symbols in source order
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outline {
    pub(crate) lines: usize,
    pub(crate) status: String,
    pub(crate) package: Option<String>,
    pub(crate) imports: Vec<String>,
    pub(crate) calls: Vec<String>,
    pub(crate) symbols: Vec<Symbol>,
}

impl Outline {
    /** Build an outline that holds no symbols
     * Input
        - lines: usize - number of lines
        - status: String - why there are no symbols
     * Output
        - Outline
    */
    fn empty(lines: usize, status: String) -> Self {
        Self {
            lines,
            status,
            package: None,
            imports: Vec::new(),
            calls: Vec::new(),
            symbols: Vec::new(),
        }
    }

    /** Serialize the outline for the inventory cache, with symbols as compact arrays
     * Input
        - None (uses self)
     * Output
        - Value JSON object
    */
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "lines": self.lines,
            "status": self.status,
            "package": self.package,
            "imports": self.imports,
            "calls": self.calls,
            "symbols": self.symbols.iter().map(|symbol| json!([
                symbol.kind.name(),
                symbol.name,
                symbol.qualified,
                symbol.namespace,
                symbol.start_line,
                symbol.end_line,
                symbol.calls,
                symbol.loops,
                symbol.branches,
                symbol.test,
                symbol.targetable,
                symbol.digest,
            ])).collect::<Vec<_>>(),
        })
    }

    /** Read an outline back from the inventory cache
     * Input
        - value: &Value - JSON written by to_json
     * Output
        - Option<Outline>, None if the entry is malformed (it is then extracted again)
    */
    pub(crate) fn from_json(value: &Value) -> Option<Self> {
        let strings = |value: &Value| -> Option<Vec<String>> {
            value
                .as_array()?
                .iter()
                .map(|item| item.as_str().map(String::from))
                .collect()
        };
        let symbols = value["symbols"]
            .as_array()?
            .iter()
            .map(|symbol| {
                Some(Symbol {
                    kind: SymbolKind::parse(symbol[0].as_str()?)?,
                    name: symbol[1].as_str()?.into(),
                    qualified: symbol[2].as_str()?.into(),
                    namespace: symbol[3].as_str().map(String::from),
                    start_line: symbol[4].as_u64()? as usize,
                    end_line: symbol[5].as_u64()? as usize,
                    calls: strings(&symbol[6])?,
                    loops: symbol[7].as_u64()? as usize,
                    branches: symbol[8].as_u64()? as usize,
                    test: symbol[9].as_bool()?,
                    targetable: symbol[10].as_bool()?,
                    digest: symbol[11].as_str()?.into(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            lines: value["lines"].as_u64()? as usize,
            status: value["status"].as_str()?.into(),
            package: value["package"].as_str().map(String::from),
            imports: strings(&value["imports"])?,
            calls: strings(&value["calls"])?,
            symbols,
        })
    }
}

/** Count the lines of a file
 * Input
    - bytes: &[u8] - file content
 * Output
    - usize, 0 for an empty file
*/
fn count_lines(bytes: &[u8]) -> usize {
    let breaks = bytes.iter().filter(|byte| **byte == b'\n').count();
    breaks + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"))
}

/** Extract a file's outline, by skipping oversized and binary files and files without a
 * grammar, then parsing with the shared parser cache (tolerating syntax errors, which only mark
 * the outline partial) and walking the tree once for symbols, imports, the package, and calls
 * Input
    - bytes: &[u8] - file content
    - path: &str - repository-relative path, used for the parser cache key
    - spec: &LanguageSpec - the file's language
 * Output
    - Outline
*/
pub(crate) fn outline(bytes: &[u8], path: &str, spec: &LanguageSpec) -> Outline {
    let lines = count_lines(bytes);
    if bytes.len() > MAX_PARSE_BYTES {
        return Outline::empty(lines, "skipped: larger than 1 MB".into());
    }
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return Outline::empty(lines, "skipped: binary".into());
    }
    let Some(grammar) = spec.grammar else {
        return Outline::empty(lines, "unsupported".into());
    };
    let source = String::from_utf8_lossy(bytes);
    let key = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or(spec.name)
        .to_ascii_lowercase();
    let tree = match parse_with(&source, &key, grammar()) {
        Ok(tree) => tree,
        Err(error) => return Outline::empty(lines, format!("skipped: {error}")),
    };
    let root = tree.root_node();
    let text = source.as_bytes();
    let mut outline = Outline::empty(
        lines,
        if root.has_error() {
            "partial".into()
        } else {
            "parsed".into()
        },
    );
    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if let Some(symbol) = symbol_at(node, text, spec.family) {
            symbols.push((node.start_byte(), symbol));
        }
        if let Some(import) = import_at(node, text, spec.family) {
            imports.push((node.start_byte(), import));
        }
        if outline.package.is_none() {
            outline.package = package_at(node, text, spec.family);
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    symbols.sort_by_key(|(start, _)| *start);
    imports.sort();
    outline.symbols = symbols.into_iter().map(|(_, symbol)| symbol).collect();
    outline.imports = imports.into_iter().map(|(_, import)| import).collect();
    outline.calls = calls_in(root, text, spec.family);
    outline
}

/** Syntax node kinds that hold their own code, so variables inside them are locals */
const CALLABLE_KINDS: &[&str] = &[
    "arrow_function",
    "function",
    "function_expression",
    "generator_function",
    "lambda",
    "lambda_expression",
    "lambda_literal",
    "closure_expression",
    "func_literal",
    "anonymous_function",
    "function_body",
    "compound_statement",
];

/** Extra loop kinds of the discovery-only grammars, added to the resolver's */
const EXTRA_LOOPS: &[&str] = &["do_while_statement", "for_range_loop"];

/** Extra branch kinds of the discovery-only grammars, added to the resolver's */
const EXTRA_BRANCHES: &[&str] = &[
    "expression_case",
    "type_case",
    "communication_case",
    "case_statement",
    "when_entry",
    "catch_block",
    "switch_statement",
];

/** Check whether a node sits inside a function, method, lambda, or block of code
 * Input
    - node: Node - syntax node
 * Output
    - bool
*/
fn inside_callable(node: Node) -> bool {
    let mut parent = node.parent();
    while let Some(ancestor) = parent {
        if DEFINITION_KINDS.contains(&ancestor.kind()) || CALLABLE_KINDS.contains(&ancestor.kind())
        {
            return true;
        }
        parent = ancestor.parent();
    }
    false
}

/** Check whether a name is written in UPPER_SNAKE_CASE, the usual spelling of constants
 * Input
    - name: &str - symbol name
 * Output
    - bool
*/
fn upper_snake(name: &str) -> bool {
    name.len() > 1
        && name.chars().any(|character| character.is_ascii_uppercase())
        && name.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        })
}

/** Return a node's text
 * Input
    - node: Node - syntax node
    - bytes: &[u8] - source bytes
 * Output
    - String
*/
fn text_of(node: Node, bytes: &[u8]) -> String {
    node.utf8_text(bytes).unwrap_or_default().to_string()
}

/** Return the first named child of a node with one of the given kinds
 * Input
    - node: Node - parent
    - kinds: &[&str] - accepted kinds
 * Output
    - Option<Node>
*/
fn child_of_kind<'tree>(node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|child| kinds.contains(&child.kind()));
    found
}

/** Return the first descendant (depth first, the node itself included) with one of the given
 * kinds, without entering subtrees of the stop kinds
 * Input
    - node: Node - subtree root
    - kinds: &[&str] - accepted kinds
    - stop: &[&str] - kinds not to descend into
 * Output
    - Option<Node>
*/
fn descendant_of_kind<'tree>(
    node: Node<'tree>,
    kinds: &[&str],
    stop: &[&str],
) -> Option<Node<'tree>> {
    if kinds.contains(&node.kind()) {
        return Some(node);
    }
    if stop.contains(&node.kind()) {
        return None;
    }
    let mut cursor = node.walk();
    let children = node.named_children(&mut cursor).collect::<Vec<_>>();
    children
        .into_iter()
        .find_map(|child| descendant_of_kind(child, kinds, stop))
}

/** Build a symbol from its node, measuring loops and branches and collecting calls for callables
 * Input
    - node: Node - the symbol's node (its whole definition)
    - bytes: &[u8] - source bytes
    - family: Family - language family, for call collection
    - kind: SymbolKind - what it is
    - qualified: Vec<String> - enclosing type names and the name
    - targetable: bool - found by the resolver's own matcher
 * Output
    - Symbol
*/
fn build(
    node: Node,
    bytes: &[u8],
    family: Family,
    kind: SymbolKind,
    qualified: Vec<String>,
    targetable: bool,
) -> Symbol {
    let (mut loops, mut branches) = (0, 0);
    if kind.callable() {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let current_kind = current.kind();
            if LOOP_KINDS.contains(&current_kind) || EXTRA_LOOPS.contains(&current_kind) {
                loops += 1;
            }
            if BRANCH_KINDS.contains(&current_kind) || EXTRA_BRANCHES.contains(&current_kind) {
                branches += 1;
            }
            let mut cursor = current.walk();
            stack.extend(current.children(&mut cursor));
        }
    }
    let name = qualified.last().cloned().unwrap_or_default();
    let body = text_of(node, bytes)
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    Symbol {
        digest: crate::util::sha256(body.as_bytes()),
        kind,
        test: is_test(node, bytes, family, kind, &name),
        name,
        qualified: qualified.join("."),
        namespace: namespace_of(node, bytes, family),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        calls: if kind.callable() {
            calls_in(node, bytes, family)
        } else {
            Vec::new()
        },
        loops,
        branches,
        targetable,
    }
}

/** Decide by language convention whether a callable is a test: a @Test annotation (Java,
 * Kotlin), a test attribute (Rust), a test_ name (Python), a Test/Benchmark/Fuzz name (Go), or a
 * TEST macro (C++ test frameworks)
 * Input
    - node: Node - the symbol's node
    - bytes: &[u8] - source bytes
    - family: Family - language family
    - kind: SymbolKind - what the symbol is
    - name: &str - its name
 * Output
    - bool
*/
fn is_test(node: Node, bytes: &[u8], family: Family, kind: SymbolKind, name: &str) -> bool {
    if !kind.callable() {
        return false;
    }
    let annotated = || {
        child_of_kind(node, &["modifiers"])
            .map(|modifiers| text_of(modifiers, bytes).contains("@Test"))
            .unwrap_or(false)
    };
    match family {
        Family::Shared => {
            if node.kind() == "function_item" {
                let mut sibling = node.prev_named_sibling();
                while let Some(attribute) = sibling.filter(|item| item.kind() == "attribute_item") {
                    if text_of(attribute, bytes).contains("test") {
                        return true;
                    }
                    sibling = attribute.prev_named_sibling();
                }
                false
            } else {
                annotated() || (node.kind() == "function_definition" && name.starts_with("test"))
            }
        }
        Family::Kotlin => annotated(),
        Family::Go => ["Test", "Benchmark", "Fuzz", "Example"]
            .iter()
            .any(|prefix| name.starts_with(prefix)),
        Family::Cpp => name.starts_with("TEST"),
        Family::TypeScript | Family::Text => false,
    }
}

/** Return the C++ namespaces enclosing a node, outermost first, joined with "."
 * Input
    - node: Node - syntax node
    - bytes: &[u8] - source bytes
    - family: Family - language family (only Cpp has namespaces here)
 * Output
    - Option<String>, None outside any namespace
*/
fn namespace_of(node: Node, bytes: &[u8], family: Family) -> Option<String> {
    if family != Family::Cpp {
        return None;
    }
    let mut names = Vec::new();
    let mut parent = node.parent();
    while let Some(ancestor) = parent {
        if ancestor.kind() == "namespace_definition" {
            if let Some(name) = ancestor.child_by_field_name("name") {
                names.push(text_of(name, bytes).replace("::", "."));
            }
        }
        parent = ancestor.parent();
    }
    names.reverse();
    (!names.is_empty()).then(|| names.join("."))
}

/** Read the symbol a node defines, if any, using the reader of the language's family
 * Input
    - node: Node - candidate node
    - bytes: &[u8] - source bytes
    - family: Family - language family
 * Output
    - Option<Symbol>
*/
fn symbol_at(node: Node, bytes: &[u8], family: Family) -> Option<Symbol> {
    match family {
        Family::Shared | Family::TypeScript => shared_symbol(node, bytes, family),
        Family::Go => go_symbol(node, bytes),
        Family::Cpp => cpp_symbol(node, bytes),
        Family::Kotlin => kotlin_symbol(node, bytes),
        Family::Text => None,
    }
}

/** Read a symbol with the resolver's own matcher and qualifier (so its qualified name is what a
 * policy target resolves), classifying Rust structs and enums, methods, local variables (skipped),
 * function-valued variables (functions), and constants; TypeScript adds abstract classes, enums,
 * and type aliases, and Java adds enums and records
 * Input
    - node: Node - candidate node
    - bytes: &[u8] - source bytes
    - family: Family - Shared or TypeScript
 * Output
    - Option<Symbol>
*/
fn shared_symbol(node: Node, bytes: &[u8], family: Family) -> Option<Symbol> {
    for kind in [
        ItemKind::Function,
        ItemKind::Class,
        ItemKind::Interface,
        ItemKind::Variable,
    ] {
        let Some((name, _)) = match_item(node, bytes, kind) else {
            continue;
        };
        let mut qualified = enclosing_types(node, bytes);
        qualified.push(name.clone());
        let nested = qualified.len() > 1;
        let symbol_kind = match kind {
            ItemKind::Function if nested => SymbolKind::Method,
            ItemKind::Function => SymbolKind::Function,
            ItemKind::Class => match node.kind() {
                "struct_item" => SymbolKind::Struct,
                "enum_item" => SymbolKind::Enum,
                _ => SymbolKind::Class,
            },
            ItemKind::Interface => SymbolKind::Interface,
            _ => {
                if inside_callable(node) {
                    return None;
                }
                let value = node
                    .child_by_field_name("value")
                    .or_else(|| node.child_by_field_name("right"));
                match value.map(|value| value.kind()) {
                    Some(
                        "arrow_function"
                        | "function"
                        | "function_expression"
                        | "generator_function"
                        | "lambda",
                    ) if nested => SymbolKind::Method,
                    Some(
                        "arrow_function"
                        | "function"
                        | "function_expression"
                        | "generator_function"
                        | "lambda",
                    ) => SymbolKind::Function,
                    _ if matches!(node.kind(), "const_item" | "static_item")
                        || upper_snake(&name) =>
                    {
                        SymbolKind::Data
                    }
                    _ => SymbolKind::Variable,
                }
            }
        };
        let enforceable = family == Family::Shared;
        return Some(build(
            node,
            bytes,
            family,
            symbol_kind,
            qualified,
            enforceable,
        ));
    }
    let symbol_kind = match node.kind() {
        "abstract_class_declaration" | "record_declaration" => SymbolKind::Class,
        "enum_declaration" => SymbolKind::Enum,
        "type_alias_declaration" => SymbolKind::Type,
        _ => return None,
    };
    let name = node.child_by_field_name("name")?;
    let mut qualified = enclosing_types(node, bytes);
    qualified.push(text_of(name, bytes));
    Some(build(node, bytes, family, symbol_kind, qualified, false))
}

/** Read a Go symbol: functions, methods (qualified by receiver type), struct, interface, and
 * other type declarations, and package-level variables and constants
 * Input
    - node: Node - candidate node
    - bytes: &[u8] - source bytes
 * Output
    - Option<Symbol>
*/
fn go_symbol(node: Node, bytes: &[u8]) -> Option<Symbol> {
    let name = || {
        node.child_by_field_name("name")
            .map(|name| text_of(name, bytes))
    };
    let (kind, qualified) = match node.kind() {
        "function_declaration" => (SymbolKind::Function, vec![name()?]),
        "method_declaration" => {
            let receiver = node
                .child_by_field_name("receiver")
                .and_then(|receiver| descendant_of_kind(receiver, &["type_identifier"], &[]))
                .map(|receiver| text_of(receiver, bytes));
            match receiver {
                Some(receiver) => (SymbolKind::Method, vec![receiver, name()?]),
                None => (SymbolKind::Function, vec![name()?]),
            }
        }
        "type_spec" => {
            let kind = match node.child_by_field_name("type").map(|value| value.kind()) {
                Some("struct_type") => SymbolKind::Struct,
                Some("interface_type") => SymbolKind::Interface,
                _ => SymbolKind::Type,
            };
            (kind, vec![name()?])
        }
        "var_spec" | "const_spec" if !inside_callable(node) => {
            let name = name()?;
            let kind = if node.kind() == "const_spec" || upper_snake(&name) {
                SymbolKind::Data
            } else {
                SymbolKind::Variable
            };
            (kind, vec![name])
        }
        _ => return None,
    };
    Some(build(node, bytes, Family::Go, kind, qualified, false))
}

/** Read a C or C++ symbol: function definitions (qualified by enclosing classes and by the
 * Class:: scope of out-of-line definitions), class, struct, union, and enum definitions with a
 * body, and namespace-level variables (prototypes and forward declarations are skipped)
 * Input
    - node: Node - candidate node
    - bytes: &[u8] - source bytes
 * Output
    - Option<Symbol>
*/
fn cpp_symbol(node: Node, bytes: &[u8]) -> Option<Symbol> {
    let mut qualified = cpp_enclosing(node, bytes);
    let kind = match node.kind() {
        "function_definition" => {
            let declarator = descendant_of_kind(
                node,
                &["function_declarator"],
                &["compound_statement", "field_declaration_list"],
            )?;
            let target = declarator.child_by_field_name("declarator")?;
            match target.kind() {
                "qualified_identifier" => {
                    let full = text_of(target, bytes);
                    qualified.extend(
                        full.split("::")
                            .map(|part| part.trim().to_string())
                            .filter(|part| !part.is_empty()),
                    );
                }
                _ => qualified.push(text_of(target, bytes)),
            }
            if qualified.len() > 1 {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            }
        }
        "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier" => {
            node.child_by_field_name("body")?;
            qualified.push(text_of(node.child_by_field_name("name")?, bytes));
            match node.kind() {
                "class_specifier" => SymbolKind::Class,
                "enum_specifier" => SymbolKind::Enum,
                _ => SymbolKind::Struct,
            }
        }
        "declaration" => {
            let parent = node.parent()?.kind();
            if !matches!(parent, "translation_unit" | "declaration_list") {
                return None;
            }
            let declarator = node.child_by_field_name("declarator")?;
            let identifier = match declarator.kind() {
                "init_declarator" => declarator.child_by_field_name("declarator")?,
                "identifier" => declarator,
                _ => return None,
            };
            if identifier.kind() != "identifier" {
                return None;
            }
            let name = text_of(identifier, bytes);
            let declared = text_of(node, bytes);
            let constant = declared.starts_with("const ")
                || declared.starts_with("constexpr ")
                || declared.contains(" const ")
                || upper_snake(&name);
            qualified.push(name);
            if constant {
                SymbolKind::Data
            } else {
                SymbolKind::Variable
            }
        }
        _ => return None,
    };
    Some(build(node, bytes, Family::Cpp, kind, qualified, false))
}

/** List the classes and structs enclosing a C++ node, outermost first
 * Input
    - node: Node - syntax node
    - bytes: &[u8] - source bytes
 * Output
    - Vec<String>
*/
fn cpp_enclosing(node: Node, bytes: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut parent = node.parent();
    while let Some(ancestor) = parent {
        if matches!(ancestor.kind(), "class_specifier" | "struct_specifier") {
            if let Some(name) = ancestor.child_by_field_name("name") {
                names.push(text_of(name, bytes));
            }
        }
        parent = ancestor.parent();
    }
    names.reverse();
    names
}

/** Read a Kotlin symbol: classes, interfaces, enum classes, objects, functions and methods, and
 * top-level or member properties
 * Input
    - node: Node - candidate node
    - bytes: &[u8] - source bytes
 * Output
    - Option<Symbol>
*/
fn kotlin_symbol(node: Node, bytes: &[u8]) -> Option<Symbol> {
    let mut qualified = kotlin_enclosing(node, bytes);
    let modifiers = child_of_kind(node, &["modifiers"])
        .map(|modifiers| text_of(modifiers, bytes))
        .unwrap_or_default();
    let kind = match node.kind() {
        "class_declaration" | "object_declaration" => {
            qualified.push(text_of(child_of_kind(node, &["type_identifier"])?, bytes));
            let mut cursor = node.walk();
            let kinds = node
                .children(&mut cursor)
                .map(|child| child.kind())
                .collect::<Vec<_>>();
            if kinds.contains(&"interface") {
                SymbolKind::Interface
            } else if kinds.contains(&"enum")
                || kinds.contains(&"enum_class_body")
                || modifiers.split_whitespace().any(|word| word == "enum")
            {
                SymbolKind::Enum
            } else {
                SymbolKind::Class
            }
        }
        "function_declaration" => {
            qualified.push(text_of(child_of_kind(node, &["simple_identifier"])?, bytes));
            if qualified.len() > 1 {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            }
        }
        "property_declaration" if !inside_callable(node) => {
            let variable = child_of_kind(node, &["variable_declaration"])?;
            let name = text_of(child_of_kind(variable, &["simple_identifier"])?, bytes);
            let constant =
                modifiers.split_whitespace().any(|word| word == "const") || upper_snake(&name);
            qualified.push(name);
            if constant {
                SymbolKind::Data
            } else {
                SymbolKind::Variable
            }
        }
        _ => return None,
    };
    Some(build(node, bytes, Family::Kotlin, kind, qualified, false))
}

/** List the classes and objects enclosing a Kotlin node, outermost first
 * Input
    - node: Node - syntax node
    - bytes: &[u8] - source bytes
 * Output
    - Vec<String>
*/
fn kotlin_enclosing(node: Node, bytes: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut parent = node.parent();
    while let Some(ancestor) = parent {
        if matches!(ancestor.kind(), "class_declaration" | "object_declaration") {
            if let Some(name) = child_of_kind(ancestor, &["type_identifier"]) {
                names.push(text_of(name, bytes));
            }
        }
        parent = ancestor.parent();
    }
    names.reverse();
    names
}

/** Read the import a node declares, if any
 * Input
    - node: Node - candidate node
    - bytes: &[u8] - source bytes
    - family: Family - language family
 * Output
    - Option<String> the imported module, package, or header
*/
fn import_at(node: Node, bytes: &[u8], family: Family) -> Option<String> {
    let clean = |text: String| {
        text.trim()
            .trim_start_matches("import")
            .trim_start_matches(" static")
            .trim_end_matches(';')
            .trim()
            .trim_matches(|character| matches!(character, '"' | '\'' | '<' | '>' | '`'))
            .to_string()
    };
    let text = match (family, node.kind()) {
        (Family::Shared, "import_declaration") => clean(text_of(node, bytes)),
        (Family::Shared, "import_statement") => {
            let mut cursor = node.walk();
            let names = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "dotted_name" || child.kind() == "aliased_import")
                .map(|child| text_of(child, bytes))
                .collect::<Vec<_>>();
            match node.child_by_field_name("source") {
                Some(source) => clean(text_of(source, bytes)),
                None => names.join(", "),
            }
        }
        (Family::Shared, "import_from_statement") => {
            text_of(node.child_by_field_name("module_name")?, bytes)
        }
        (Family::Shared, "use_declaration") => {
            text_of(node.child_by_field_name("argument")?, bytes)
        }
        (Family::TypeScript, "import_statement") => {
            clean(text_of(node.child_by_field_name("source")?, bytes))
        }
        (Family::Go, "import_spec") => clean(text_of(node.child_by_field_name("path")?, bytes)),
        (Family::Cpp, "preproc_include") => {
            clean(text_of(node.child_by_field_name("path")?, bytes))
        }
        (Family::Kotlin, "import_header") => text_of(child_of_kind(node, &["identifier"])?, bytes),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/** Read the package a node declares, if any (Java, Kotlin, Go)
 * Input
    - node: Node - candidate node
    - bytes: &[u8] - source bytes
    - family: Family - language family
 * Output
    - Option<String>
*/
fn package_at(node: Node, bytes: &[u8], family: Family) -> Option<String> {
    let name = match (family, node.kind()) {
        (Family::Shared, "package_declaration") => {
            child_of_kind(node, &["scoped_identifier", "identifier"])?
        }
        (Family::Kotlin, "package_header") => child_of_kind(node, &["identifier"])?,
        (Family::Go, "package_clause") => child_of_kind(node, &["package_identifier"])?,
        _ => return None,
    };
    let text = text_of(name, bytes)
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

/** Collect the names called inside a node, sorted and unique: the resolver's own call reader for
 * the languages it supports (and TypeScript, which shares JavaScript's call syntax), and the
 * language's call syntax for Go, C++, and Kotlin
 * Input
    - node: Node - subtree root
    - bytes: &[u8] - source bytes
    - family: Family - language family
 * Output
    - Vec<String>
*/
fn calls_in(node: Node, bytes: &[u8], family: Family) -> Vec<String> {
    let mut calls = Vec::new();
    match family {
        Family::Shared | Family::TypeScript => collect_calls(node, bytes, &mut calls),
        Family::Text => {}
        _ => {
            let mut stack = vec![node];
            while let Some(current) = stack.pop() {
                if let Some(name) = callee(current, bytes, family) {
                    calls.push(name);
                }
                let mut cursor = current.walk();
                stack.extend(current.children(&mut cursor));
            }
        }
    }
    calls.retain(|name| !name.is_empty());
    calls.sort();
    calls.dedup();
    calls
}

/** Read the called name of a Go, C++, or Kotlin call node: the last segment of a selector,
 * field, qualified, navigation, or template callee
 * Input
    - node: Node - candidate node
    - bytes: &[u8] - source bytes
    - family: Family - Go, Cpp, or Kotlin
 * Output
    - Option<String>
*/
fn callee(node: Node, bytes: &[u8], family: Family) -> Option<String> {
    if node.kind() != "call_expression" {
        return None;
    }
    let mut function = match family {
        Family::Kotlin => node.named_child(0)?,
        _ => node.child_by_field_name("function")?,
    };
    loop {
        function = match function.kind() {
            "identifier" | "simple_identifier" | "field_identifier" => {
                return Some(text_of(function, bytes))
            }
            "selector_expression" => function.child_by_field_name("field")?,
            "field_expression" => function.child_by_field_name("field")?,
            "template_function" => function.child_by_field_name("name")?,
            "qualified_identifier" => {
                return text_of(function, bytes)
                    .rsplit("::")
                    .next()
                    .map(|name| name.trim().to_string())
            }
            "navigation_expression" => {
                let suffix = function.named_child(function.named_child_count().checked_sub(1)?)?;
                child_of_kind(suffix, &["simple_identifier"])?
            }
            _ => return None,
        };
    }
}
