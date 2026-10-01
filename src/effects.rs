// Effect verification: after a tool call runs, Crane looks at what actually changed in the
// session's worktree (whatever the tool claimed), down to symbols, and verifies only what those
// changes can affect. Three levels: fast effect verification after every tool call, affected test
// execution for the code that changed, and full session validation at stop and finalization.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::authority::{AgentAction, FileChange, Operation, Proposed};
use crate::inventory::index::{list_files, read_blob, store_blobs, store_dirty};
use crate::inventory::outline::{language_of, outline, Symbol};
use crate::inventory::{discover, Options};
use crate::model::Violation;
use crate::repository::root;
use crate::resolver::matches_target;
use crate::scope::ScopeContext;
use crate::session::ContractSession;
use crate::util::{io_error, now_unix};
use crate::verify::{verify_contracts, Report};
use crate::zones::model::Autonomy;

/** Path to content id of every file in a worktree */
type Files = BTreeMap<String, String>;

/** Characters of test output kept in the journal */
const OUTPUT_TAIL: usize = 2000;

/** Default test timeout in seconds */
const TEST_TIMEOUT: u64 = 300;

/** Exclusive access to a session's observation files, released on drop
 * Fields
    - path: PathBuf - lock file
*/
struct Lock {
    path: PathBuf,
}

impl Lock {
    /** Take the lock, waiting up to ten seconds and taking over one older than two minutes
     * Input
        - directory: &Path - session directory
     * Output
        - Result<Lock, String>
    */
    fn acquire(directory: &Path) -> Result<Self, String> {
        let path = directory.join("observe.lock");
        for _ in 0..100 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(120));
                    if stale {
                        let _ = fs::remove_file(&path);
                    } else {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        Err("another hook is observing this session's effects".into())
    }
}

impl Drop for Lock {
    /** Release the lock
     * Input
        - None (uses self)
     * Output
        - None
    */
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/** Read a stored observation
 * Input
    - directory: &Path - session directory
    - name: &str - baseline or observed
 * Output
    - Option<Files>
*/
fn load(directory: &Path, name: &str) -> Option<Files> {
    let content = fs::read_to_string(directory.join(format!("{name}.json"))).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    value["files"]
        .as_object()?
        .iter()
        .map(|(path, blob)| Some((path.clone(), blob.as_str()?.to_string())))
        .collect()
}

/** Store an observation, through a temporary file renamed into place
 * Input
    - directory: &Path - session directory
    - name: &str - baseline or observed
    - files: &Files - observation
 * Output
    - Result<(), String>
*/
fn save(directory: &Path, name: &str, files: &Files) -> Result<(), String> {
    let path = directory.join(format!("{name}.json"));
    let temporary = directory.join(format!("{name}.json.{}", std::process::id()));
    fs::write(
        &temporary,
        json!({"at": now_unix(), "files": files}).to_string(),
    )
    .map_err(io_error)?;
    fs::rename(&temporary, &path).map_err(io_error)
}

/** Record the session's starting point: every file's content id, with changed and untracked
 * content written to Git's object store so it can be read back; done once, when the session is
 * created
 * Input
    - session: &ContractSession - new session
 * Output
    - Result<(), String>
*/
pub(crate) fn baseline(session: &ContractSession) -> Result<(), String> {
    let Some(directory) = session.directory() else {
        return Ok(());
    };
    let root = session.root_path();
    store_dirty(root)?;
    let files = list_files(root)?.0;
    save(&directory, "baseline", &files)?;
    save(&directory, "observed", &files)
}

/** Return the policy-relevant level a session needs to change a path without asking: the lowest
 * of its mode, its zones, and (in delegated mode) its task scope
 * Input
    - session: &ContractSession - session
    - path: &str - repository-relative path
    - exists: bool - whether the file exists now
 * Output
    - (Autonomy, Vec<String>) the level and why it is restricted
*/
fn required_level(session: &ContractSession, path: &str, exists: bool) -> (Autonomy, Vec<String>) {
    let governance = session.governance();
    let mut level = governance.autonomy;
    let mut reasons = Vec::new();
    if let Some(constraint) = governance.constraint(path) {
        let cap = constraint.autonomy.min(constraint.state.autonomy_cap());
        if cap <= Autonomy::Assisted {
            reasons.push(format!(
                "zones {} ({})",
                constraint.zones.join(", "),
                constraint.criticality.name()
            ));
        }
        level = level.min(cap);
    }
    if governance.autonomy == Autonomy::Delegated && !governance.in_scope(path, exists) {
        reasons.push("outside the task scope".into());
        level = level.min(Autonomy::Assisted);
    }
    (level, reasons)
}

/** The difference between two observations
 * Fields
    - added: Vec<String> - new paths
    - modified: Vec<String> - paths whose content changed
    - deleted: Vec<String> - removed paths
    - renamed: Vec<(String, String)> - removed and added paths with the same content
*/
struct Difference {
    added: Vec<String>,
    modified: Vec<String>,
    deleted: Vec<String>,
    renamed: Vec<(String, String)>,
}

impl Difference {
    /** Compare two observations, pairing a removed and an added path with the same content as a
     * rename
     * Input
        - before: &Files - earlier observation
        - after: &Files - later observation
     * Output
        - Difference
    */
    fn between(before: &Files, after: &Files) -> Self {
        let mut added = after
            .keys()
            .filter(|path| !before.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        let mut deleted = before
            .keys()
            .filter(|path| !after.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        let modified = after
            .iter()
            .filter(|(path, blob)| before.get(*path).is_some_and(|old| old != *blob))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let mut renamed = Vec::new();
        deleted.retain(|old| {
            let position = added
                .iter()
                .position(|new| after.get(new) == before.get(old));
            match position {
                Some(index) => {
                    renamed.push((old.clone(), added.remove(index)));
                    false
                }
                None => true,
            }
        });
        Self {
            added,
            modified,
            deleted,
            renamed,
        }
    }

    /** List every path touched, old and new names of renames included
     * Input
        - None (uses self)
     * Output
        - BTreeSet<String>
    */
    fn touched(&self) -> BTreeSet<String> {
        self.added
            .iter()
            .chain(&self.modified)
            .chain(&self.deleted)
            .cloned()
            .chain(
                self.renamed
                    .iter()
                    .flat_map(|(old, new)| [old.clone(), new.clone()]),
            )
            .collect()
    }

    /** Check whether nothing changed
     * Input
        - None (uses self)
     * Output
        - bool
    */
    fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.modified.is_empty()
            && self.deleted.is_empty()
            && self.renamed.is_empty()
    }
}

/** Read the symbols of one version of a file (stored content by id, or the worktree's file)
 * Input
    - root: &Path - worktree root
    - path: &str - repository-relative path
    - blob: Option<&str> - stored content id, None to read the worktree
 * Output
    - Vec<Symbol>, empty for files without a grammar or unreadable content
*/
fn symbols(root: &Path, path: &str, blob: Option<&str>) -> Vec<Symbol> {
    let Some(language) = language_of(path) else {
        return Vec::new();
    };
    let bytes = match blob {
        Some(blob) => read_blob(root, blob),
        None => fs::read(root.join(path)).map_err(io_error),
    };
    bytes
        .map(|bytes| outline(&bytes, path, language).symbols)
        .unwrap_or_default()
}

/** Key the symbols of a file by qualified name and occurrence, with their content digests
 * Input
    - symbols: Vec<Symbol> - symbols in source order
 * Output
    - BTreeMap<String, (String, String)> key to (kind, digest)
*/
fn keyed(symbols: Vec<Symbol>) -> BTreeMap<String, (String, String)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    symbols
        .into_iter()
        .map(|symbol| {
            let count = counts.entry(symbol.qualified.clone()).or_default();
            let key = if *count == 0 {
                symbol.qualified.clone()
            } else {
                format!("{}~{}", symbol.qualified, *count + 1)
            };
            *count += 1;
            (key, (symbol.kind.name().to_string(), symbol.digest))
        })
        .collect()
}

/** List the symbol-level changes of a difference: symbols added, modified, or removed in changed
 * files (by content digest), every symbol of added and deleted files, and the symbols of renamed
 * files as moved
 * Input
    - root: &Path - worktree root
    - before: &Files - earlier observation
    - difference: &Difference - file-level changes
 * Output
    - Vec<Value> one entry per changed symbol: {symbol, kind, change}
*/
fn symbol_changes(root: &Path, before: &Files, difference: &Difference) -> Vec<Value> {
    let mut changes = Vec::new();
    let mut push = |path: &str, key: &str, kind: &str, change: &str| {
        changes.push(json!({"symbol": format!("{path}#{key}"), "kind": kind, "change": change}));
    };
    for path in &difference.modified {
        let old = keyed(symbols(root, path, before.get(path).map(String::as_str)));
        let new = keyed(symbols(root, path, None));
        for (key, (kind, digest)) in &new {
            match old.get(key) {
                None => push(path, key, kind, "added"),
                Some((_, previous)) if previous != digest => push(path, key, kind, "modified"),
                _ => {}
            }
        }
        for (key, (kind, _)) in &old {
            if !new.contains_key(key) {
                push(path, key, kind, "removed");
            }
        }
    }
    for path in &difference.added {
        for (key, (kind, _)) in keyed(symbols(root, path, None)) {
            push(path, &key, &kind, "added");
        }
    }
    for path in &difference.deleted {
        for (key, (kind, _)) in keyed(symbols(root, path, before.get(path).map(String::as_str))) {
            push(path, &key, &kind, "removed");
        }
    }
    for (old, new) in &difference.renamed {
        for (key, (kind, _)) in keyed(symbols(root, new, None)) {
            changes.push(json!({"symbol": format!("{new}#{key}"), "kind": kind, "change": "moved", "from": format!("{old}#{key}")}));
        }
    }
    // A type changes whenever one of its members does; report the innermost symbols only
    let names = changes
        .iter()
        .filter_map(|change| change["symbol"].as_str().map(String::from))
        .collect::<Vec<_>>();
    changes.retain(|change| {
        let symbol = change["symbol"].as_str().unwrap_or_default();
        change["change"] != "modified"
            || !names
                .iter()
                .any(|other| other.starts_with(&format!("{symbol}.")))
    });
    changes
}

/** Describe an effect and judge it against the session's authority: the zones it touched, and
 * every changed path the session needed approval for (restricted or critical zones, or outside the
 * task scope in delegated mode) that no earlier authorized or approval-gated tool call named, which
 * becomes an unauthorized_effect violation the agent must revert
 * Input
    - session: &ContractSession - session
    - before: &Files - earlier observation
    - difference: &Difference - file-level changes
 * Output
    - (Value, Vec<Violation>) effect description and violations
*/
fn describe(
    session: &ContractSession,
    before: &Files,
    difference: &Difference,
) -> (Value, Vec<Violation>) {
    let root = session.root_path();
    let authorized = session.activity().files;
    let governance = session.governance();
    let mut zones = BTreeSet::new();
    let mut violations = Vec::new();
    for path in difference.touched() {
        if let Some(constraint) = governance.constraint(&path) {
            zones.extend(constraint.zones.iter().cloned());
        }
        let exists = root.join(&path).exists();
        let (level, reasons) = required_level(session, &path, exists);
        if level <= Autonomy::Assisted
            && (level == Autonomy::Observe || !authorized.contains(&path))
        {
            violations.push(Violation {
                policy_id: "crane".into(),
                rule: "session".into(),
                target: path.clone(),
                checkpoint: String::new(),
                violation_type: "unauthorized_effect".into(),
                message: format!(
                    "{path} changed without authorization ({}); no tool call the session may run without approval named it, so revert it in the worktree",
                    reasons.join("; ")
                ),
            });
        }
    }
    let effect = json!({
        "files": {
            "added": difference.added,
            "modified": difference.modified,
            "deleted": difference.deleted,
            "renamed": difference.renamed.iter().map(|(from, to)| json!({"from": from, "to": to})).collect::<Vec<_>>(),
        },
        "symbols": symbol_changes(root, before, difference),
        "zones_touched": zones,
    });
    (effect, violations)
}

/** Level 1, fast effect verification after a tool call ran: compare the worktree with the last
 * observation (catching changes made by shell commands, scripts, generators, renames, and
 * deletions whatever the tool claimed), describe the files and symbols changed and the zones
 * touched, flag unauthorized effects, and verify only the contract clauses the changed files (and
 * the tool's own files) can affect (every clause when there is no earlier observation to compare
 * with); reads change nothing and are skipped
 * Input
    - session: &ContractSession - session
    - action: &AgentAction - the tool call that ran
    - protected: &[&str] - provider-specific protected settings files
 * Output
    - Result<(Report, Value), String> verification report and the effect journal entry
*/
pub(crate) fn observe(
    session: &ContractSession,
    action: &AgentAction,
    protected: &[&str],
) -> Result<(Report, Value), String> {
    let root = session.root_path().clone();
    let directory = session.directory();
    let _lock = match &directory {
        Some(directory) => Some(Lock::acquire(directory)?),
        None => None,
    };
    let before = directory
        .as_deref()
        .and_then(|directory| load(directory, "observed"));
    let now = list_files(&root)?.0;
    // Without an earlier observation (a transient session, or one older than effect tracking)
    // nothing can be diffed, so every clause is verified instead
    let observed = before.is_some();
    let before = before.unwrap_or_else(|| now.clone());
    let difference = Difference::between(&before, &now);
    let stored = difference
        .added
        .iter()
        .chain(&difference.modified)
        .cloned()
        .collect::<Vec<_>>();
    store_blobs(&root, &stored)?;
    let (mut effect, unauthorized) = describe(session, &before, &difference);
    let mut changed = difference.touched();
    changed.extend(action.files.iter().map(|change| change.path.clone()));
    let probe = AgentAction {
        tool: action.tool.clone(),
        operation: if changed.is_empty() {
            Operation::Read
        } else {
            Operation::Write
        },
        files: changed
            .iter()
            .map(|path| FileChange {
                path: root.join(path).to_string_lossy().into_owned(),
                proposed: Proposed::Delete,
            })
            .collect(),
        command: None,
        arguments: Vec::new(),
        digest: String::new(),
    };
    let mut relevant = if observed {
        session.runtime(protected).relevant(&probe)
    } else {
        vec![true; session.contracts().clauses().count()]
    };
    // A clause is also affected when a symbol it names changed, appeared, moved, or disappeared,
    // even if the changed file no longer mentions it (a renamed method)
    let named = effect["symbols"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|change| [change["symbol"].as_str(), change["from"].as_str()])
        .flatten()
        .filter_map(|symbol| {
            symbol
                .split_once('#')
                .map(|(_, key)| key.split('~').next().unwrap_or(key).to_string())
        })
        .collect::<BTreeSet<_>>();
    for (index, _, clause) in session.contracts().clauses() {
        let hit = named.iter().any(|qualified| {
            let name = qualified.rsplit('.').next().unwrap_or(qualified);
            matches_target(name, qualified, &clause.target)
        });
        if hit {
            if let Some(slot) = relevant.get_mut(index) {
                *slot = true;
            }
        }
    }
    let mut report = verify_contracts(session.contracts(), &mut ScopeContext::new(), &|index| {
        relevant.get(index).copied().unwrap_or(true)
    });
    report.violations.extend(unauthorized);
    effect["clauses_checked"] = json!(relevant.iter().filter(|selected| **selected).count());
    effect["clauses_total"] = json!(relevant.len());
    effect["violations"] = json!(report
        .violations
        .iter()
        .map(|violation| violation.violation_type.clone())
        .collect::<Vec<_>>());
    effect["targets_satisfied"] = json!(report
        .passes
        .iter()
        .filter(|pass| pass.rule == "target")
        .map(|pass| pass.target.clone())
        .collect::<Vec<_>>());
    if let Some(directory) = &directory {
        if !difference.is_empty() {
            save(directory, "observed", &now)?;
        }
    }
    Ok((report, effect))
}

/** The cumulative effect of the whole session: the worktree now compared with the session's
 * baseline (a change that was reverted leaves nothing)
 * Input
    - session: &ContractSession - session
 * Output
    - Result<(Value, BTreeSet<String>, Vec<Violation>), String> effect, changed paths, and
      unauthorized effects
*/
pub(crate) fn cumulative(
    session: &ContractSession,
) -> Result<(Value, BTreeSet<String>, Vec<Violation>), String> {
    let root = session.root_path();
    let now = list_files(root)?.0;
    let before = session
        .directory()
        .and_then(|directory| load(&directory, "baseline"))
        .unwrap_or_else(|| now.clone());
    let difference = Difference::between(&before, &now);
    let (effect, violations) = describe(session, &before, &difference);
    Ok((effect, difference.touched(), violations))
}

/** The affected-test configuration in .crane/testing.json: per language, the command that runs
 * test files ("{files}" stands for them), and a timeout; agents cannot edit it, since Crane runs
 * these commands
 * Input
    - None
 * Output
    - Result<Value, String> the configuration, Null when there is none
*/
fn testing_config() -> Result<Value, String> {
    match fs::read_to_string(root()?.join("testing.json")) {
        Ok(content) => {
            serde_json::from_str(&content).map_err(|error| format!(".crane/testing.json: {error}"))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Value::Null),
        Err(error) => Err(io_error(error)),
    }
}

/** Run one test command with a timeout, keeping the end of its output
 * Input
    - root: &Path - worktree root (the working directory)
    - command: &[String] - program and arguments
    - timeout: u64 - seconds
 * Output
    - Value {status: passed|failed|timed_out|error, exit, output}
*/
fn run(root: &Path, command: &[String], timeout: u64) -> Value {
    let Some((program, arguments)) = command.split_first() else {
        return json!({"status": "error", "output": "empty test command"});
    };
    let child = Command::new(program)
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            return json!({"status": "error", "output": format!("cannot run {program}: {error}")})
        }
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let readers = [
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(stream) = stdout.as_mut() {
                let _ = stream.read_to_string(&mut text);
            }
            text
        }),
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(stream) = stderr.as_mut() {
                let _ = stream.read_to_string(&mut text);
            }
            text
        }),
    ];
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() > Duration::from_secs(timeout) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break None,
        }
    };
    let output = readers
        .into_iter()
        .filter_map(|reader| reader.join().ok())
        .collect::<String>();
    let tail = output
        .chars()
        .rev()
        .take(OUTPUT_TAIL)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    match status {
        Some(status) if status.success() => {
            json!({"status": "passed", "exit": status.code(), "output": tail})
        }
        Some(status) => json!({"status": "failed", "exit": status.code(), "output": tail}),
        None => json!({"status": "timed_out", "exit": null, "output": tail}),
    }
}

/** Level 2, affected test execution: find the tests that exercise the changed files (through the
 * inventory's test relationships, plus changed test files themselves) and run only those, grouped
 * by language with the configured command; languages without a command are reported, not run
 * Input
    - session: &ContractSession - session (its worktree is the working directory)
    - changed: &BTreeSet<String> - changed paths
 * Output
    - Result<Vec<Value>, String> one result per language: {language, files, command, status, exit,
      output}
*/
pub(crate) fn affected_tests(
    session: &ContractSession,
    changed: &BTreeSet<String>,
) -> Result<Vec<Value>, String> {
    if changed.is_empty() {
        return Ok(Vec::new());
    }
    let config = testing_config()?;
    let inventory = discover(&Options { full: false })?;
    let snapshot = &inventory.snapshot;
    let graph = &inventory.graph;
    let mut tests: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    let index = snapshot
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    for path in changed {
        let Some(file) = index.get(path.as_str()).copied() else {
            continue;
        };
        let mut found = Vec::new();
        if graph.files[file].test {
            found.push(file);
        }
        found.extend(graph.files[file].tested_by.iter().copied());
        for entity in graph.entities.iter().filter(|entity| entity.file == file) {
            for test in &entity.tested_by {
                let target = test.strip_prefix("file:").map(String::from).or_else(|| {
                    graph
                        .entities
                        .iter()
                        .find(|other| &other.id == test)
                        .map(|other| snapshot.files[other.file].path.clone())
                });
                if let Some(target) = target.and_then(|path| index.get(path.as_str()).copied()) {
                    found.push(target);
                }
            }
        }
        for test in found {
            if let Some(language) = snapshot.files[test].language {
                tests
                    .entry(language.name)
                    .or_default()
                    .insert(snapshot.files[test].path.clone());
            }
        }
    }
    let timeout = config["timeout_seconds"].as_u64().unwrap_or(TEST_TIMEOUT);
    let root = session.root_path();
    Ok(tests
        .into_iter()
        .map(|(language, files)| {
            let template = config["commands"][language].as_array().map(|parts| {
                parts.iter().filter_map(|part| part.as_str().map(String::from)).collect::<Vec<_>>()
            });
            let mut result = match &template {
                None => json!({"status": "not_configured", "output": format!("no command for {language} in .crane/testing.json")}),
                Some(template) => {
                    let command = template
                        .iter()
                        .flat_map(|part| if part == "{files}" { files.iter().cloned().collect::<Vec<_>>() } else { vec![part.clone()] })
                        .collect::<Vec<_>>();
                    let mut result = run(root, &command, timeout);
                    result["command"] = json!(command);
                    result
                }
            };
            result["language"] = json!(language);
            result["files"] = json!(files);
            result
        })
        .collect())
}

/** Level 3, full session validation (at stop and finalization): the full reconciliation of the
 * bound contract, the cumulative effect since the session started (with unauthorized effects), and
 * the affected tests of everything the session changed; failing tests and unauthorized effects
 * fail the outcome and are the agent's to repair
 * Input
    - session: &ContractSession - session
 * Output
    - Result<(Report, Value), String> report and attestation
*/
pub(crate) fn validate(session: &ContractSession) -> Result<(Report, Value), String> {
    match workspace_of(session) {
        Some(path) => within(&path, || validate_here(session)),
        None => validate_here(session),
    }
}

/** Return a session's isolated worktree when it belongs to this repository's .crane; any other
 * session is judged from the current directory, so a session copied from another repository is
 * still found displaced
 * Input
    - session: &ContractSession - session
 * Output
    - Option<PathBuf>
*/
pub(crate) fn workspace_of(session: &ContractSession) -> Option<PathBuf> {
    let worktrees = root().ok()?.join("runtime").join("worktrees");
    let path = session.root_path();
    let inside = |base: &Path| path.starts_with(base);
    (inside(&worktrees)
        || worktrees
            .canonicalize()
            .ok()
            .is_some_and(|base| path.canonicalize().is_ok_and(|path| path.starts_with(base))))
    .then(|| path.clone())
}

/** Run a closure with the working directory set to a session's worktree (verification and
 * discovery read the repository around the working directory), restoring it afterwards
 * Input
    - root: &Path - worktree root
    - work: impl FnOnce() -> Result<T, String> - work to run there
 * Output
    - Result<T, String>
*/
pub(crate) fn within<T>(
    root: &Path,
    work: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let previous = std::env::current_dir().map_err(io_error)?;
    std::env::set_current_dir(root).map_err(io_error)?;
    let result = work();
    std::env::set_current_dir(previous).map_err(io_error)?;
    result
}

/** Validate a session from inside its worktree (see validate)
 * Input
    - session: &ContractSession - session
 * Output
    - Result<(Report, Value), String>
*/
fn validate_here(session: &ContractSession) -> Result<(Report, Value), String> {
    let (effect, changed, unauthorized) = cumulative(session)?;
    let tests = affected_tests(session, &changed)?;
    let (mut report, mut attestation) = session.reconcile();
    report.violations.extend(unauthorized);
    for result in tests.iter().filter(|result| {
        matches!(
            result["status"].as_str(),
            Some("failed" | "timed_out" | "error")
        )
    }) {
        report.violations.push(Violation {
            policy_id: "crane".into(),
            rule: "tests".into(),
            target: result["files"]
                .as_array()
                .map(|files| {
                    files
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
            checkpoint: String::new(),
            violation_type: "tests_failed".into(),
            message: format!(
                "affected {} tests {}; fix the code in the worktree: {}",
                result["language"].as_str().unwrap_or_default(),
                result["status"].as_str().unwrap_or_default(),
                result["output"]
                    .as_str()
                    .unwrap_or_default()
                    .lines()
                    .rev()
                    .take(5)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(" | ")
            ),
        });
    }
    attestation["effects"] = effect;
    attestation["tests"] = json!(tests);
    if !report.violations.is_empty() {
        attestation["final_status"] = json!("FAIL");
    }
    Ok((report, attestation))
}
