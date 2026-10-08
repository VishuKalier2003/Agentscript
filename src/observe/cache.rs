// In-memory memoization for the read-only observability layer, so a long-running server answers
// large repositories without re-reading every journal and record on every request. An entry is
// reused while the files it was computed from are unchanged (size and modification time of each);
// those files are checked again at most every few seconds, so a warm request over tens of
// thousands of tasks touches no file at all, and an active session is recomputed at least once a
// minute, since parts of it depend on the time (authority lapses, expiry). Nothing is ever
// written: the cache lives only in the process, so the records under .crane stay the single source
// of truth, and a page is at most FRESH seconds behind them. A long-running server also
// revalidates every entry in the background (see observe::refresh), so requests rarely touch the
// file system at all.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use super::Run;

/** Seconds an active session's projection is reused even when its files did not change */
const ACTIVE_TTL: Duration = Duration::from_secs(60);

/** How long a checked entry or listing is trusted before its files are checked again */
const FRESH: Duration = Duration::from_secs(5);

/** What a cached value was computed from: each file's path, size, and modification time */
type Fingerprint = Vec<(PathBuf, u64, Option<SystemTime>)>;

/** One cached value
 * Fields
    - fingerprint: Fingerprint - its source files when computed
    - computed: Instant - when it was computed
    - checked: Instant - when its source files were last found unchanged
    - value: Arc<T> - the value
*/
struct Entry<T> {
    fingerprint: Fingerprint,
    computed: Instant,
    checked: Instant,
    value: Arc<T>,
}

/** Cached sessions, by session directory */
static RUNS: OnceLock<Mutex<HashMap<PathBuf, Entry<Run>>>> = OnceLock::new();

/** Cached JSON values (task records, task contracts), by key */
static VALUES: OnceLock<Mutex<HashMap<String, Entry<Value>>>> = OnceLock::new();

/** A cached directory listing: when it was listed, and the names in it */
type Listing = (Instant, Arc<Vec<String>>);

/** Cached directory listings (session ids, task ids), by key */
static LISTINGS: OnceLock<Mutex<HashMap<String, Listing>>> = OnceLock::new();

/** Fingerprint files and directories: a file by its size and time, a directory by every file
 * directly in it (a missing path counts as absent)
 * Input
    - paths: &[PathBuf] - files or directories
 * Output
    - Fingerprint
*/
pub(super) fn fingerprint(paths: &[PathBuf]) -> Fingerprint {
    let mut found = Vec::new();
    let mut note = |path: &Path| {
        if let Ok(metadata) = fs::metadata(path) {
            found.push((path.to_path_buf(), metadata.len(), metadata.modified().ok()));
        }
    };
    for path in paths {
        if path.is_dir() {
            let mut entries = fs::read_dir(path)
                .map(|entries| {
                    entries
                        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            entries.sort();
            for entry in entries.iter().filter(|entry| entry.is_file()) {
                note(entry);
            }
        } else {
            note(path);
        }
    }
    found
}

/** What to do with a cached entry, decided under the lock without touching the file system
 * Variants
    - Reuse(Arc<T>): it was checked recently enough
    - Compare(Fingerprint, Arc<T>): compare its files with the fingerprint it was computed from
    - Compute: there is none, or it outlived its lifetime
*/
enum Check<T> {
    Reuse(Arc<T>),
    Compare(Fingerprint, Arc<T>),
    Compute,
}

/** Find a cached value, or compute it: the lock is held only to read or update the table, never
 * while files are checked or the value is computed, so a background revalidation never stalls a
 * request
 * Input
    - table: &Mutex<HashMap<K, Entry<T>>> - the cache
    - key: K - entry key
    - sources: &[PathBuf] - its source files
    - lifetime: impl Fn(&Entry<T>) -> Option<Duration> - how long an entry may be reused at most
    - force: bool - check the files even when they were checked within FRESH
    - compute: impl FnOnce() -> Result<Option<T>, String> - how to compute it (None: absent)
 * Output
    - Result<Option<Arc<T>>, String>
*/
fn fetch<K: std::hash::Hash + Eq + Clone, T>(
    table: &Mutex<HashMap<K, Entry<T>>>,
    key: K,
    sources: &[PathBuf],
    lifetime: impl Fn(&Entry<T>) -> Option<Duration>,
    force: bool,
    compute: impl FnOnce() -> Result<Option<T>, String>,
) -> Result<Option<Arc<T>>, String> {
    let check = match table.lock() {
        Ok(entries) => match entries.get(&key) {
            None => Check::Compute,
            Some(entry)
                if lifetime(entry).is_some_and(|limit| entry.computed.elapsed() >= limit) =>
            {
                Check::Compute
            }
            Some(entry) if !force && entry.checked.elapsed() < FRESH => {
                Check::Reuse(entry.value.clone())
            }
            Some(entry) => Check::Compare(entry.fingerprint.clone(), entry.value.clone()),
        },
        Err(_) => Check::Compute,
    };
    let print = match check {
        Check::Reuse(value) => return Ok(Some(value)),
        Check::Compare(known, value) => {
            let print = fingerprint(sources);
            if print == known {
                if let Ok(mut entries) = table.lock() {
                    if let Some(entry) = entries.get_mut(&key) {
                        entry.checked = Instant::now();
                    }
                }
                return Ok(Some(value));
            }
            print
        }
        Check::Compute => fingerprint(sources),
    };
    let Some(value) = compute()? else {
        if let Ok(mut entries) = table.lock() {
            entries.remove(&key);
        }
        return Ok(None);
    };
    let value = Arc::new(value);
    if let Ok(mut entries) = table.lock() {
        entries.insert(
            key,
            Entry {
                fingerprint: print,
                computed: Instant::now(),
                checked: Instant::now(),
                value: value.clone(),
            },
        );
    }
    Ok(Some(value))
}

/** Return a session's projection, computing it only when its files changed (or, for an active
 * session, when it is older than a minute)
 * Input
    - key: PathBuf - the session's directory
    - sources: Vec<PathBuf> - every file or directory it is computed from
    - compute: impl FnOnce() -> Result<Option<Run>, String> - how to compute it
 * Output
    - Result<Option<Arc<Run>>, String>
*/
pub(super) fn run(
    key: PathBuf,
    sources: Vec<PathBuf>,
    compute: impl FnOnce() -> Result<Option<Run>, String>,
) -> Result<Option<Arc<Run>>, String> {
    run_checked(key, sources, false, compute)
}

/** Check a session's files now (whatever the last check) and recompute it if they changed
 * Input
    - key: PathBuf - the session's directory
    - sources: Vec<PathBuf> - every file or directory it is computed from
    - compute: impl FnOnce() -> Result<Option<Run>, String> - how to compute it
 * Output
    - Result<Option<Arc<Run>>, String>
*/
pub(super) fn revalidate_run(
    key: PathBuf,
    sources: Vec<PathBuf>,
    compute: impl FnOnce() -> Result<Option<Run>, String>,
) -> Result<Option<Arc<Run>>, String> {
    run_checked(key, sources, true, compute)
}

/** Return a session's projection (see run), checking its files now when forced
 * Input
    - key: PathBuf - the session's directory
    - sources: Vec<PathBuf> - its files
    - force: bool - check the files even when checked within FRESH
    - compute: impl FnOnce() -> Result<Option<Run>, String> - how to compute it
 * Output
    - Result<Option<Arc<Run>>, String>
*/
fn run_checked(
    key: PathBuf,
    sources: Vec<PathBuf>,
    force: bool,
    compute: impl FnOnce() -> Result<Option<Run>, String>,
) -> Result<Option<Arc<Run>>, String> {
    let cache = RUNS.get_or_init(|| Mutex::new(HashMap::new()));
    fetch(
        cache,
        key,
        &sources,
        |entry| (entry.value.described["lifecycle"] == "active").then_some(ACTIVE_TTL),
        force,
        compute,
    )
}

/** Forget cached sessions that are no longer on disk
 * Input
    - keep: &[PathBuf] - session directories that exist
 * Output
    - None
*/
pub(super) fn retain_runs(keep: &[PathBuf]) {
    if let Some(Ok(mut entries)) = RUNS.get().map(Mutex::lock) {
        entries.retain(|key, _| keep.contains(key));
    }
}

/** Return a JSON value, computing it only when its source files changed
 * Input
    - key: String - cache key
    - sources: Vec<PathBuf> - files or directories it is read from
    - compute: impl FnOnce() -> Result<Value, String> - how to read it
 * Output
    - Result<Arc<Value>, String> the value, shared with the cache
*/
pub(super) fn value(
    key: String,
    sources: Vec<PathBuf>,
    compute: impl FnOnce() -> Result<Value, String>,
) -> Result<Arc<Value>, String> {
    value_checked(key, sources, false, compute)
}

/** Check a value's files now (whatever the last check) and recompute it if they changed
 * Input
    - key: String - cache key
    - sources: Vec<PathBuf> - files or directories it is read from
    - compute: impl FnOnce() -> Result<Value, String> - how to read it
 * Output
    - Result<Arc<Value>, String>
*/
pub(super) fn revalidate_value(
    key: String,
    sources: Vec<PathBuf>,
    compute: impl FnOnce() -> Result<Value, String>,
) -> Result<Arc<Value>, String> {
    value_checked(key, sources, true, compute)
}

/** Return a JSON value (see value), checking its files now when forced
 * Input
    - key: String - cache key
    - sources: Vec<PathBuf> - its files
    - force: bool - check the files even when checked within FRESH
    - compute: impl FnOnce() -> Result<Value, String> - how to read it
 * Output
    - Result<Arc<Value>, String>
*/
fn value_checked(
    key: String,
    sources: Vec<PathBuf>,
    force: bool,
    compute: impl FnOnce() -> Result<Value, String>,
) -> Result<Arc<Value>, String> {
    let cache = VALUES.get_or_init(|| Mutex::new(HashMap::new()));
    let value = fetch(
        cache,
        key,
        &sources,
        |_| None,
        force,
        || compute().map(Some),
    )?;
    Ok(value.unwrap_or_else(|| Arc::new(Value::Null)))
}

/** Return a directory listing, listing the directory again at most every FRESH seconds
 * Input
    - key: &str - cache key
    - compute: impl FnOnce() -> Result<Vec<String>, String> - how to list it
 * Output
    - Result<Arc<Vec<String>>, String>
*/
pub(super) fn listing(
    key: &str,
    compute: impl FnOnce() -> Result<Vec<String>, String>,
) -> Result<Arc<Vec<String>>, String> {
    let cache = LISTINGS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(entries) = cache.lock() {
        if let Some((listed, names)) = entries.get(key) {
            if listed.elapsed() < FRESH {
                return Ok(names.clone());
            }
        }
    }
    let names = Arc::new(compute()?);
    if let Ok(mut entries) = cache.lock() {
        entries.insert(key.to_string(), (Instant::now(), names.clone()));
    }
    Ok(names)
}

/** List a directory now and keep the listing (whatever the last listing)
 * Input
    - key: &str - cache key
    - compute: impl FnOnce() -> Result<Vec<String>, String> - how to list it
 * Output
    - Result<Arc<Vec<String>>, String>
*/
pub(super) fn relist(
    key: &str,
    compute: impl FnOnce() -> Result<Vec<String>, String>,
) -> Result<Arc<Vec<String>>, String> {
    let names = Arc::new(compute()?);
    if let Ok(mut entries) = LISTINGS.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        entries.insert(key.to_string(), (Instant::now(), names.clone()));
    }
    Ok(names)
}

/** Forget cached values under a key prefix that are no longer wanted (removed task records)
 * Input
    - prefix: &str - key prefix
    - keep: &std::collections::HashSet<String> - keys to keep
 * Output
    - None
*/
pub(super) fn retain_values(prefix: &str, keep: &std::collections::HashSet<String>) {
    if let Some(Ok(mut entries)) = VALUES.get().map(Mutex::lock) {
        entries.retain(|key, _| !key.starts_with(prefix) || keep.contains(key));
    }
}

/** Run work over items on several threads (each item independently; the order is not kept)
 * Input
    - items: &[T] - items
    - threads: usize - threads to use at most
    - work: impl Fn(&T) + Sync - what to do with one
 * Output
    - None
*/
pub(super) fn parallel<T: Sync>(items: &[T], threads: usize, work: impl Fn(&T) + Sync) {
    let threads = threads.clamp(1, 16);
    if threads < 2 || items.len() < 64 {
        items.iter().for_each(work);
        return;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(item) = items.get(index) else {
                    break;
                };
                work(item);
            });
        }
    });
}
