// Provider-neutral tool calls, as translated by the agent adapters, and the simulation of a
// proposed write on a file's current text.

/** What a tool call does, as far as can be told before it runs
 * Variants
    - Read - only inspects (files, search, web reads)
    - Write - replaces, edits, or deletes named files
    - Execute - runs a shell command, whose effects are unknown until it runs
    - Other - any other tool (MCP servers, new built-ins), whose effects are unknown
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Read,
    Write,
    Execute,
    Other,
}

impl Operation {
    /** Return the operation's name
     * Input
        - None (uses self)
     * Output
        - &'static str, "read", "write", "execute", or "other"
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Other => "other",
        }
    }

    /** Parse an operation name of the neutral action format
     * Input
        - value: &str - name
     * Output
        - Option<Operation>
    */
    pub(crate) fn parse(value: &str) -> Option<Self> {
        [Self::Read, Self::Write, Self::Execute, Self::Other]
            .into_iter()
            .find(|operation| operation.name() == value)
    }
}

/** One text replacement proposed by an edit tool
 * Fields
    - old: String - text to find
    - new: String - replacement
    - all: bool - replace every occurrence instead of the first
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextEdit {
    pub(crate) old: String,
    pub(crate) new: String,
    pub(crate) all: bool,
}

/** The new state a write proposes for one file
 * Variants
    - Content(String) - the complete new text
    - Edits(Vec<TextEdit>) - replacements applied in order
    - Delete - the file is removed
    - Unknown - the tool does not say, so the effect cannot be simulated
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Proposed {
    Content(String),
    Edits(Vec<TextEdit>),
    Delete,
    Unknown,
}

/** A file a write proposes to change
 * Fields
    - path: String - path as the tool gave it (absolute or relative to the agent's directory)
    - proposed: Proposed - new state
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileChange {
    pub(crate) path: String,
    pub(crate) proposed: Proposed,
}

/** A tool call in provider-neutral form
 * Fields
    - tool: String - provider tool name
    - operation: Operation - kind of action
    - files: Vec<FileChange> - files a write proposes to change
    - reads: Vec<String> - paths or URLs a read names
    - command: Option<String> - shell command text
    - arguments: Vec<String> - every string argument (scanned, never stored raw)
    - digest: String - SHA-512 of the raw tool input, stored instead of the input
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentAction {
    pub(crate) tool: String,
    pub(crate) operation: Operation,
    pub(crate) files: Vec<FileChange>,
    pub(crate) reads: Vec<String>,
    pub(crate) command: Option<String>,
    pub(crate) arguments: Vec<String>,
    pub(crate) digest: String,
}

/** Simulate a proposed change on the current text, applying edits in order; an edit whose old
 * text is not found is retried with CRLF line endings, since editors may normalize them
 * Input
    - proposed: &Proposed - proposed new state
    - before: Option<&str> - current text, None if the file does not exist
 * Output
    - Result<Option<String>, String> new text (None when deleted), or why it cannot be simulated
*/
pub(crate) fn apply(proposed: &Proposed, before: Option<&str>) -> Result<Option<String>, String> {
    match proposed {
        Proposed::Content(text) => Ok(Some(text.clone())),
        Proposed::Delete => Ok(None),
        Proposed::Unknown => Err("the tool does not state the new file content".into()),
        Proposed::Edits(edits) => {
            let mut text = before.unwrap_or_default().to_string();
            for edit in edits {
                if edit.old.is_empty() {
                    if !text.is_empty() {
                        return Err("an edit with empty old text targets a non-empty file".into());
                    }
                    text = edit.new.clone();
                    continue;
                }
                let (old, new) = if text.contains(&edit.old) {
                    (edit.old.clone(), edit.new.clone())
                } else {
                    (
                        edit.old.replace('\n', "\r\n"),
                        edit.new.replace('\n', "\r\n"),
                    )
                };
                if !text.contains(&old) {
                    return Err(
                        "the text to replace was not found, so the edit cannot be simulated".into(),
                    );
                }
                text = if edit.all {
                    text.replace(&old, &new)
                } else {
                    text.replacen(&old, &new, 1)
                };
            }
            Ok(Some(text))
        }
    }
}
