// Language-independent change classification for target commands. Selected content is lexed into
// identifiers, literals, operators, and punctuation (comments split off using the file's comment
// adapter), and the before and after versions are compared on four deterministic features:
// whitespace, comments, business tokens (literals, operators, called names), complexity (loop and
// branch keywords), and shape (the token stream with identifiers numbered by first appearance, so
// a consistent rename keeps the shape). Crane measures; it does not judge intent.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::selection::anchors::CommentSyntax;

/** The kind of change a target command requires
 * Variants
    - LogicalBn - business logic: literals, operators, or called names changed
    - LogicalCn - complexity: the number of loops or branches changed
    - LogicalSn - structure: code was restructured while business tokens and complexity stayed
    - Semantic - wording: names or comments changed while the code's shape and business tokens
      stayed the same
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChangeType {
    LogicalBn,
    LogicalCn,
    LogicalSn,
    Semantic,
}

impl ChangeType {
    /** Parse a change type keyword
     * Input
        - value: &str - keyword
     * Output
        - Result<ChangeType, String>
        - Error listing the valid keywords
    */
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().replace('-', "_").as_str() {
            "logical_bn" => Ok(Self::LogicalBn),
            "logical_cn" => Ok(Self::LogicalCn),
            "logical_sn" => Ok(Self::LogicalSn),
            "semantic" => Ok(Self::Semantic),
            _ => Err(format!(
                "invalid change_type '{value}'; expected logical_bn, logical_cn, logical_sn, or semantic"
            )),
        }
    }

    /** Return the keyword of the change type
     * Input
        - None (uses self)
     * Output
        - &'static str
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::LogicalBn => "logical_bn",
            Self::LogicalCn => "logical_cn",
            Self::LogicalSn => "logical_sn",
            Self::Semantic => "semantic",
        }
    }
}

/** A lexical token
 * Variants
    - Identifier(String) - a name or keyword
    - Literal(String) - a number or string literal, kept whole
    - Operator(String) - an operator
    - Punctuation(char) - brackets, commas, semicolons
*/
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Identifier(String),
    Literal(String),
    Operator(String),
    Punctuation(char),
}

/** Words that are keywords in common languages; they stay literal in the shape */
const KEYWORDS: &[&str] = &[
    "if",
    "else",
    "elif",
    "for",
    "while",
    "loop",
    "do",
    "switch",
    "case",
    "match",
    "default",
    "break",
    "continue",
    "return",
    "fn",
    "def",
    "function",
    "func",
    "class",
    "struct",
    "enum",
    "impl",
    "trait",
    "interface",
    "let",
    "const",
    "var",
    "val",
    "mut",
    "pub",
    "static",
    "public",
    "private",
    "protected",
    "try",
    "catch",
    "except",
    "finally",
    "throw",
    "throws",
    "raise",
    "new",
    "delete",
    "in",
    "of",
    "is",
    "not",
    "and",
    "or",
    "import",
    "from",
    "as",
    "package",
    "use",
    "mod",
    "self",
    "this",
    "super",
    "async",
    "await",
    "yield",
    "lambda",
    "with",
    "pass",
    "true",
    "false",
    "null",
    "None",
    "True",
    "False",
    "nil",
    "undefined",
    "void",
    "foreach",
    "when",
    "unless",
    "guard",
    "then",
    "end",
    "begin",
    "go",
    "defer",
    "select",
    "where",
];

/** Keywords counted as loops */
const LOOPS: &[&str] = &["for", "while", "loop", "do", "foreach", "until"];

/** Keywords counted as branches */
const BRANCHES: &[&str] = &[
    "if", "elif", "switch", "case", "match", "catch", "except", "when", "unless", "guard", "select",
];

/** Literal keywords counted as business values */
const LITERAL_WORDS: &[&str] = &[
    "true",
    "false",
    "null",
    "None",
    "True",
    "False",
    "nil",
    "undefined",
];

/** Multi-character operators, longest first */
const OPERATORS: &[&str] = &[
    ">>>=", "===", "!==", "**=", "//=", "<<=", ">>=", ">>>", "...", "==", "!=", "<=", ">=", "&&",
    "||", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "**", "//", "<<", ">>", "->",
    "=>", "::", "??", "?.", ":=",
];

/** Lexed content: code tokens and the text of its comments
 * Fields
    - tokens: Vec<Token> - code tokens in order
    - comments: Vec<String> - comment texts with whitespace collapsed
*/
struct Lexed {
    tokens: Vec<Token>,
    comments: Vec<String>,
}

/** Collapse runs of whitespace to single spaces and trim
 * Input
    - text: &str - text
 * Output
    - String
*/
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/** Lex content using the comment syntax of its file: line comments start at the adapter's opener
 * (and block comments of C-family languages are recognized too); strings in double quotes, single
 * quotes (unless the quote opens comments), backquotes, and triple quotes are kept whole
 * Input
    - text: &str - selected content
    - syntax: CommentSyntax - comment syntax of the file
 * Output
    - Lexed
*/
fn lex(text: &str, syntax: CommentSyntax) -> Lexed {
    let chars = text.chars().collect::<Vec<_>>();
    let starts_with = |index: usize, pattern: &str| {
        pattern
            .chars()
            .enumerate()
            .all(|(offset, character)| chars.get(index + offset) == Some(&character))
    };
    let (line_opener, block) = match syntax {
        CommentSyntax::Line("//") => (Some("//"), Some(("/*", "*/"))),
        CommentSyntax::Line(opener) => (Some(opener), None),
        CommentSyntax::Block(open, close) => (None, Some((open, close))),
    };
    let mut lexed = Lexed {
        tokens: Vec::new(),
        comments: Vec::new(),
    };
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character.is_whitespace() {
            index += 1;
        } else if let Some((open, close)) = block.filter(|(open, _)| starts_with(index, open)) {
            let start = index + open.chars().count();
            let mut end = start;
            while end < chars.len() && !starts_with(end, close) {
                end += 1;
            }
            lexed.comments.push(collapse(
                &chars[start..end.min(chars.len())]
                    .iter()
                    .collect::<String>(),
            ));
            index = (end + close.chars().count()).min(chars.len());
        } else if let Some(opener) = line_opener.filter(|opener| starts_with(index, opener)) {
            let start = index + opener.chars().count();
            let mut end = start;
            while end < chars.len() && chars[end] != '\n' {
                end += 1;
            }
            lexed
                .comments
                .push(collapse(&chars[start..end].iter().collect::<String>()));
            index = end;
        } else if character == '"'
            || character == '`'
            || (character == '\'' && line_opener != Some("'"))
        {
            let triple = starts_with(index, &character.to_string().repeat(3));
            let quote = if triple {
                character.to_string().repeat(3)
            } else {
                character.to_string()
            };
            let mut end = index + quote.len();
            while end < chars.len() && !starts_with(end, &quote) {
                end += if chars[end] == '\\' { 2 } else { 1 };
            }
            end = (end + quote.len()).min(chars.len());
            lexed
                .tokens
                .push(Token::Literal(chars[index..end].iter().collect()));
            index = end;
        } else if character.is_ascii_digit() {
            let start = index;
            while index < chars.len()
                && (chars[index].is_alphanumeric() || chars[index] == '.' || chars[index] == '_')
            {
                index += 1;
            }
            lexed
                .tokens
                .push(Token::Literal(chars[start..index].iter().collect()));
        } else if character.is_alphabetic() || character == '_' || character == '$' {
            let start = index;
            while index < chars.len()
                && (chars[index].is_alphanumeric() || chars[index] == '_' || chars[index] == '$')
            {
                index += 1;
            }
            lexed
                .tokens
                .push(Token::Identifier(chars[start..index].iter().collect()));
        } else if "()[]{},;".contains(character) {
            lexed.tokens.push(Token::Punctuation(character));
            index += 1;
        } else if let Some(operator) = OPERATORS
            .iter()
            .find(|operator| starts_with(index, operator))
        {
            lexed.tokens.push(Token::Operator(operator.to_string()));
            index += operator.chars().count();
        } else {
            lexed.tokens.push(Token::Operator(character.to_string()));
            index += 1;
        }
    }
    lexed
}

/** Measurements of lexed content
 * Fields
    - business: BTreeMap<String, usize> - multiset of literals, operators, and called names
    - loops: usize - loop keywords
    - branches: usize - branch keywords and ternary operators
    - shape: Vec<String> - token stream with identifiers numbered by first appearance
    - names: Vec<String> - non-keyword identifiers in order
*/
#[derive(Debug, Default, PartialEq, Eq)]
struct Features {
    business: BTreeMap<String, usize>,
    loops: usize,
    branches: usize,
    shape: Vec<String>,
    names: Vec<String>,
}

/** Measure lexed content
 * Input
    - lexed: &Lexed - tokens
 * Output
    - Features
*/
fn features(lexed: &Lexed) -> Features {
    let mut result = Features::default();
    let mut numbers: BTreeMap<String, usize> = BTreeMap::new();
    let structural = |operator: &str| matches!(operator, "." | "::" | "->" | ":" | "?.");
    for (index, token) in lexed.tokens.iter().enumerate() {
        match token {
            Token::Literal(text) => {
                *result
                    .business
                    .entry(format!("literal {text}"))
                    .or_default() += 1;
                result.shape.push(text.clone());
            }
            Token::Operator(text) => {
                if !structural(text) {
                    *result
                        .business
                        .entry(format!("operator {text}"))
                        .or_default() += 1;
                }
                if text == "?" {
                    result.branches += 1;
                }
                result.shape.push(text.clone());
            }
            Token::Punctuation(character) => result.shape.push(character.to_string()),
            Token::Identifier(name) => {
                if LITERAL_WORDS.contains(&name.as_str()) {
                    *result
                        .business
                        .entry(format!("literal {name}"))
                        .or_default() += 1;
                }
                if LOOPS.contains(&name.as_str()) {
                    result.loops += 1;
                }
                if BRANCHES.contains(&name.as_str()) {
                    result.branches += 1;
                }
                if KEYWORDS.contains(&name.as_str()) {
                    result.shape.push(name.clone());
                    continue;
                }
                if lexed.tokens.get(index + 1) == Some(&Token::Punctuation('(')) {
                    *result.business.entry(format!("call {name}")).or_default() += 1;
                }
                let next = numbers.len();
                let number = *numbers.entry(name.clone()).or_insert(next);
                result.shape.push(format!("<id{number}>"));
                result.names.push(name.clone());
            }
        }
    }
    result
}

/** What differs between two versions of selected content
 * Fields
    - changed: bool - the bytes differ
    - whitespace_only: bool - only whitespace differs
    - comments: bool - comment text differs
    - code: bool - code tokens differ
    - business: bool - literals, operators, or called names differ
    - complexity: bool - loop or branch counts differ
    - structure: bool - the identifier-numbered token shape differs
    - renamed: bool - identifiers differ while the shape is the same
*/
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Summary {
    pub(crate) changed: bool,
    pub(crate) whitespace_only: bool,
    pub(crate) comments: bool,
    pub(crate) code: bool,
    pub(crate) business: bool,
    pub(crate) complexity: bool,
    pub(crate) structure: bool,
    pub(crate) renamed: bool,
}

impl Summary {
    /** Describe the detected change kinds in words
     * Input
        - None (uses self)
     * Output
        - String such as "business logic, structure"
    */
    pub(crate) fn describe(&self) -> String {
        if !self.changed {
            return "no change".into();
        }
        if self.whitespace_only {
            return "whitespace only".into();
        }
        let mut kinds = Vec::new();
        for (present, name) in [
            (
                self.business,
                "business logic (literals, operators, or calls)",
            ),
            (self.complexity, "complexity (loops or branches)"),
            (
                self.structure && !self.business && !self.complexity,
                "structure",
            ),
            (self.renamed, "names"),
            (self.comments, "comments"),
        ] {
            if present {
                kinds.push(name);
            }
        }
        if kinds.is_empty() {
            kinds.push("layout");
        }
        kinds.join(", ")
    }
}

/** Classify the difference between two versions of selected content
 * Input
    - before: &str - baseline content
    - after: &str - current content
    - syntax: CommentSyntax - comment syntax of the file
 * Output
    - Summary
*/
pub(crate) fn classify(before: &str, after: &str, syntax: CommentSyntax) -> Summary {
    if before == after {
        return Summary::default();
    }
    let (old, new) = (lex(before, syntax), lex(after, syntax));
    let (old_features, new_features) = (features(&old), features(&new));
    let code = old.tokens != new.tokens;
    let comments = old.comments != new.comments;
    let structure = old_features.shape != new_features.shape;
    Summary {
        changed: true,
        whitespace_only: !code && !comments,
        comments,
        code,
        business: old_features.business != new_features.business,
        complexity: (old_features.loops, old_features.branches)
            != (new_features.loops, new_features.branches),
        structure,
        renamed: code && !structure && old_features.names != new_features.names,
    }
}

/** Decide whether a change satisfies a target command: an unchanged or whitespace-only selection
 * never does; without a change type any code change does (a comment-only change does not); with
 * one, the detected kinds must match it
 * Input
    - summary: &Summary - classified change
    - change_type: Option<ChangeType> - required kind
 * Output
    - Result<(), String>
    - Error explaining why the change is insufficient
*/
pub(crate) fn judge(summary: &Summary, change_type: Option<ChangeType>) -> Result<(), String> {
    if !summary.changed {
        return Err("the target selection has not changed yet".into());
    }
    if summary.whitespace_only {
        return Err("only whitespace changed; a target needs a meaningful change".into());
    }
    let satisfied = match change_type {
        None => summary.code,
        Some(ChangeType::LogicalBn) => summary.business,
        Some(ChangeType::LogicalCn) => summary.complexity,
        Some(ChangeType::LogicalSn) => {
            summary.structure && !summary.business && !summary.complexity
        }
        Some(ChangeType::Semantic) => {
            !summary.structure && !summary.business && (summary.renamed || summary.comments)
        }
    };
    if satisfied {
        Ok(())
    } else {
        Err(format!(
            "the change is not {}: detected {}",
            change_type.map_or("a code change", ChangeType::name),
            summary.describe()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /** Classify a Python change
     * Input
        - before: &str - baseline
        - after: &str - current
     * Output
        - Summary
    */
    fn python(before: &str, after: &str) -> Summary {
        classify(before, after, CommentSyntax::Line("#"))
    }

    /** Check each change kind and the target decisions
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn classifies_change_kinds() {
        let base = "total = price * 2\nreturn total\n";
        assert!(judge(&python(base, base), None).is_err());
        let spaces = python(base, "total  =  price * 2\n\nreturn total\n");
        assert!(spaces.whitespace_only);
        assert!(judge(&spaces, None).unwrap_err().contains("whitespace"));
        let comment = python(base, "# doubled\ntotal = price * 2\nreturn total\n");
        assert!(comment.comments && !comment.code);
        assert!(judge(&comment, None).is_err());
        assert!(judge(&comment, Some(ChangeType::Semantic)).is_ok());
        let business = python(base, "total = price * 3\nreturn total\n");
        assert!(business.business);
        assert!(judge(&business, Some(ChangeType::LogicalBn)).is_ok());
        assert!(judge(&business, Some(ChangeType::Semantic)).is_err());
        let renamed = python(base, "amount = price * 2\nreturn amount\n");
        assert!(renamed.renamed && !renamed.business && !renamed.structure);
        assert!(judge(&renamed, Some(ChangeType::Semantic)).is_ok());
        assert!(judge(&renamed, None).is_ok());
        let complex = python(base, "total = price * 2\nif total:\n    return total\n");
        assert!(complex.complexity);
        assert!(judge(&complex, Some(ChangeType::LogicalCn)).is_ok());
        let reordered = classify(
            "if a:\n    b()\nelse:\n    c()\n",
            "if not a:\n    c()\nelse:\n    b()\n",
            CommentSyntax::Line("#"),
        );
        assert!(reordered.structure && !reordered.business && !reordered.complexity);
        assert!(judge(&reordered, Some(ChangeType::LogicalSn)).is_ok());
    }

    /** Check that strings hiding comment openers and C block comments are lexed correctly
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn respects_strings_and_block_comments() {
        let summary = classify(
            "let url = \"http://x\"; /* a */\n",
            "let url = \"http://x\"; /* b */\n",
            CommentSyntax::Line("//"),
        );
        assert!(summary.comments && !summary.code);
        let changed = classify("s = 'a#b'\n", "s = 'a#c'\n", CommentSyntax::Line("#"));
        assert!(changed.business);
    }
}
