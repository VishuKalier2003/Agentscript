use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;

use serde_json::{json, Map, Value};

use super::outline::{language_of, outline, LanguageSpec, Outline, EXTRACTOR_VERSION};
use crate::scope::hash_files;
use crate::util::io_error;

/** Version of the inventory cache layout; a cache with another layout is ignored */
const CACHE_FORMAT: u64 = 1;

/** Path prefixes of tool metadata, never inventoried */
const EXCLUDED_PREFIXES: &[&str] = &[".crane/", ".claude/", ".codex/", ".git/"];

/** Path segments of vendored or generated code, never inventoried */
const EXCLUDED_SEGMENTS: &[&str] = &["node_modules", "vendor", "third_party"];

/** One file of the repository as discovery sees it
 * Fields
    - path: String - repository-relative path with "/" separators
    - blob: String - Git object id of its current content
    - language: Option<&'static LanguageSpec> - its language, None for unknown files
    - outline: Arc<Outline> - what its content holds (shared with the cache)
*/
pub(crate) struct SourceFile {
    pub(crate) path: String,
    pub(crate) blob: String,
    pub(crate) language: Option<&'static LanguageSpec>,
    pub(crate) outline: Arc<Outline>,
}

/** The repository's files with their outlines, and what changed since the cached inventory
 * Fields
    - files: Vec<SourceFile> - every inventoried file, sorted by path
    - excluded: usize - files skipped as metadata or vendored code
    - parsed: usize - files whose outline was extracted in this run
    - reused: usize - files whose outline came from the cache
    - changed: BTreeSet<String> - paths added, removed, or changed since the cached inventory
      (every path when there is no usable cache)
    - previous: HashMap<String, Arc<Outline>> - the cached outline of each changed path that
      existed before
    - links: Option<BTreeMap<String, Vec<String>>> - call links of the cached inventory by
      location key, None when there is no usable cache
*/
pub(crate) struct Snapshot {
    pub(crate) files: Vec<SourceFile>,
    pub(crate) excluded: usize,
    pub(crate) parsed: usize,
    pub(crate) reused: usize,
    pub(crate) changed: BTreeSet<String>,
    pub(crate) previous: HashMap<String, Arc<Outline>>,
    pub(crate) links: Option<BTreeMap<String, Vec<String>>>,
}

/** The cached inventory read from disk
 * Fields
    - files: BTreeMap<String, String> - path to blob of the cached run
    - outlines: HashMap<String, Arc<Outline>> - outlines by "blob:language"
    - links: BTreeMap<String, Vec<String>> - call links by location key
*/
struct Cache {
    files: BTreeMap<String, String>,
    outlines: HashMap<String, Arc<Outline>>,
    links: BTreeMap<String, Vec<String>>,
}

/** Run git in the repository root and return its raw output
 * Input
    - root: &Path - repository root
    - args: &[&str] - git arguments
 * Output
    - Result<Vec<u8>, String>
    - Error if git fails
*/
fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| format!("failed to execute git: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().into());
    }
    Ok(output.stdout)
}

/** Split NUL-separated git output into strings
 * Input
    - bytes: &[u8] - output of a -z git command
 * Output
    - Vec<String>
*/
fn entries(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .collect()
}

/** Check whether a path is tool metadata or vendored code
 * Input
    - path: &str - repository-relative path
 * Output
    - bool
*/
fn excluded(path: &str) -> bool {
    EXCLUDED_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
        || path
            .split('/')
            .any(|segment| EXCLUDED_SEGMENTS.contains(&segment))
}

/** List the repository's files with the Git object id of their current content, the way Git's
 * own status does it: tracked files take their id from the index unless git diff-files reports
 * them changed, and only changed or untracked files are hashed (deleted files are dropped,
 * submodules and symlinks skipped, .gitignore honored)
 * Input
    - root: &Path - repository root
 * Output
    - Result<(BTreeMap<String, String>, usize), String> path to blob, and the excluded count
    - Error if git fails
*/
pub(crate) fn list_files(root: &Path) -> Result<(BTreeMap<String, String>, usize), String> {
    let mut known = BTreeMap::new();
    let mut unhashed = BTreeSet::new();
    for entry in entries(&git_bytes(root, &["ls-files", "-s", "-z"])?) {
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let fields = meta.split(' ').collect::<Vec<_>>();
        if fields.len() != 3 || matches!(fields[0], "160000" | "120000") {
            continue;
        }
        if fields[2] == "0" {
            known.insert(path.to_string(), fields[1].to_string());
        } else {
            unhashed.insert(path.to_string()); // a merge conflict: hash what is on disk
        }
    }
    for path in entries(&git_bytes(root, &["diff-files", "--name-only", "-z"])?) {
        known.remove(&path);
        if root.join(&path).is_file() {
            unhashed.insert(path);
        } else {
            unhashed.remove(&path);
        }
    }
    let others = git_bytes(root, &["ls-files", "-z", "--others", "--exclude-standard"])?;
    unhashed.extend(entries(&others));
    let mut skipped = 0;
    known.retain(|path, _| {
        let keep = !excluded(path);
        skipped += usize::from(!keep);
        keep
    });
    let hashable = unhashed
        .into_iter()
        .filter(|path| {
            let keep = !excluded(path) && root.join(path).is_file();
            skipped += usize::from(!keep && excluded(path));
            keep
        })
        .collect::<Vec<_>>();
    let hashes = hash_files(&root.to_path_buf(), &hashable)?;
    known.extend(hashable.into_iter().zip(hashes));
    Ok((known, skipped))
}

/** Write files' current content into Git's object store (git hash-object -w), so the exact
 * version observed now can be read back later even if it was never committed
 * Input
    - root: &Path - repository root
    - paths: &[String] - repository-relative paths of existing files
 * Output
    - Result<(), String>
    - Error if git fails
*/
pub(crate) fn store_blobs(root: &Path, paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin-paths"])
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to execute git: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("git hash-object has no stdin")?;
    let input = paths.join("\n") + "\n";
    let writer = thread::spawn(move || std::io::Write::write_all(&mut stdin, input.as_bytes()));
    let output = child.wait_with_output().map_err(io_error)?;
    writer
        .join()
        .map_err(|_| "git hash-object input thread panicked".to_string())?
        .map_err(io_error)?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().into());
    }
    Ok(())
}

/** Write every file whose content differs from the index (changed or untracked) into Git's object
 * store, so a baseline of the whole worktree can be read back later; clean tracked files are
 * already stored
 * Input
    - root: &Path - repository root
 * Output
    - Result<(), String>
    - Error if git fails
*/
pub(crate) fn store_dirty(root: &Path) -> Result<(), String> {
    let mut paths = entries(&git_bytes(root, &["diff-files", "--name-only", "-z"])?);
    paths.extend(entries(&git_bytes(
        root,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?));
    paths.retain(|path| !excluded(path) && root.join(path).is_file());
    store_blobs(root, &paths)
}

/** Read stored content by object id
 * Input
    - root: &Path - repository root
    - blob: &str - Git object id
 * Output
    - Result<Vec<u8>, String>
    - Error if the object is not stored
*/
pub(crate) fn read_blob(root: &Path, blob: &str) -> Result<Vec<u8>, String> {
    git_bytes(root, &["cat-file", "blob", blob])
}

/** Return the cache key of an outline: the content id plus the language reading it
 * Input
    - blob: &str - Git object id
    - language: &LanguageSpec - language
 * Output
    - String
*/
fn cache_key(blob: &str, language: &LanguageSpec) -> String {
    format!("{blob}:{}", language.name)
}

/** Load the cached inventory, ignoring a missing, unreadable, or outdated cache (the run then
 * starts from scratch) and dropping individual malformed outlines (they are extracted again)
 * Input
    - directory: &Path - cache directory
 * Output
    - Option<Cache>
*/
fn load_cache(directory: &Path) -> Option<Cache> {
    let content = fs::read_to_string(directory.join("index.json")).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    if value["cache_format"].as_u64() != Some(CACHE_FORMAT)
        || value["extractor"].as_u64() != Some(EXTRACTOR_VERSION)
    {
        return None;
    }
    let files = value["files"]
        .as_object()?
        .iter()
        .map(|(path, blob)| Some((path.clone(), blob.as_str()?.to_string())))
        .collect::<Option<BTreeMap<_, _>>>()?;
    let outlines = value["outlines"]
        .as_object()?
        .iter()
        .filter_map(|(key, outline)| Some((key.clone(), Arc::new(Outline::from_json(outline)?))))
        .collect();
    let links = value["links"]
        .as_object()?
        .iter()
        .map(|(location, targets)| {
            let targets = targets
                .as_array()?
                .iter()
                .map(|target| target.as_str().map(String::from))
                .collect::<Option<Vec<_>>>()?;
            Some((location.clone(), targets))
        })
        .collect::<Option<BTreeMap<_, _>>>()?;
    Some(Cache {
        files,
        outlines,
        links,
    })
}

/** Extract outlines for files the cache does not hold, by reading and parsing them on up to 8
 * threads (each thread keeps its own parser per grammar, as the resolver's parser cache is per
 * thread)
 * Input
    - root: &Path - repository root
    - work: Vec<(String, &'static LanguageSpec)> - paths and languages to extract
 * Output
    - Vec<Arc<Outline>> aligned with work
*/
fn extract_all(root: &Path, work: Vec<(String, &'static LanguageSpec)>) -> Vec<Arc<Outline>> {
    if work.is_empty() {
        return Vec::new();
    }
    let threads = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .clamp(1, 8)
        .min(work.len());
    let chunk = work.len().div_ceil(threads);
    thread::scope(|scope| {
        let handles = work
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|(path, language)| {
                            let outline = match fs::read(root.join(path)) {
                                Ok(bytes) => outline(&bytes, path, language),
                                Err(error) => Outline {
                                    lines: 0,
                                    status: format!("skipped: {}", io_error(error)),
                                    package: None,
                                    imports: Vec::new(),
                                    calls: Vec::new(),
                                    symbols: Vec::new(),
                                },
                            };
                            Arc::new(outline)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
}

/** Take a snapshot of the repository for discovery: list files with their content ids, reuse the
 * cached outline of every content id seen before, extract only the rest, and work out which paths
 * changed since the cached inventory (with their previous outlines) so the graph can relink only
 * the affected region
 * Input
    - root: &Path - repository root
    - cache: Option<&Path> - cache directory, None to work without a cache
    - full: bool - ignore the cache and extract everything
 * Output
    - Result<Snapshot, String>
    - Error if git fails
*/
pub(crate) fn snapshot(root: &Path, cache: Option<&Path>, full: bool) -> Result<Snapshot, String> {
    let (listing, excluded) = list_files(root)?;
    let cached = if full {
        None
    } else {
        cache.and_then(load_cache)
    };
    let unknown = Arc::new(Outline {
        lines: 0,
        status: "unknown".into(),
        package: None,
        imports: Vec::new(),
        calls: Vec::new(),
        symbols: Vec::new(),
    });
    let mut files = Vec::with_capacity(listing.len());
    let mut work = Vec::new();
    let mut slots = Vec::new();
    for (path, blob) in &listing {
        let language = language_of(path);
        let reused = language.and_then(|language| {
            cached
                .as_ref()?
                .outlines
                .get(&cache_key(blob, language))
                .cloned()
        });
        let outline = match (language, reused) {
            (None, _) => unknown.clone(),
            (Some(_), Some(outline)) => outline,
            (Some(language), None) => {
                slots.push(files.len());
                work.push((path.clone(), language));
                unknown.clone()
            }
        };
        files.push(SourceFile {
            path: path.clone(),
            blob: blob.clone(),
            language,
            outline,
        });
    }
    let parsed = work.len();
    for (slot, outline) in slots.into_iter().zip(extract_all(root, work)) {
        files[slot].outline = outline;
    }
    let reused = files.iter().filter(|file| file.language.is_some()).count() - parsed;
    let (changed, previous, links) = match cached {
        None => (listing.keys().cloned().collect(), HashMap::new(), None),
        Some(cache) => {
            let mut changed = BTreeSet::new();
            let mut previous = HashMap::new();
            for (path, blob) in &cache.files {
                if listing.get(path) != Some(blob) {
                    changed.insert(path.clone());
                    let old = language_of(path)
                        .and_then(|language| cache.outlines.get(&cache_key(blob, language)));
                    if let Some(old) = old {
                        previous.insert(path.clone(), old.clone());
                    }
                }
            }
            changed.extend(
                listing
                    .keys()
                    .filter(|path| !cache.files.contains_key(*path))
                    .cloned(),
            );
            (changed, previous, Some(cache.links))
        }
    };
    Ok(Snapshot {
        files,
        excluded,
        parsed,
        reused,
        changed,
        previous,
        links,
    })
}

/** Write the cache for the next run, keeping only outlines of current content (older content
 * ids are pruned), through a temporary file renamed into place; the runtime directory gets a
 * .gitignore so the cache is never committed
 * Input
    - directory: &Path - cache directory, .crane/runtime/inventory
    - snapshot: &Snapshot - this run's files
    - links: &BTreeMap<String, Vec<String>> - this run's call links by location key
 * Output
    - Result<PathBuf, String> the cache file
    - Error if the cache cannot be written
*/
pub(crate) fn save(
    directory: &Path,
    snapshot: &Snapshot,
    links: &BTreeMap<String, Vec<String>>,
) -> Result<PathBuf, String> {
    fs::create_dir_all(directory).map_err(io_error)?;
    if let Some(runtime) = directory.parent() {
        let ignore = runtime.join(".gitignore");
        if !ignore.exists() {
            fs::write(ignore, "*\n").map_err(io_error)?;
        }
    }
    let mut files = Map::new();
    let mut outlines = Map::new();
    for file in &snapshot.files {
        files.insert(file.path.clone(), json!(file.blob));
        if let Some(language) = file.language {
            outlines
                .entry(cache_key(&file.blob, language))
                .or_insert_with(|| file.outline.to_json());
        }
    }
    let document = json!({
        "cache_format": CACHE_FORMAT,
        "extractor": EXTRACTOR_VERSION,
        "files": files,
        "outlines": outlines,
        "links": links,
    });
    let path = directory.join("index.json");
    let temporary = directory.join(format!("index.json.{}", std::process::id()));
    fs::write(&temporary, document.to_string()).map_err(io_error)?;
    fs::rename(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        io_error(error)
    })?;
    Ok(path)
}
