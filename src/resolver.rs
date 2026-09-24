use std::env;
use std::fs;
use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::model::SourceTarget;
use crate::repository::{ensure_commit, git};
use crate::util::io_error;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolution {
    Found(SourceTarget),
    Missing,
    Duplicate(usize),
    Unsupported,
    ParseFailure(String),
}

pub(crate) fn language(path: &str) -> Option<tree_sitter::Language> {
    // Select a parser from the source extension and fail closed for unknown languages
    match Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase()
        .as_str()
    {
        "java" => Some(tree_sitter_java::language()),
        "js" | "jsx" | "mjs" | "cjs" => Some(tree_sitter_javascript::language()),
        "py" => Some(tree_sitter_python::language()),
        "rs" => Some(tree_sitter_rust::language()),
        _ => None,
    }
}

pub(crate) fn supported_extensions() -> &'static str {
    // Keep supported-language diagnostics aligned with parser selection
    ".java, .js, .jsx, .mjs, .cjs, .py, .rs"
}

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

    // Walk the syntax tree and collect every exact qualified target match
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
                    let mut parent = node.parent();
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
                        output.push(canonical_source_node(node, bytes));
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit(child, wanted, target, separator, bytes, output);
        }
    }

    // Ignore formatting and comments while retaining token order and spelling
    fn canonical_source_node(node: Node, bytes: &[u8]) -> String {
        // Leaves are separated with a sentinel so adjacent tokens stay distinct
        fn append_tokens(node: Node, bytes: &[u8], output: &mut String) {
            if node.kind().contains("comment") {
                return;
            }
            let mut cursor = node.walk();
            let mut has_named_child = false;
            for child in node.children(&mut cursor) {
                has_named_child = true;
                append_tokens(child, bytes, output);
            }
            if !has_named_child {
                output.push_str(&String::from_utf8_lossy(&bytes[node.byte_range()]));
                output.push('\0');
            }
        }

        let mut canonical = String::new();
        append_tokens(node, bytes, &mut canonical);
        canonical
    }

    let mut output = Vec::new();
    visit(
        tree.root_node(),
        wanted,
        target,
        separator,
        bytes,
        &mut output,
    );
    Ok(output)
}

fn supported(path: &str) -> bool {
    // Identify files that Crane can parse for target resolution
    language(path).is_some()
}

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
