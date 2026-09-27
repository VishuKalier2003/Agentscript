use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::thread;

use crate::model::{ChangeType, ItemKind, Scope};
use crate::repository::{ensure_commit, git, git_raw, BlobReader};
use crate::resolver::{
    canonical_file, definitions, file_features, matches_target, supported, Definition, Features,
};
use crate::util::io_error;

/** Metadata directories that belong to tooling rather than the codebase, excluded from every scope */
const EXCLUDED_PREFIXES: &[&str] = &[".crane/", ".claude/"];

/** Maximum number of changed paths or functions listed in one violation message */
const REPORTED_CHANGES: usize = 10;

/** Maximum number of names passed to one git grep call, keeping command lines short on Windows */
const GREP_BATCH: usize = 200;

/** Protected content keyed by file path or "path#qualified.name", mapped to canonical text */
type Members = BTreeMap<String, String>;

/** Measured code units (functions or files) keyed like Members, used by target rules */
type Units = BTreeMap<String, Features>;

/** Identifies one traced definition as (index into Graph::files, index into that file's list) */
type DefinitionId = (usize, usize);

/** Which version of the repository is being read
 * Variants
    - Checkpoint - files as committed in the checkpoint commit
    - Worktree - files on disk under the repository root, honoring .gitignore
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Side {
    Checkpoint,
    Worktree,
}

impl Side {
    /** Name this side in diagnostics, keeping the "checkpoint" and "worktree" wording that violation
     * classification and repair ownership depend on
     * Input
        - None (uses self)
     * Output
        - &'static str, "checkpoint" or "worktree"
    */
    fn label(self) -> &'static str {
        match self {
            Self::Checkpoint => "checkpoint",
            Self::Worktree => "worktree",
        }
    }
}

/** State shared by every scope rule in one crane run, so Git processes, file listings, and parsed
 * files are created once instead of once per rule
 * Fields
    - root: Option<PathBuf> - repository root, looked up on first use
    - commits: HashMap<String, CommitState> - per-checkpoint-commit state, created on first use
*/
pub(crate) struct ScopeContext {
    root: Option<PathBuf>,
    commits: HashMap<String, CommitState>,
}

/** Everything cached for comparing one checkpoint commit with the worktree
 * Fields
    - commit: String - checkpoint commit SHA
    - root: PathBuf - repository root
    - blobs: BlobReader - batch reader for checkpoint files
    - parsed: HashMap<(Side, String), Rc<Vec<Definition>>> - definitions per parsed file
    - flagged: Option<Vec<String>> - worktree files hidden from Git's change detection
      (assume-unchanged or skip-worktree), looked up on first use
*/
struct CommitState {
    commit: String,
    root: PathBuf,
    blobs: BlobReader,
    parsed: HashMap<(Side, String), Rc<Vec<Definition>>>,
    flagged: Option<Vec<String>>,
}

/** The part of the call graph loaded so far while tracing a flow
 * Fields
    - files: Vec<(String, Rc<Vec<Definition>>)> - loaded files and their definitions
    - loaded: HashSet<String> - paths already loaded
    - searched: HashSet<String> - names already searched for with git grep
    - by_name: HashMap<String, Vec<DefinitionId>> - functions by name
    - callers_of: HashMap<String, Vec<DefinitionId>> - functions by the names they call
    - users_of: HashMap<String, Vec<DefinitionId>> - functions by the identifiers they use
*/
#[derive(Default)]
struct Graph {
    files: Vec<(String, Rc<Vec<Definition>>)>,
    loaded: HashSet<String>,
    searched: HashSet<String>,
    by_name: HashMap<String, Vec<DefinitionId>>,
    callers_of: HashMap<String, Vec<DefinitionId>>,
    users_of: HashMap<String, Vec<DefinitionId>>,
}

impl Graph {
    /** Look up a loaded definition by id
     * Input
        - id: DefinitionId - file and definition index
     * Output
        - &Definition
    */
    fn get(&self, (file, index): DefinitionId) -> &Definition {
        &self.files[file].1[index]
    }
}

impl ScopeContext {
    /** Create an empty context; Git processes and listings are started lazily on first use
     * Input
        - None
     * Output
        - ScopeContext
    */
    pub(crate) fn new() -> Self {
        Self {
            root: None,
            commits: HashMap::new(),
        }
    }

    /** Get the cached state for a checkpoint commit, by looking up the repository root once and,
     * the first time a commit is seen, confirming it exists and starting its batch blob reader
     * Input
        - commit: &str - checkpoint commit SHA
     * Output
        - Result<&mut CommitState, String>
        - Error if the commit is missing or Git cannot be started
    */
    fn state(&mut self, commit: &str) -> Result<&mut CommitState, String> {
        if self.root.is_none() {
            self.root = Some(PathBuf::from(git(&["rev-parse", "--show-toplevel"])?));
        }
        let root = self.root.clone().unwrap_or_default();
        match self.commits.entry(commit.to_string()) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                ensure_commit(commit)?;
                Ok(entry.insert(CommitState {
                    commit: commit.to_string(),
                    root,
                    blobs: BlobReader::new()?,
                    parsed: HashMap::new(),
                    flagged: None,
                }))
            }
        }
    }
}

/** Verify a preserve rule with a non-block scope, by building only the content that can differ:
 * for file, folder, and all, the files whose Git content hash changed since the checkpoint (passing
 * at once when none did); for flow, the functions reached by tracing calls both ways from the
 * target in each version; and finally comparing the checkpoint and worktree maps entry by entry
 * Input
    - context: &mut ScopeContext - shared state for this crane run
    - scope: Scope - file, flow, folder, or all
    - commit: &str - checkpoint commit SHA
    - kind: ItemKind - kind of the item that anchors the scope
    - target: &str - qualified target that anchors the scope
 * Output
    - Result<(), String>
    - Error describing a missing or ambiguous target, a parse failure, or the modified entries
*/
pub(crate) fn verify_scope(
    context: &mut ScopeContext,
    scope: Scope,
    commit: &str,
    kind: ItemKind,
    target: &str,
) -> Result<(), String> {
    let state = context.state(commit)?;
    match scope {
        Scope::Block => {
            Err("block scope is verified by comparing the function node directly".into())
        }
        Scope::File => {
            let path = state.locate(kind, target)?;
            let changed = state.changed_files(&path, |candidate| candidate == path)?;
            state.compare_files(scope, &changed)
        }
        Scope::Folder => {
            let folder = folder_of(&state.locate(kind, target)?);
            let prefix = folder_prefix(&folder);
            let changed =
                state.changed_files(&folder, |candidate| candidate.starts_with(&prefix))?;
            state.compare_files(scope, &changed)
        }
        Scope::All => {
            let changed = state.changed_files("", |_| true)?;
            state.compare_files(scope, &changed)
        }
        Scope::Flow => {
            let (expected, actual) = state.flows(kind, target)?;
            compare(scope, &expected, &actual)
        }
    }
}

/** Verify a target rule, by collecting the code covered by the scope on both sides (the target
 * function for block; the changed files for file, folder, and all, failing at once when nothing
 * changed; the flow members for flow), classifying how that code changed, and requiring a change
 * of the requested kind
 * Input
    - context: &mut ScopeContext - shared state for this crane run
    - scope: Scope - block, file, flow, folder, or all
    - change_type: Option<ChangeType> - required kind of change, any change when None
    - commit: &str - checkpoint commit SHA
    - kind: ItemKind - kind of the item that anchors the scope
    - target: &str - qualified target that anchors the scope
 * Output
    - Result<(), String>
    - Error if the target is missing or ambiguous, a file does not parse, nothing changed, or
      the change is of the wrong kind
*/
pub(crate) fn verify_target(
    context: &mut ScopeContext,
    scope: Scope,
    change_type: Option<ChangeType>,
    commit: &str,
    kind: ItemKind,
    target: &str,
) -> Result<(), String> {
    let state = context.state(commit)?;
    let (before, after) = match scope {
        Scope::Block => state.block_units(kind, target)?,
        Scope::Flow => state.flow_units(kind, target)?,
        Scope::File | Scope::Folder | Scope::All => {
            let changed = match scope {
                Scope::File => {
                    let path = state.locate(kind, target)?;
                    state.changed_files(&path, |candidate| candidate == path)?
                }
                Scope::Folder => {
                    let folder = folder_of(&state.locate(kind, target)?);
                    let prefix = folder_prefix(&folder);
                    state.changed_files(&folder, |candidate| candidate.starts_with(&prefix))?
                }
                _ => state.changed_files("", |_| true)?,
            };
            state.file_units(&changed)?
        }
    };
    judge(kind, target, scope, change_type, &before, &after)
}

/** Find the file that defines a target in the checkpoint, used when creating rules to reject
 * targets that are missing or ambiguous before they are written to a policy
 * Input
    - context: &mut ScopeContext - shared state for this crane run
    - commit: &str - checkpoint commit SHA
    - kind: ItemKind - kind of the item
    - target: &str - qualified target
 * Output
    - Result<String, String> repository-relative path of the defining file
    - Error if the commit is missing or the target is missing or ambiguous in the checkpoint
*/
pub(crate) fn locate_target(
    context: &mut ScopeContext,
    commit: &str,
    kind: ItemKind,
    target: &str,
) -> Result<String, String> {
    context.state(commit)?.locate(kind, target)
}

/** How the code covered by a target rule changed, summed over every unit (function or file)
 * Fields
    - changed: Vec<String> - units whose text differs, including added and removed units
    - business: bool - literals, operators, or called functions differ somewhere
    - complexity: bool - loop or branch measurements differ somewhere
    - structure: bool - the identifier-masked token sequence differs somewhere
*/
#[derive(Debug, Default)]
struct ChangeSummary {
    changed: Vec<String>,
    business: bool,
    complexity: bool,
    structure: bool,
}

impl ChangeSummary {
    /** List the kinds of change detected, for violation messages
     * Input
        - None (uses self)
     * Output
        - String such as "business logic, structure" or "wording and alignment only"
    */
    fn describe(&self) -> String {
        let kinds = [
            (self.business, "business logic (logical_bn)"),
            (self.complexity, "complexity (logical_cn)"),
            (self.structure, "structure"),
        ]
        .into_iter()
        .filter(|(present, _)| *present)
        .map(|(_, name)| name)
        .collect::<Vec<_>>();
        if kinds.is_empty() {
            "wording and alignment only (semantic)".into()
        } else {
            kinds.join(", ")
        }
    }
}

/** Classify the change between two sets of units and check it against the required change type,
 * by comparing each unit's features (a unit present on one side only is compared with empty
 * features) and then applying: any change for no change type; business differences for
 * logical_bn; complexity differences for logical_cn; structure differences without business or
 * complexity differences for logical_sn; and text differences without structure or business
 * differences for semantic
 * Input
    - kind: ItemKind - kind of the target item, used in messages
    - target: &str - qualified target, used in messages
    - scope: Scope - scope of the rule, used in messages
    - change_type: Option<ChangeType> - required kind of change
    - before: &Units - checkpoint units
    - after: &Units - worktree units
 * Output
    - Result<(), String>
    - Error if nothing changed or the change is of the wrong kind
*/
fn judge(
    kind: ItemKind,
    target: &str,
    scope: Scope,
    change_type: Option<ChangeType>,
    before: &Units,
    after: &Units,
) -> Result<(), String> {
    let empty = Features::default();
    let mut summary = ChangeSummary::default();
    for key in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        let old = before.get(key).unwrap_or(&empty);
        let new = after.get(key).unwrap_or(&empty);
        if before.contains_key(key) == after.contains_key(key) && old.text == new.text {
            continue;
        }
        summary.changed.push(key.clone());
        summary.business |= old.business != new.business;
        summary.complexity |= old.complexity != new.complexity;
        summary.structure |= old.shape != new.shape;
    }
    if summary.changed.is_empty() {
        return Err(format!(
            "Target {} {target} was not changed within {} scope",
            kind.noun(),
            scope.name()
        ));
    }
    let satisfied = match change_type {
        None => true,
        Some(ChangeType::LogicalBn) => summary.business,
        Some(ChangeType::LogicalCn) => summary.complexity,
        Some(ChangeType::LogicalSn) => {
            summary.structure && !summary.business && !summary.complexity
        }
        Some(ChangeType::Semantic) => !summary.structure && !summary.business,
    };
    if satisfied {
        return Ok(());
    }
    let required = change_type.map_or("any", ChangeType::name);
    let mut listed = summary
        .changed
        .iter()
        .take(REPORTED_CHANGES)
        .cloned()
        .collect::<Vec<_>>();
    if summary.changed.len() > REPORTED_CHANGES {
        listed.push(format!(
            "and {} more",
            summary.changed.len() - REPORTED_CHANGES
        ));
    }
    Err(format!(
        "Target change within {} scope is not {required}: detected {}; changed: {}",
        scope.name(),
        summary.describe(),
        listed.join(", ")
    ))
}

/** Return the folder part of a repository-relative path, empty for files at the root
 * Input
    - path: &str - repository-relative file path
 * Output
    - String folder path without a trailing slash
*/
fn folder_of(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(folder, _)| folder.to_string())
        .unwrap_or_default()
}

/** Turn a folder into a path prefix that only matches files inside it, empty for the root
 * Input
    - folder: &str - folder path without a trailing slash
 * Output
    - String prefix ending in "/", or empty
*/
fn folder_prefix(folder: &str) -> String {
    if folder.is_empty() {
        String::new()
    } else {
        format!("{folder}/")
    }
}

impl CommitState {
    /** List the files under a path whose content differs between the checkpoint and the worktree,
     * by reading checkpoint blob ids with one git ls-tree, listing worktree files with one git
     * ls-files, hashing their current contents with one git hash-object, and keeping every path
     * whose id differs or that exists on only one side; hashing real contents means timestamps,
     * assume-unchanged, and skip-worktree flags cannot hide an edit
     * Input
        - path: &str - repository-relative file or folder to list, empty for the whole repository
        - keep: impl Fn(&str) -> bool - exact filter applied to every listed path
     * Output
        - Result<Vec<String>, String> of changed paths, sorted
        - Error if a Git command fails
    */
    fn changed_files(
        &self,
        path: &str,
        keep: impl Fn(&str) -> bool,
    ) -> Result<Vec<String>, String> {
        let keep = |candidate: &str| {
            !candidate.is_empty()
                && keep(candidate)
                && !EXCLUDED_PREFIXES
                    .iter()
                    .any(|prefix| candidate.starts_with(prefix))
        };

        let mut tree_args = vec!["ls-tree", "-r", "-z", "--full-tree", self.commit.as_str()];
        if !path.is_empty() {
            tree_args.extend(["--", path]);
        }
        let mut committed = HashMap::new();
        for entry in git_raw(&tree_args)?.split('\0') {
            let Some((meta, file)) = entry.split_once('\t') else {
                continue;
            };
            let fields = meta.split(' ').collect::<Vec<_>>();
            if let [_, "blob", oid] = fields.as_slice() {
                if keep(file) {
                    committed.insert(file.to_string(), oid.to_string());
                }
            }
        }

        let pathspec = format!(":/{path}");
        let current = git_raw(&[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--full-name",
            "--",
            &pathspec,
        ])?
        .split('\0')
        .filter(|file| keep(file) && self.root.join(file).is_file())
        .map(String::from)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
        let hashes = hash_files(&self.root, &current)?;

        let mut changed = current
            .iter()
            .zip(&hashes)
            .filter(|(file, hash)| committed.get(*file) != Some(*hash))
            .map(|(file, _)| file.clone())
            .collect::<BTreeSet<_>>();
        let present = current.iter().collect::<HashSet<_>>();
        changed.extend(
            committed
                .keys()
                .filter(|file| !present.contains(file))
                .cloned(),
        );
        Ok(changed.into_iter().collect())
    }

    /** Compare changed files by their canonical text, by canonicalizing each path on both sides
     * (a path absent on one side is left out of that side's map) and passing at once when there is
     * nothing to compare
     * Input
        - scope: Scope - scope being verified, used in the message
        - paths: &[String] - changed paths
     * Output
        - Result<(), String>
        - Error listing the modified files, or a read or parse failure
    */
    fn compare_files(&mut self, scope: Scope, paths: &[String]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut expected = BTreeMap::new();
        let mut actual = BTreeMap::new();
        for path in paths {
            if let Some(text) = self.canonical(Side::Checkpoint, path)? {
                expected.insert(path.clone(), text);
            }
            if let Some(text) = self.canonical(Side::Worktree, path)? {
                actual.insert(path.clone(), text);
            }
        }
        compare(scope, &expected, &actual)
    }

    /** Read one file from a side without altering its bytes, using the batch blob reader for the
     * checkpoint and the filesystem for the worktree, decoding invalid UTF-8 lossily on both sides
     * Input
        - side: Side - checkpoint or worktree
        - path: &str - repository-relative path
     * Output
        - Result<Option<String>, String>, None if the file does not exist on that side
        - Error if the file cannot be read
    */
    fn read(&mut self, side: Side, path: &str) -> Result<Option<String>, String> {
        let bytes = match side {
            Side::Checkpoint => self.blobs.read(&self.commit, path)?,
            Side::Worktree => {
                let file = self.root.join(path);
                if file.is_file() {
                    Some(fs::read(file).map_err(io_error)?)
                } else {
                    None
                }
            }
        };
        Ok(bytes.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    /** Canonicalize one file from a side, by reading it and applying canonical_file
     * Input
        - side: Side - checkpoint or worktree
        - path: &str - repository-relative path
     * Output
        - Result<Option<String>, String>, None if the file does not exist on that side
        - Error if the file cannot be read or a supported source file does not parse
    */
    fn canonical(&mut self, side: Side, path: &str) -> Result<Option<String>, String> {
        let Some(source) = self.read(side, path)? else {
            return Ok(None);
        };
        canonical_file(&source, path).map(Some).map_err(|error| {
            format!(
                "{} source could not be parsed: {path}: {error}",
                side.label()
            )
        })
    }

    /** Parse one supported file from a side into definitions, at most once per run, by returning
     * the memoized result when present and otherwise reading and parsing the file
     * Input
        - side: Side - checkpoint or worktree
        - path: &str - repository-relative path of a supported source file
     * Output
        - Result<Rc<Vec<Definition>>, String>, empty if the file does not exist on that side
        - Error if the file cannot be read or does not parse
    */
    fn definitions(&mut self, side: Side, path: &str) -> Result<Rc<Vec<Definition>>, String> {
        let key = (side, path.to_string());
        if let Some(found) = self.parsed.get(&key) {
            return Ok(Rc::clone(found));
        }
        let found = match self.read(side, path)? {
            Some(source) => definitions(&source, path).map_err(|error| {
                format!(
                    "{} source could not be parsed: {path}: {error}",
                    side.label()
                )
            })?,
            None => Vec::new(),
        };
        let found = Rc::new(found);
        self.parsed.insert(key, Rc::clone(&found));
        Ok(found)
    }

    /** List worktree files that Git may read from its index instead of disk (assume-unchanged or
     * skip-worktree), once per run, by reading the tag letters from git ls-files -v
     * Input
        - None (uses self)
     * Output
        - Result<Vec<String>, String> of flagged repository-relative paths
        - Error if git ls-files fails
    */
    fn flagged(&mut self) -> Result<Vec<String>, String> {
        if let Some(flagged) = &self.flagged {
            return Ok(flagged.clone());
        }
        let flagged = git_raw(&["ls-files", "-v", "-z", "--full-name", "--", ":/"])?
            .split('\0')
            .filter_map(|entry| entry.split_once(' '))
            .filter(|(tag, _)| tag.chars().any(|c| c.is_ascii_lowercase()) || *tag == "S")
            .map(|(_, path)| path.to_string())
            .collect::<Vec<_>>();
        self.flagged = Some(flagged.clone());
        Ok(flagged)
    }

    /** Start git grep processes that list supported source files on a side containing any of the
     * names as a whole word, without waiting for them, so greps for both sides run at the same time
     * (the commit tree for the checkpoint; tracked plus untracked-but-not-ignored files for the
     * worktree); names are split into batches to keep command lines short
     * Input
        - side: Side - checkpoint or worktree
        - names: &[String] - identifiers to search for
     * Output
        - Result<Vec<Child>, String> running git grep processes
        - Error if git cannot be started
    */
    fn grep_spawn(&self, side: Side, names: &[String]) -> Result<Vec<Child>, String> {
        let mut children = Vec::new();
        for chunk in names.chunks(GREP_BATCH) {
            let mut args = vec!["grep", "-l", "-z", "-w", "-F", "--full-name"];
            if side == Side::Worktree {
                args.push("--untracked");
            }
            for name in chunk {
                args.extend(["-e", name.as_str()]);
            }
            if side == Side::Checkpoint {
                args.push(self.commit.as_str());
            }
            args.extend(["--", ":/"]);
            let child = Command::new("git")
                .args(&args)
                .current_dir(&self.root)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| format!("failed to execute git: {error}"))?;
            children.push(child);
        }
        Ok(children)
    }

    /** Collect the files found by grep_spawn, by waiting for each process (exit code 1 means no
     * match), stripping the "COMMIT:" prefix from checkpoint results, always adding flagged
     * worktree files (Git may grep their index copy instead of the file on disk), and keeping
     * only supported source files outside tooling metadata
     * Input
        - side: Side - checkpoint or worktree
        - children: Vec<Child> - processes started by grep_spawn
     * Output
        - Result<Vec<String>, String> of matching repository-relative paths, sorted
        - Error if git grep fails
    */
    fn grep_collect(&mut self, side: Side, children: Vec<Child>) -> Result<Vec<String>, String> {
        let prefix = format!("{}:", self.commit);
        let mut found = BTreeSet::new();
        for child in children {
            let output = child.wait_with_output().map_err(io_error)?;
            match output.status.code() {
                Some(0) => {}
                Some(1) => continue, // git grep exits 1 when nothing matched
                _ => return Err(String::from_utf8_lossy(&output.stderr).trim().into()),
            }
            for entry in String::from_utf8_lossy(&output.stdout).split('\0') {
                let path = match side {
                    Side::Checkpoint => entry.strip_prefix(&prefix).unwrap_or(entry),
                    Side::Worktree => entry,
                };
                found.insert(path.to_string());
            }
        }
        if side == Side::Worktree {
            found.extend(self.flagged()?);
        }
        Ok(found
            .into_iter()
            .filter(|path| {
                supported(path)
                    && !EXCLUDED_PREFIXES
                        .iter()
                        .any(|prefix| path.starts_with(prefix))
            })
            .collect())
    }

    /** Load into each side's graph every file that mentions a not-yet-searched name, by starting
     * the greps for both sides together, then parsing each new file once and indexing its
     * definitions by name and by the names they call; any definition or caller of a name must
     * mention that name, so each graph is complete for every name it has searched
     * Input
        - graphs: &mut [Graph; 2] - partially loaded call graphs, indexed like SIDES
        - names: [Vec<String>; 2] - names the next tracing step needs on each side
     * Output
        - Result<(), String>
        - Error if git grep fails or a candidate file does not parse
    */
    fn load(&mut self, graphs: &mut [Graph; 2], names: [Vec<String>; 2]) -> Result<(), String> {
        let mut pending = Vec::new();
        for (index, names) in names.into_iter().enumerate() {
            let names = names
                .into_iter()
                .filter(|name| graphs[index].searched.insert(name.clone()))
                .collect::<Vec<_>>();
            if !names.is_empty() {
                pending.push((index, self.grep_spawn(SIDES[index], &names)?));
            }
        }
        for (index, children) in pending {
            let side = SIDES[index];
            let graph = &mut graphs[index];
            for path in self.grep_collect(side, children)? {
                if !graph.loaded.insert(path.clone()) {
                    continue;
                }
                let found = self.definitions(side, &path)?;
                let file = graph.files.len();
                // Only functions join the call graph; other items can still start a flow
                for (position, definition) in found.iter().enumerate() {
                    if definition.kind != ItemKind::Function {
                        continue;
                    }
                    let id = (file, position);
                    graph
                        .by_name
                        .entry(definition.name.clone())
                        .or_default()
                        .push(id);
                    for call in &definition.calls {
                        graph.callers_of.entry(call.clone()).or_default().push(id);
                    }
                    for name in &definition.uses {
                        graph.users_of.entry(name.clone()).or_default().push(id);
                    }
                }
                graph.files.push((path, found));
            }
        }
        Ok(())
    }

    /** Find the file that defines the target on the checkpoint side, by grepping for the target's
     * function name, parsing only the matching files, and requiring exactly one definition
     * Input
        - target: &str - qualified function target
     * Output
        - Result<String, String> repository-relative path of the defining file
        - Error if the target is missing or ambiguous
    */
    fn locate(&mut self, kind: ItemKind, target: &str) -> Result<String, String> {
        let mut graphs = [Graph::default(), Graph::default()];
        self.load(&mut graphs, [vec![function_name(target)], Vec::new()])?;
        let (file, _) = find_start(&graphs[0], Side::Checkpoint, kind, target)?;
        Ok(graphs[0].files[file].0.clone())
    }

    /** Build the flow around the target in the checkpoint and in the worktree, by finding the unique
     * target definition on each side, then tracing downstream (functions it calls) and upstream
     * (functions that call it) together one level at a time, where each level loads, for both
     * sides at once, only the files that mention the names that level needs; calls link to
     * definitions by function name, and every reached definition is keyed by "path#qualified.name"
     * Input
        - target: &str - qualified function target
     * Output
        - Result<(Members, Members), String> checkpoint and worktree flow members
        - Error if the target is missing or ambiguous, or a traced file does not parse
    */
    fn flows(&mut self, kind: ItemKind, target: &str) -> Result<(Members, Members), String> {
        let (graphs, [checkpoint, worktree]) = self.trace(kind, target)?;
        let snippet = |definition: &Definition| definition.snippet.clone();
        Ok((
            checkpoint.members(&graphs[0], snippet),
            worktree.members(&graphs[1], snippet),
        ))
    }

    /** Measure every flow member in the checkpoint and in the worktree for a target rule, by
     * tracing the flow like flows and recording each member's Features
     * Input
        - target: &str - qualified function target
     * Output
        - Result<(Units, Units), String> checkpoint and worktree flow members
        - Error if the target is missing or ambiguous, or a traced file does not parse
    */
    fn flow_units(&mut self, kind: ItemKind, target: &str) -> Result<(Units, Units), String> {
        let (graphs, [checkpoint, worktree]) = self.trace(kind, target)?;
        let features = |definition: &Definition| definition.features.clone();
        Ok((
            checkpoint.members(&graphs[0], features),
            worktree.members(&graphs[1], features),
        ))
    }

    /** Measure the target function itself on both sides for a block-scope target rule, by loading
     * the files that mention the target's name and finding the unique definition on each side
     * Input
        - target: &str - qualified function target
     * Output
        - Result<(Units, Units), String> with one unit keyed by the target on each side
        - Error if the target is missing or ambiguous on either side
    */
    fn block_units(&mut self, kind: ItemKind, target: &str) -> Result<(Units, Units), String> {
        let mut graphs = [Graph::default(), Graph::default()];
        let name = function_name(target);
        self.load(&mut graphs, [vec![name.clone()], vec![name]])?;
        let before = graphs[0].get(find_start(&graphs[0], Side::Checkpoint, kind, target)?);
        let after = graphs[1].get(find_start(&graphs[1], Side::Worktree, kind, target)?);
        Ok((
            BTreeMap::from([(target.to_string(), before.features.clone())]),
            BTreeMap::from([(target.to_string(), after.features.clone())]),
        ))
    }

    /** Measure changed files on both sides for a file, folder, or all target rule, by reading each
     * path from the checkpoint and the worktree and recording file_features for the sides where
     * it exists
     * Input
        - paths: &[String] - changed paths
     * Output
        - Result<(Units, Units), String> checkpoint and worktree files
        - Error if a file cannot be read or a supported source file does not parse
    */
    fn file_units(&mut self, paths: &[String]) -> Result<(Units, Units), String> {
        let mut before = BTreeMap::new();
        let mut after = BTreeMap::new();
        for path in paths {
            for (side, units) in [
                (Side::Checkpoint, &mut before),
                (Side::Worktree, &mut after),
            ] {
                if let Some(source) = self.read(side, path)? {
                    let features = file_features(&source, path).map_err(|error| {
                        format!(
                            "{} source could not be parsed: {path}: {error}",
                            side.label()
                        )
                    })?;
                    units.insert(path.clone(), features);
                }
            }
        }
        Ok((before, after))
    }

    /** Trace the flow around the target in the checkpoint and in the worktree, by finding the
     * unique target definition on each side, then tracing downstream and upstream together one
     * level at a time, loading for both sides at once only the files that mention the names that
     * level needs
     * Input
        - target: &str - qualified function target
     * Output
        - Result<([Graph; 2], [Trace; 2]), String> loaded graphs and finished traces per side
        - Error if the target is missing or ambiguous, or a traced file does not parse
    */
    fn trace(&mut self, kind: ItemKind, target: &str) -> Result<([Graph; 2], [Trace; 2]), String> {
        let mut graphs = [Graph::default(), Graph::default()];
        let name = function_name(target);
        self.load(&mut graphs, [vec![name.clone()], vec![name]])?;
        let mut traces = [
            Trace::new(find_start(&graphs[0], Side::Checkpoint, kind, target)?),
            Trace::new(find_start(&graphs[1], Side::Worktree, kind, target)?),
        ];

        while traces.iter().any(|trace| !trace.is_done()) {
            let names = [
                traces[0].needed_names(&graphs[0]),
                traces[1].needed_names(&graphs[1]),
            ];
            self.load(&mut graphs, names)?;
            for (trace, graph) in traces.iter_mut().zip(&graphs) {
                trace.expand(graph);
            }
        }

        Ok((graphs, traces))
    }
}

/** The two sides in the order used by per-side arrays */
const SIDES: [Side; 2] = [Side::Checkpoint, Side::Worktree];

/** Breadth-first tracing state for one side of a flow
 * Fields
    - downstream: BTreeSet<DefinitionId> - the target and everything it calls, transitively
    - upstream: BTreeSet<DefinitionId> - the target and everything that calls it, transitively
    - down_frontier: Vec<DefinitionId> - downstream definitions whose calls are not yet followed
    - up_frontier: Vec<DefinitionId> - upstream definitions whose callers are not yet found
*/
struct Trace {
    downstream: BTreeSet<DefinitionId>,
    upstream: BTreeSet<DefinitionId>,
    down_frontier: Vec<DefinitionId>,
    up_frontier: Vec<DefinitionId>,
}

impl Trace {
    /** Start a trace at the target definition, which is both the first downstream and upstream
     * member
     * Input
        - start: DefinitionId - the target definition
     * Output
        - Trace
    */
    fn new(start: DefinitionId) -> Self {
        Self {
            downstream: BTreeSet::from([start]),
            upstream: BTreeSet::from([start]),
            down_frontier: vec![start],
            up_frontier: vec![start],
        }
    }

    /** Report whether both directions have finished
     * Input
        - None (uses self)
     * Output
        - bool, true when neither frontier has work left
    */
    fn is_done(&self) -> bool {
        self.down_frontier.is_empty() && self.up_frontier.is_empty()
    }

    /** List the names the next level needs loaded: the calls made by the downstream frontier and
     * the names of the upstream frontier
     * Input
        - graph: &Graph - this side's graph
     * Output
        - Vec<String> of names to search for
    */
    fn needed_names(&self, graph: &Graph) -> Vec<String> {
        self.down_frontier
            .iter()
            .flat_map(|id| graph.get(*id).calls.iter().cloned())
            .chain(
                self.up_frontier
                    .iter()
                    .map(|id| graph.get(*id).name.clone()),
            )
            .collect()
    }

    /** Advance both directions by one level, by following each downstream frontier call to every
     * definition with that name and each upstream frontier name to every definition calling it,
     * keeping only definitions not reached before as the next frontiers
     * Input
        - graph: &Graph - this side's graph, already loaded for needed_names
     * Output
        - None (updates self)
    */
    fn expand(&mut self, graph: &Graph) {
        let mut next = Vec::new();
        for id in &self.down_frontier {
            for call in &graph.get(*id).calls {
                for &callee in graph.by_name.get(call).into_iter().flatten() {
                    if self.downstream.insert(callee) {
                        next.push(callee);
                    }
                }
            }
        }
        self.down_frontier = next;

        let mut next = Vec::new();
        for id in &self.up_frontier {
            // Functions are reached by their callers; data, variables, classes, and interfaces
            // by the functions that mention them
            let definition = graph.get(*id);
            let upstream = match definition.kind {
                ItemKind::Function => &graph.callers_of,
                _ => &graph.users_of,
            };
            for &caller in upstream.get(&definition.name).into_iter().flatten() {
                if self.upstream.insert(caller) {
                    next.push(caller);
                }
            }
        }
        self.up_frontier = next;
    }

    /** Map every flow member, keyed by "path#qualified.name", to a value taken from its definition
     * Input
        - graph: &Graph - this side's graph
        - value: impl Fn(&Definition) -> T - what to record per member (canonical text or features)
     * Output
        - BTreeMap<String, T> of flow members
    */
    fn members<T>(&self, graph: &Graph, value: impl Fn(&Definition) -> T) -> BTreeMap<String, T> {
        self.downstream
            .union(&self.upstream)
            .map(|&id| {
                let definition = graph.get(id);
                (
                    format!("{}#{}", graph.files[id.0].0, definition.qualified),
                    value(definition),
                )
            })
            .collect()
    }
}

/** Find the unique target definition among a graph's loaded files
 * Input
    - graph: &Graph - graph loaded for the target's function name
    - side: Side - side the graph belongs to, used in messages
    - target: &str - qualified function target
 * Output
    - Result<DefinitionId, String>
    - Error if the target is missing or ambiguous
*/
fn find_start(
    graph: &Graph,
    side: Side,
    kind: ItemKind,
    target: &str,
) -> Result<DefinitionId, String> {
    let starts = graph
        .files
        .iter()
        .enumerate()
        .flat_map(|(file, (_, found))| {
            found
                .iter()
                .enumerate()
                .filter(|(_, definition)| is_item(definition, kind, target))
                .map(move |(index, _)| (file, index))
        })
        .collect::<Vec<_>>();
    match starts.as_slice() {
        [start] => Ok(*start),
        [] => Err(missing(side, kind, target)),
        _ => Err(ambiguous(side, kind, target, starts.len())),
    }
}

/** Hash the current contents of worktree files as Git blob ids, by streaming the paths to one
 * git hash-object --stdin-paths process (from a separate thread, so a full output pipe cannot
 * deadlock) and reading one id per path in order
 * Input
    - root: &PathBuf - repository root, used as the working directory
    - paths: &[String] - repository-relative paths
 * Output
    - Result<Vec<String>, String> of blob ids, one per path
    - Error if git fails or returns the wrong number of ids
*/
fn hash_files(root: &PathBuf, paths: &[String]) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin-paths"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to execute git: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("git hash-object has no stdin")?;
    let input = paths.join("\n") + "\n";
    let writer = thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output().map_err(io_error)?;
    writer
        .join()
        .map_err(|_| "git hash-object input thread panicked".to_string())?
        .map_err(io_error)?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().into());
    }
    let hashes = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(String::from)
        .collect::<Vec<_>>();
    if hashes.len() != paths.len() {
        return Err("git hash-object returned an unexpected number of ids".into());
    }
    Ok(hashes)
}

/** Return the function-name part of a target, by taking the last segment after "::" or "."
 * Input
    - target: &str - qualified function target
 * Output
    - String function name
*/
fn function_name(target: &str) -> String {
    target
        .rsplit(['.', ':'])
        .next()
        .unwrap_or(target)
        .to_string()
}

/** Check whether a definition is the requested item: the same kind, and a name that matches the
 * target by the resolver's rule
 * Input
    - definition: &Definition - candidate item
    - kind: ItemKind - requested kind
    - target: &str - qualified target
 * Output
    - bool, true if the definition is the requested item
*/
fn is_item(definition: &Definition, kind: ItemKind, target: &str) -> bool {
    definition.kind == kind && matches_target(&definition.name, &definition.qualified, target)
}

/** Build the missing-target message, keeping the wording used by block scope so the violation is
 * classified as target_not_found and owned by the right party
 * Input
    - side: Side - where the target was missing
    - target: &str - qualified function target
 * Output
    - String error message
*/
fn missing(side: Side, kind: ItemKind, target: &str) -> String {
    let place = match side {
        Side::Checkpoint => "checkpoint",
        Side::Worktree => "the worktree",
    };
    format!("protected {} {target} is missing from {place}", kind.noun())
}

/** Build the ambiguous-target message, keeping the wording used by block scope so the violation
 * is classified as duplicate_target
 * Input
    - side: Side - where the target was ambiguous
    - target: &str - qualified function target
    - count: usize - number of matches
 * Output
    - String error message
*/
fn ambiguous(side: Side, kind: ItemKind, target: &str, count: usize) -> String {
    format!(
        "protected {} {target} is ambiguous in {} ({count} matches)",
        kind.noun(),
        side.label()
    )
}

/** Compare the protected content of the checkpoint and the worktree, by walking the union of
 * their keys in order and recording every entry that was changed, added, or removed
 * Input
    - scope: Scope - scope being verified, used in the message
    - expected: &BTreeMap<String, String> - checkpoint content
    - actual: &BTreeMap<String, String> - worktree content
 * Output
    - Result<(), String>
    - Error listing up to REPORTED_CHANGES modified entries
*/
fn compare(
    scope: Scope,
    expected: &BTreeMap<String, String>,
    actual: &BTreeMap<String, String>,
) -> Result<(), String> {
    let keys = expected
        .keys()
        .chain(actual.keys())
        .collect::<BTreeSet<_>>();
    let changes = keys
        .into_iter()
        .filter_map(|key| match (expected.get(key), actual.get(key)) {
            (Some(before), Some(after)) if before == after => None,
            (Some(_), Some(_)) => Some(format!("{key} (changed)")),
            (Some(_), None) => Some(format!("{key} (removed)")),
            (None, _) => Some(format!("{key} (added)")),
        })
        .collect::<Vec<_>>();
    if changes.is_empty() {
        return Ok(());
    }
    let mut listed = changes
        .iter()
        .take(REPORTED_CHANGES)
        .cloned()
        .collect::<Vec<_>>();
    if changes.len() > REPORTED_CHANGES {
        listed.push(format!("and {} more", changes.len() - REPORTED_CHANGES));
    }
    Err(format!(
        "Protected {} scope was modified: {}",
        scope.name(),
        listed.join(", ")
    ))
}
