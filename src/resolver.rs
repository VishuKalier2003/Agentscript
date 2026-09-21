use std::env;
use std::fs;
use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::model::SourceTarget;
use crate::repository::{ensure_commit, git};
use crate::util::io_error;

pub(crate) fn language(path: &str) -> Option<tree_sitter::Language> {
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

pub(crate) fn extract_function(source: &str, path: &str, target: &str) -> Option<String> {
    let language = language(path)?;
    let mut parser = Parser::new();
    parser.set_language(language).ok()?;
    let tree = parser.parse(source, None)?;
    let wanted = target
        .rsplit("::")
        .next()
        .unwrap_or(target)
        .rsplit('.')
        .next()?;
    let separator = if target.contains("::") { "::" } else { "." };
    let bytes = source.as_bytes();

    fn visit(
        node: Node,
        wanted: &str,
        target: &str,
        separator: &str,
        bytes: &[u8],
        output: &mut Option<String>,
    ) {
        if output.is_some() {
            return;
        }
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
                        *output = Some(
                            String::from_utf8_lossy(&bytes[node.byte_range()])
                                .replace("\r\n", "\n")
                                .replace('\r', "\n"),
                        );
                        return;
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit(child, wanted, target, separator, bytes, output);
        }
    }

    let mut output = None;
    visit(
        tree.root_node(),
        wanted,
        target,
        separator,
        bytes,
        &mut output,
    );
    output
}

fn supported(path: &str) -> bool {
    language(path).is_some()
}

pub(crate) fn resolve_worktree(target: &str) -> Result<Option<SourceTarget>, String> {
    let mut stack = vec![env::current_dir().map_err(io_error)?];
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
                let source = fs::read_to_string(&path).map_err(io_error)?;
                if let Some(snippet) = extract_function(&source, &path.to_string_lossy(), target) {
                    return Ok(Some(SourceTarget { snippet }));
                }
            }
        }
    }
    Ok(None)
}

pub(crate) fn resolve_git(commit: &str, target: &str) -> Result<Option<SourceTarget>, String> {
    ensure_commit(commit)?;
    let files = git(&["ls-tree", "-r", "--name-only", commit])?;
    for path in files.lines().filter(|path| supported(path)) {
        let source = git(&["show", &format!("{commit}:{path}")])?;
        if let Some(snippet) = extract_function(&source, path, target) {
            return Ok(Some(SourceTarget { snippet }));
        }
    }
    Ok(None)
}
