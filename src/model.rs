/** A parsed .crane policy, produced by policy::parse from a "policy NAME { ... }" block and
 * evaluated rule by rule during crane check
 * Fields
    - name: String - policy identifier, used as policy_id in reports
    - checkpoint: String - name of the checkpoint the rules compare against
    - rules: Vec<Rule> - rules in source order, at least one
*/
#[derive(Debug, Clone)]
pub(crate) struct Policy {
    pub(crate) name: String,
    pub(crate) checkpoint: String,
    pub(crate) rules: Vec<Rule>,
}

/** A single policy rule
 * Variants
    - Preserve { kind, target, scope } - the code covered by scope around the target item must
      match its checkpoint version (ignoring comments)
    - Target { kind, target, scope, change_type } - the code covered by scope around the target
      item must differ from its checkpoint version, and when change_type is given the difference
      must be of that kind
*/
#[derive(Debug, Clone)]
pub(crate) enum Rule {
    Preserve {
        kind: ItemKind,
        target: String,
        scope: Scope,
    },
    Target {
        kind: ItemKind,
        target: String,
        scope: Scope,
        change_type: Option<ChangeType>,
    },
}

impl Rule {
    /** Render the rule back in policy syntax (without the ';'), for reports and context output,
     * by writing the keyword, item flag, target, scope, and change type when present
     * Input
        - None (uses self)
     * Output
        - String such as "target --function A.b scope flow change_type logical_bn"
    */
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Preserve {
                kind,
                target,
                scope,
            } => format!("preserve {} {target} scope {}", kind.flag(), scope.name()),
            Self::Target {
                kind,
                target,
                scope,
                change_type,
            } => {
                let mut text = format!("target {} {target} scope {}", kind.flag(), scope.name());
                if let Some(change_type) = change_type {
                    text.push_str(&format!(" change_type {}", change_type.name()));
                }
                text
            }
        }
    }
}

/** What kind of code item a rule points at, written as the flag after preserve or target
 * Variants
    - Function - a function or method definition (--function)
    - Data - the value stored in a variable, such as its initializer (--data)
    - Variable - a whole variable declaration: name, type, modifiers, and value (--variable)
    - Class - a class as a whole, with its fields and methods (--class); Rust structs and enums
    - Interface - an interface as a whole (--interface); Rust traits
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemKind {
    Function,
    Data,
    Variable,
    Class,
    Interface,
}

impl ItemKind {
    /** Every item kind, in the order used for flags and messages */
    pub(crate) const ALL: [ItemKind; 5] = [
        Self::Function,
        Self::Data,
        Self::Variable,
        Self::Class,
        Self::Interface,
    ];

    /** Accepted flags, listed in error messages */
    pub(crate) const FLAGS: &'static str =
        "--function, --data, --variable, --class, or --interface";

    /** Parse an item flag, by matching it against the flag of every kind
     * Input
        - flag: &str - a word such as "--class"
     * Output
        - Option<ItemKind>, None if the word is not an item flag
    */
    pub(crate) fn from_flag(flag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.flag() == flag)
    }

    /** Return the flag for this kind, as written in policies and commands
     * Input
        - None (uses self)
     * Output
        - &'static str such as "--function"
    */
    pub(crate) fn flag(self) -> &'static str {
        match self {
            Self::Function => "--function",
            Self::Data => "--data",
            Self::Variable => "--variable",
            Self::Class => "--class",
            Self::Interface => "--interface",
        }
    }

    /** Return the word for this kind used in messages, such as "protected class X"
     * Input
        - None (uses self)
     * Output
        - &'static str such as "function"
    */
    pub(crate) fn noun(self) -> &'static str {
        &self.flag()[2..]
    }
}

/** The kind of change a target rule requires, written as "change_type VALUE"; Crane measures
 * each kind deterministically from the syntax tree, it does not judge intent
 * Variants
    - LogicalBn - business logic: literals, operators, or called functions changed
    - LogicalCn - complexity: loop count, loop nesting depth, or branch count changed
    - LogicalSn - structure: code was reordered or restructured while literals, operators,
      called functions, and complexity stayed the same
    - Semantic - wording and alignment: names, comments, or layout changed while the code's
      shape, literals, operators, and called functions stayed the same
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeType {
    LogicalBn,
    LogicalCn,
    LogicalSn,
    Semantic,
}

impl ChangeType {
    /** Parse a change type keyword, by lowercasing it and matching the four supported values
     * Input
        - value: &str - change type from a policy
     * Output
        - Result<ChangeType, String>
        - Error if the value is not logical_bn, logical_cn, logical_sn, or semantic
    */
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "logical_bn" => Ok(Self::LogicalBn),
            "logical_cn" => Ok(Self::LogicalCn),
            "logical_sn" => Ok(Self::LogicalSn),
            "semantic" => Ok(Self::Semantic),
            _ => Err(format!(
                "invalid change_type '{value}'; expected logical_bn, logical_cn, logical_sn, or semantic"
            )),
        }
    }

    /** Return the keyword for this change type, by matching the variant to its policy spelling
     * Input
        - None (uses self)
     * Output
        - &'static str change type keyword
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

/** How much code around the target a preserve rule protects, written as "scope VALUE" at the end
 * of a preserve statement or protect command
 * Variants
    - Block - only the target function's own definition (the default)
    - File - the whole file that defines the target
    - Flow - the target plus every function it calls and every function that calls it,
      transitively, traced through the codebase by function name
    - Folder - every file in the folder that contains the target, including subfolders
    - All - every file in the repository
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Block,
    File,
    Flow,
    Folder,
    All,
}

impl Scope {
    /** Parse a scope value, by matching the exact lowercase keyword
     * Input
        - value: &str - scope keyword from a policy or command line
     * Output
        - Result<Scope, String>
        - Error if the value is not block, file, flow, folder, or all
    */
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "block" => Ok(Self::Block),
            "file" => Ok(Self::File),
            "flow" => Ok(Self::Flow),
            "folder" => Ok(Self::Folder),
            "all" => Ok(Self::All),
            _ => Err(format!(
                "invalid scope '{value}'; expected block, file, flow, folder, or all"
            )),
        }
    }

    /** Return the keyword for this scope, by matching the variant to its policy spelling
     * Input
        - None (uses self)
     * Output
        - &'static str scope keyword
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::File => "file",
            Self::Flow => "flow",
            Self::Folder => "folder",
            Self::All => "all",
        }
    }
}

/** A trusted baseline stored in .crane/checkpoints/NAME.json, written by crane checkpoint and read
 * back by repository::load_checkpoint
 * Fields
    - name: String - checkpoint identifier
    - commit: String - Git commit SHA used as the baseline
    - branch: String - branch at creation time (DETACHED if none), informational only
    - created_at_unix: u64 - creation time in seconds; not parsed back when loading, so it is 0 then
*/
#[derive(Debug, Clone)]
pub(crate) struct Checkpoint {
    pub(crate) name: String,
    pub(crate) commit: String,
    pub(crate) branch: String,
    pub(crate) created_at_unix: u64,
}

/** One failed rule or setup problem, serialized into the stable JSON contract by check::render_json
 * (repair_owner is derived from these fields at render time, not stored)
 * Fields
    - policy_id: String - failing policy, or "crane" for setup-level failures
    - rule: String - rule kind, such as preserve, policy, or verification
    - target: String - protected function, empty for policy and setup failures
    - checkpoint: String - checkpoint name, empty when not applicable
    - violation_type: String - machine-readable category, such as source_changed or malformed_policy
    - message: String - human-readable explanation
*/
#[derive(Debug, Clone)]
pub(crate) struct Violation {
    pub(crate) policy_id: String,
    pub(crate) rule: String,
    pub(crate) target: String,
    pub(crate) checkpoint: String,
    pub(crate) violation_type: String,
    pub(crate) message: String,
}

/** A resolved function definition, reduced to its canonical form for comparison
 * Fields
    - snippet: String - the definition's source text with comments removed, trailing whitespace
      trimmed, and blank lines dropped; indentation and line layout are kept, so two snippets are
      equal only if the code is written identically
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceTarget {
    pub(crate) snippet: String,
}
