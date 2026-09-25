use std::env;
use std::fs;
use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::model::SourceTarget;
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

/** Find every definition matching a target in one source file, by first parsing the source with the
 * grammar for its extension and rejecting trees with parse errors, then splitting the target into
 * its final function name and separator, and finally walking the syntax tree to collect each
 * matching definition as a canonical token string
 * Input
    - source: &str - file contents
    - path: &str - file path, used to pick the language
    - target: &str - qualified target such as Type.method or Type::method
 * Output
    - Result<Vec<String>, String> with one canonical snippet per match
    - Error if the language is unsupported or the source does not parse
*/
pub(crate) fn extract_functions(
    source: &str,
    path: &str,
    target: &str,
) -> Result<Vec<String>, String> {
    // Resolve target nodes and reduce each node to canonical non-comment tokens
    let language = language(path).ok_or_else(|| "unsupported source language".to_string())?;
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .map_err(|error| error.to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "parser returned no syntax tree".to_string())?;
    if tree.root_node().has_error() {
        return Err("source contains a parser error".into());
    }
    let wanted = target
        .rsplit("::")
        .next()
        .unwrap_or(target)
        .rsplit('.')
        .next()
        .ok_or_else(|| "target has no function name".to_string())?;
    let separator = if target.contains("::") { "::" } else { "." };
    let bytes = source.as_bytes();

    /** Walk the syntax tree and collect every exact qualified target match, by checking whether the
     * node is a function, method, or declaration named like the target, then climbing its ancestors
     * to build the class/struct/impl qualified name, and finally recursing into every child node
     * Input
        - node: Node - current syntax node
        - wanted: &str - final function name to match
        - target: &str - full qualified target
        - separator: &str - "." or "::" used by the target
        - bytes: &[u8] - source bytes for reading node text
        - output: &mut Vec<String> - collected canonical snippets
     * Output
        - None (appends matches to output)
    */
    fn visit(
        node: Node,
        wanted: &str,
        target: &str,
        separator: &str,
        bytes: &[u8],
        output: &mut Vec<String>,
    ) {
        let kind = node.kind();
        if kind.contains("function")
            || kind.contains("method")
            || kind.contains("declaration")
            || kind == "function_item"
        {
            if let Some(name) = node.child_by_field_name("name") {
                if name.utf8_text(bytes).ok() == Some(wanted) {
                    let mut qualified = Vec::new();
                    let mut parent = node.parent(); // if found, check the parent for the correct declaration
                    while let Some(ancestor) = parent {
                        let named = ancestor
                            .child_by_field_name("name")
                            .or_else(|| ancestor.child_by_field_name("type"));
                        if let Some(name) = named {
                            if ancestor.kind().contains("class")
                                || ancestor.kind().contains("struct")
                                || ancestor.kind().contains("impl")
                            {
                                qualified
                                    .push(name.utf8_text(bytes).unwrap_or_default().to_string());
                            }
                        }
                        parent = ancestor.parent();
                    }
                    qualified.reverse();
                    qualified.push(wanted.into());
                    if target.split(separator).count() == 1 || qualified.join(separator) == target {
                        output.push(canonical_source_node(node, bytes)); // functional cal for canonical code
                    }
                }
            }
        }
        let mut cursor = node.walk(); // creating a pointer
        for child in node.children(&mut cursor) {
            // If there are children of this node, base case termination if no children
            visit(child, wanted, target, separator, bytes, output);
        }
    }

    /** Reduce a definition to a canonical token string that ignores formatting and comments, by
     * collecting its leaf tokens in source order through append_tokens
     * Input
        - node: Node - matched definition node
        - bytes: &[u8] - source bytes
     * Output
        - String of tokens separated by \0
    */
    fn canonical_source_node(node: Node, bytes: &[u8]) -> String {
        /** Append a node's tokens to the output, by skipping comment nodes, recursing into
         * children, and writing each leaf's text followed by a \0 sentinel so adjacent tokens stay
         * distinct
         * Input
            - node: Node - current syntax node
            - bytes: &[u8] - source bytes
            - output: &mut String - canonical token buffer
         * Output
            - None (appends to output)
        */
        fn append_tokens(node: Node, bytes: &[u8], output: &mut String) {
            if node.kind().contains("comment") {
                // comments are skipped
                return;
            }
            let mut cursor = node.walk(); // creating a new pointer
            let mut has_named_child = false;
            for child in node.children(&mut cursor) {
                has_named_child = true;
                append_tokens(child, bytes, output); // append the data (text)
            }
            if !has_named_child {
                output.push_str(&String::from_utf8_lossy(&bytes[node.byte_range()]));
                output.push('\0'); // delimiter for token concatenation x + 1, becomes x\0+\0+1\0
            }
        }

        let mut canonical = String::new();
        append_tokens(node, bytes, &mut canonical);
        canonical
    }

    let mut output = Vec::new();
    visit(
        // Recursively called in the function
        tree.root_node(),
        wanted,
        target,
        separator,
        bytes,
        &mut output,
    );
    Ok(output)
}

/** Check whether Crane can parse a file, by asking language for a grammar for its path
 * Input
    - path: &str - file path
 * Output
    - bool, true if a grammar exists
*/
fn supported(path: &str) -> bool {
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
    - target: &str - qualified function target
 * Output
    - Result<Resolution, String>
    - ParseFailure resolution if a supported file does not parse
    - Error if a directory or file cannot be read
*/
pub(crate) fn resolve_worktree(target: &str) -> Result<Resolution, String> {
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
                match extract_functions(&source, &path.to_string_lossy(), target) {
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
    - target: &str - qualified function target
 * Output
    - Result<Resolution, String>
    - ParseFailure resolution if a supported file does not parse
    - Error if the commit is missing or a Git command fails
*/
pub(crate) fn resolve_git(commit: &str, target: &str) -> Result<Resolution, String> {
    // Resolve the same target from an immutable Git commit for baseline comparison
    ensure_commit(commit)?;
    let files = git(&["ls-tree", "-r", "--name-only", commit])?;
    let mut unsupported = false;
    let mut supported_seen = false;
    let mut matches = Vec::new();
    for path in files.lines().filter(|path| supported(path)) {
        supported_seen = true;
        let source = git(&["show", &format!("{commit}:{path}")])?;
        match extract_functions(&source, path, target) {
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
