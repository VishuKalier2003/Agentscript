// The repository connection: one provider-neutral record of the repository Society works on,
// made once and reused by discovery, zones, policies, task execution, and delivery. Connecting
// is idempotent: it initializes Crane's metadata, makes sure a trusted checkpoint exists (never
// moving one), runs discovery, and records identity, provider, owner, name, default branch, and
// the policy and zone versions. Host-specific knowledge lives in providers; nothing here uses the
// network. The record is .crane/connection.json (format 2; format 1 records are read and upgraded).

pub(crate) mod providers;

#[cfg(test)] // Compile the module only when running tests, not in production builds
mod tests;

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::inventory::{discover, Inventory, Options};
use crate::proposals::store::agent_environment;
use crate::repository::{git, root};
use crate::util::{io_error, now_unix};
use providers::{named, parse, select, Remote};

/** Version of the connection record */
pub(crate) const CONNECTION_FORMAT: u64 = 2;

/** File in .crane holding the connection */
pub(crate) const CONNECTION_FILE: &str = "connection.json";

/** Checkpoint a connection trusts when none is named */
pub(crate) const DEFAULT_CHECKPOINT: &str = "baseline";

/** How to connect
 * Fields
    - refresh: bool - update the metadata of an existing connection
    - provider: Option<String> - provider to use instead of the detected one
    - checkpoint: Option<String> - trusted checkpoint name (created when missing)
    - default_branch: Option<String> - default branch instead of the detected one
*/
#[derive(Default)]
pub(crate) struct ConnectOptions {
    pub(crate) refresh: bool,
    pub(crate) provider: Option<String>,
    pub(crate) checkpoint: Option<String>,
    pub(crate) default_branch: Option<String>,
}

/** Read the connection record, upgrading a format 1 record (written by the first control plane)
 * Input
    - None
 * Output
    - Result<Option<Value>, String>
*/
pub(crate) fn load() -> Result<Option<Value>, String> {
    let text = match fs::read_to_string(root()?.join(CONNECTION_FILE)) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let mut record: Value = serde_json::from_str(&text)
        .map_err(|error| format!(".crane/{CONNECTION_FILE} is not valid JSON: {error}"))?;
    if record["connection_format"].as_u64() == Some(1) {
        let remote = record["remote"].as_str().and_then(parse);
        let provider = remote
            .as_ref()
            .and_then(|remote| select(remote, None).ok())
            .map_or("local", |provider| provider.name());
        record["connection_format"] = json!(CONNECTION_FORMAT);
        record["provider"] = json!(provider);
        record["owner"] = json!(remote.as_ref().and_then(|remote| remote.owner.clone()));
        record["name"] = json!(remote.as_ref().map(|remote| remote.name.clone()));
        record["local_path"] = record["root"].clone();
        record["connection_status"] = json!("connected");
        record["trusted_checkpoint"] = json!(DEFAULT_CHECKPOINT);
        record["history"] = json!([{"action": "connected", "at": record["connected_at"], "by": record["connected_by"]}]);
    }
    if record["connection_format"].as_u64() != Some(CONNECTION_FORMAT) {
        return Err(format!(
            ".crane/{CONNECTION_FILE} has an unsupported format"
        ));
    }
    Ok(Some(record))
}

/** Return the record only while the repository is connected
 * Input
    - None
 * Output
    - Option<Value>
*/
pub(crate) fn connected() -> Option<Value> {
    load()
        .ok()
        .flatten()
        .filter(|record| record["connection_status"] == "connected")
}

/** Write the record atomically (a temporary file renamed into place)
 * Input
    - record: &Value - record
 * Output
    - Result<(), String>
*/
fn save(record: &Value) -> Result<(), String> {
    let path = root()?.join(CONNECTION_FILE);
    let temporary = path.with_extension(format!("json.tmp{}", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_string_pretty(record).map_err(io_error)? + "\n",
    )
    .map_err(io_error)?;
    fs::rename(&temporary, &path).map_err(io_error)
}

/** Make a checkpoint the connected repository's trusted state (a verified merge does this); without
 * a connection nothing is recorded
 * Input
    - checkpoint: &str - checkpoint name
    - commit: &str - its commit
    - why: &str - what made it trusted
 * Output
    - Result<(), String>
*/
pub(crate) fn trust(checkpoint: &str, commit: &str, why: &str) -> Result<(), String> {
    let Some(mut record) = connected() else {
        return Ok(());
    };
    let previous = record["trusted_checkpoint"].clone();
    record["trusted_checkpoint"] = json!(checkpoint);
    log(
        &mut record,
        "checkpoint_trusted",
        json!({"checkpoint": checkpoint, "commit": commit, "previous": previous, "why": why}),
    );
    save(&record)
}

/** Append a history entry
 * Input
    - record: &mut Value - record
    - action: &str - connected, refreshed, reconnected, disconnected, checkpoint_created
    - detail: Value - details
 * Output
    - None
*/
fn log(record: &mut Value, action: &str, detail: Value) {
    let entry = json!({"action": action, "at": now_unix(), "by": git(&["config", "user.email"]).ok(), "detail": detail});
    match record["history"].as_array_mut() {
        Some(history) => history.push(entry),
        None => record["history"] = json!([entry]),
    }
}

/** Find the repository's top level and check it can be connected: a Git repository with at least
 * one commit, and not a session worktree
 * Input
    - None
 * Output
    - Result<PathBuf, String>
*/
fn top_level() -> Result<PathBuf, String> {
    let top = git(&["rev-parse", "--show-toplevel"]).map_err(|_| {
        "invalid repository: the current directory is not inside a Git repository; run 'git init' and commit first".to_string()
    })?;
    let top = PathBuf::from(top);
    let normalized = top.to_string_lossy().replace('\\', "/");
    if normalized.contains("/.crane/runtime/worktrees/") {
        return Err("this is a Crane session worktree; connect the main repository instead".into());
    }
    if git(&["rev-parse", "--verify", "HEAD^{commit}"]).is_err() {
        return Err(
            "missing Git metadata: the repository has no commits yet; commit once, then connect"
                .into(),
        );
    }
    Ok(top)
}

/** What identify finds: the remote, the provider's name, the web page, and the pull request
 * link template */
type Identified = (Option<Remote>, &'static str, Option<String>, Option<String>);

/** Read the origin remote and choose its provider
 * Input
    - forced: Option<&str> - provider named by the user
 * Output
    - Result<(Option<Remote>, &'static str, Option<String>, Option<String>), String> remote,
      provider name, web URL, pull request URL template
*/
fn identify(forced: Option<&str>) -> Result<Identified, String> {
    let url = git(&["remote", "get-url", "origin"])
        .ok()
        .filter(|url| !url.trim().is_empty());
    let remote = url.as_deref().and_then(parse);
    let fallback = Remote {
        host: None,
        owner: None,
        name: String::new(),
        url: String::new(),
    };
    let provider = select(remote.as_ref().unwrap_or(&fallback), forced)?;
    let (web, pull) = match &remote {
        Some(remote) => (provider.web_url(remote), provider.pull_request_url(remote)),
        None => (None, None),
    };
    Ok((remote, provider.name(), web, pull))
}

/** Find the default branch: the one given, origin's HEAD, else the checked-out branch
 * Input
    - given: Option<&String> - branch named by the user
 * Output
    - Option<String>
*/
fn default_branch(given: Option<&String>) -> Option<String> {
    given
        .cloned()
        .or_else(|| {
            git(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
                .ok()
                .map(|reference| reference.trim_start_matches("origin/").to_string())
        })
        .or_else(|| git(&["branch", "--show-current"]).ok())
        .filter(|branch| !branch.is_empty())
}

/** Summarize a discovery run for the record
 * Input
    - inventory: &Inventory - inventory
 * Output
    - Value
*/
fn discovery_summary(inventory: &Inventory) -> Value {
    let languages = inventory.languages().into_iter().map(|(name, totals)| json!({"language": name, "files": totals.files, "enforceable": totals.enforceable})).collect::<Vec<_>>();
    json!({
        "at": now_unix(),
        "head": inventory.head,
        "files": inventory.snapshot.files.len(),
        "symbols": inventory.graph.entities.len(),
        "languages": languages,
        "enforceable": inventory.languages().values().any(|totals| totals.enforceable && totals.files > 0),
        "warnings": inventory.warnings.len(),
    })
}

/** Record a discovery run on the connection (called by crane discover), when connected
 * Input
    - inventory: &Inventory - inventory
 * Output
    - Result<(), String>
*/
pub(crate) fn note_discovery(inventory: &Inventory) -> Result<(), String> {
    let Some(mut record) = connected() else {
        return Ok(());
    };
    record["last_discovery"] = discovery_summary(inventory);
    save(&record)
}

/** Return the current contract and zone versions
 * Input
    - crane: &Path - .crane directory
 * Output
    - (Option<String>, Option<String>) policy (contract set) version and zone set version
*/
fn versions(crane: &Path) -> (Option<String>, Option<String>) {
    let policy = crate::ir::compile().ok().map(|set| set.version);
    let zones = crate::zones::model::load(crane)
        .ok()
        .map(|(zones, _)| crate::zones::model::set_version(&zones));
    (policy, zones)
}

/** Connect the repository: refuse non-repositories, repositories without commits, session
 * worktrees, a .crane connected to another repository, and agents; return an existing
 * connection unchanged (unless refreshing); otherwise initialize Crane's metadata, make sure the
 * trusted checkpoint exists (creating it at HEAD only when missing), run discovery, and record
 * everything; a disconnected repository is reconnected with its history kept
 * Input
    - options: &ConnectOptions - options
 * Output
    - Result<Value, String> the record, with already_connected
*/
pub(crate) fn connect(options: &ConnectOptions) -> Result<Value, String> {
    let top = top_level()?;
    crate::effects::within(&top, || connect_here(options))
}

/** Connect from the repository's top level (see connect)
 * Input
    - options: &ConnectOptions - options
 * Output
    - Result<Value, String>
*/
fn connect_here(options: &ConnectOptions) -> Result<Value, String> {
    let (_, identity) = crate::session::repository_identity()?;
    let existing = root().ok().map(|_| load()).transpose()?.flatten();
    if let Some(existing) = &existing {
        if existing["repository_id"] != identity.as_str() {
            return Err(format!(
                "this .crane is connected to another repository ({}); run 'crane repo disconnect --forget' to release it first",
                existing["repository_id"].as_str().unwrap_or("unknown")
            ));
        }
        if existing["connection_status"] == "connected" && !options.refresh {
            let mut value = existing.clone();
            value["already_connected"] = json!(true);
            return Ok(value);
        }
    }
    if let Some(marker) = agent_environment() {
        return Err(format!(
            "crane repo connect refuses to run in an agent environment ({marker} is set)"
        ));
    }
    let crane = crate::commands::init::initialize()?;
    let (remote, provider, web_url, pull_request_url) = identify(options.provider.as_deref())?;
    let top = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?);
    let name = remote
        .as_ref()
        .map(|remote| remote.name.clone())
        .or_else(|| {
            top.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
    let owner = remote.as_ref().and_then(|remote| remote.owner.clone());
    let checkpoint = options
        .checkpoint
        .clone()
        .or_else(|| {
            existing
                .as_ref()
                .and_then(|existing| existing["trusted_checkpoint"].as_str().map(String::from))
        })
        .unwrap_or_else(|| DEFAULT_CHECKPOINT.into());
    let mut record = existing.clone().unwrap_or_else(|| json!({"history": []}));
    if !crane
        .join("checkpoints")
        .join(format!("{checkpoint}.json"))
        .exists()
    {
        // A trusted checkpoint is created once and never moved by connecting again
        let created = crate::commands::checkpoint::create(&checkpoint)?;
        log(
            &mut record,
            "checkpoint_created",
            json!({"checkpoint": created.name, "commit": created.commit}),
        );
    }
    let inventory = discover(&Options { full: false })?;
    let summary = discovery_summary(&inventory);
    let (policy_version, zone_version) = versions(&crane);
    let now = now_unix();
    let action = match &existing {
        None => "connected",
        Some(existing) if existing["connection_status"] != "connected" => "reconnected",
        Some(_) => "refreshed",
    };
    for (key, value) in [
        ("connection_format", json!(CONNECTION_FORMAT)),
        ("repository_id", json!(identity)),
        ("provider", json!(provider)),
        (
            "host",
            json!(remote.as_ref().and_then(|remote| remote.host.clone())),
        ),
        ("owner", json!(owner)),
        ("name", json!(name)),
        (
            "full_name",
            json!(match (&owner, &name) {
                (Some(owner), Some(name)) => format!("{owner}/{name}"),
                (_, Some(name)) => name.clone(),
                _ => String::new(),
            }),
        ),
        (
            "remote",
            json!(remote.as_ref().map(|remote| remote.url.clone())),
        ),
        ("web_url", json!(web_url)),
        ("pull_request_url", json!(pull_request_url)),
        (
            "default_branch",
            json!(default_branch(options.default_branch.as_ref())),
        ),
        ("local_path", json!(top.to_string_lossy())),
        ("root", json!(top.to_string_lossy())),
        ("connection_status", json!("connected")),
        ("trusted_checkpoint", json!(checkpoint)),
        ("last_discovery", summary.clone()),
        ("head", summary["head"].clone()),
        ("languages", summary["languages"].clone()),
        ("files", summary["files"].clone()),
        ("symbols", summary["symbols"].clone()),
        ("policy_version", json!(policy_version)),
        ("zone_version", json!(zone_version)),
        ("disconnected_at", Value::Null),
        ("disconnect_reason", Value::Null),
    ] {
        record[key] = value;
    }
    if existing.is_none() {
        record["connected_at"] = json!(now);
        record["connected_by"] = json!(git(&["config", "user.email"]).ok());
    }
    record["refreshed_at"] = if action == "connected" {
        Value::Null
    } else {
        json!(now)
    };
    let detail = json!({"provider": provider, "default_branch": record["default_branch"]});
    log(&mut record, action, detail);
    save(&record)?;
    record["already_connected"] = json!(false);
    Ok(record)
}

/** Disconnect: Society stops using the repository (the control plane answers 409) but the record,
 * its history, and every piece of Crane configuration stay, so reconnecting restores it; forget
 * removes the record itself (to connect this .crane to another repository)
 * Input
    - reason: Option<String> - why
    - forget: bool - remove the record
 * Output
    - Result<Value, String> the record afterwards (null when forgotten)
*/
pub(crate) fn disconnect(reason: Option<String>, forget: bool) -> Result<Value, String> {
    if let Some(marker) = agent_environment() {
        return Err(format!(
            "crane repo disconnect refuses to run in an agent environment ({marker} is set)"
        ));
    }
    let mut record = load()?.ok_or("the repository is not connected")?;
    if forget {
        fs::remove_file(root()?.join(CONNECTION_FILE)).map_err(io_error)?;
        return Ok(Value::Null);
    }
    if record["connection_status"] == "disconnected" {
        return Ok(record);
    }
    record["connection_status"] = json!("disconnected");
    record["disconnected_at"] = json!(now_unix());
    record["disconnect_reason"] = json!(reason);
    log(&mut record, "disconnected", json!({"reason": reason}));
    save(&record)?;
    Ok(record)
}

/** Describe a checkpoint against the repository now: absent, invalid, missing (its commit is gone),
 * current (HEAD), stale (HEAD moved on), or diverged (HEAD no longer contains it)
 * Input
    - name: &str - checkpoint name
 * Output
    - Value {name, commit, status, commits_since, created_at, message}
*/
pub(crate) fn checkpoint_state(name: &str) -> Value {
    let path = match root() {
        Ok(crane) => crane.join("checkpoints").join(format!("{name}.json")),
        Err(error) => return json!({"name": name, "status": "absent", "message": error}),
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return json!({"name": name, "status": "absent", "message": format!("no checkpoint '{name}'; reconnect or run 'crane checkpoint --name {name}'")});
    };
    let stored: Value = serde_json::from_str(&text).unwrap_or(json!({}));
    let commit = stored["commit"].as_str().unwrap_or_default().to_string();
    if let Err(error) =
        crate::ir::checkpoint_commit(name, stored["name"].as_str().unwrap_or(name), &commit)
    {
        return json!({"name": name, "commit": commit, "status": "invalid", "message": error});
    }
    if git(&["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_err() {
        return json!({"name": name, "commit": commit, "status": "missing", "message": "the checkpoint's commit is not in this repository (history rewritten or not fetched)"});
    }
    let head = git(&["rev-parse", "HEAD"]).unwrap_or_default();
    let ancestor = git(&["merge-base", "--is-ancestor", &commit, "HEAD"]).is_ok();
    let since = git(&["rev-list", "--count", &format!("{commit}..HEAD")])
        .ok()
        .and_then(|count| count.parse::<u64>().ok())
        .unwrap_or(0);
    let status = if commit == head {
        "current"
    } else if ancestor {
        "stale"
    } else {
        "diverged"
    };
    json!({
        "name": name,
        "commit": commit,
        "status": status,
        "commits_since": since,
        "created_at": stored["created_at_unix"],
        "message": match status {
            "stale" => format!("HEAD is {since} commits past the trusted checkpoint; contracts still compare against it (move it deliberately with 'crane checkpoint --name {name}')"),
            "diverged" => "HEAD no longer contains the trusted checkpoint".to_string(),
            _ => "HEAD is the trusted checkpoint".to_string(),
        },
    })
}

/** Report the connection with live state: provider, branch, trusted checkpoint, discovery
 * freshness, and whether policies or zones changed since connecting; no discovery is run
 * Input
    - None
 * Output
    - Result<Value, String>
*/
pub(crate) fn status() -> Result<Value, String> {
    let Some(record) = root().ok().map(|_| load()).transpose()?.flatten() else {
        return Ok(
            json!({"connection_status": "not_connected", "message": "run 'crane repo connect' in the repository"}),
        );
    };
    let crane = root()?;
    let head = git(&["rev-parse", "HEAD"]).ok();
    let discovery_state = match record["last_discovery"]["head"].as_str() {
        None => "never",
        Some(seen) if Some(seen) == head.as_deref() => "fresh",
        Some(_) => "stale",
    };
    let (policy, zones) = versions(&crane);
    let drift = |key: &str, current: &Option<String>| json!({"recorded": record[key], "current": current, "changed": record[key].as_str() != current.as_deref()});
    Ok(json!({
        "connection_format": record["connection_format"],
        "connection_status": record["connection_status"],
        "repository_id": record["repository_id"],
        "provider": record["provider"],
        "host": record["host"],
        "owner": record["owner"],
        "name": record["name"],
        "full_name": record["full_name"],
        "remote": record["remote"],
        "web_url": record["web_url"],
        "default_branch": record["default_branch"],
        "current_branch": git(&["branch", "--show-current"]).ok().filter(|branch| !branch.is_empty()),
        "head": head,
        "local_path": record["local_path"],
        "connected_at": record["connected_at"],
        "connected_by": record["connected_by"],
        "refreshed_at": record["refreshed_at"],
        "disconnected_at": record["disconnected_at"],
        "trusted_checkpoint": checkpoint_state(record["trusted_checkpoint"].as_str().unwrap_or(DEFAULT_CHECKPOINT)),
        "discovery": {"state": discovery_state, "last": record["last_discovery"]},
        "policy_version": drift("policy_version", &policy),
        "zone_version": drift("zone_version", &zones),
    }))
}

/** Describe the repository in full: the status plus branches, worktrees (Crane session worktrees
 * marked), checkpoints, the provider's local capabilities, what is configured, readiness, and the
 * connection history
 * Input
    - None
 * Output
    - Result<Value, String>
*/
pub(crate) fn inspect() -> Result<Value, String> {
    let mut value = status()?;
    if value["connection_status"] == "not_connected" {
        return Ok(value);
    }
    let crane = root()?;
    let record = load()?.unwrap_or(json!({}));
    let branches = git(&["for-each-ref", "--format=%(refname:short)\t%(objectname)", "refs/heads"])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(name, commit)| json!({"name": name, "commit": commit, "delivery": name.starts_with("crane/")}))
        .collect::<Vec<_>>();
    let mut worktrees = Vec::new();
    let mut current = json!({});
    for line in git(&["worktree", "list", "--porcelain"])
        .unwrap_or_default()
        .lines()
    {
        if let Some(path) = line.strip_prefix("worktree ") {
            if current.get("path").is_some() {
                worktrees.push(current.clone());
            }
            let normalized = path.replace('\\', "/");
            current = json!({"path": path, "session": normalized.contains("/.crane/runtime/worktrees/").then(|| normalized.rsplit('/').next().unwrap_or_default().to_string())});
        } else if let Some(branch) = line.strip_prefix("branch ") {
            current["branch"] = json!(branch.trim_start_matches("refs/heads/"));
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            current["head"] = json!(head);
        }
    }
    if current.get("path").is_some() {
        worktrees.push(current);
    }
    let checkpoints = fs::read_dir(crane.join("checkpoints"))
        .map(|entries| {
            let mut names = entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .strip_suffix(".json")
                        .map(String::from)
                })
                .collect::<Vec<_>>();
            names.sort();
            names
                .iter()
                .map(|name| checkpoint_state(name))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let repository = crane.parent().map(Path::to_path_buf).unwrap_or_default();
    let mentions = |path: &str, needle: &str| {
        fs::read_to_string(repository.join(path)).is_ok_and(|text| text.contains(needle))
    };
    let configured = |name: &str| crane.join(name).exists();
    let provider = value["provider"]
        .as_str()
        .and_then(named)
        .map_or(json!(null), |provider| provider.capabilities());
    let checkpoint_ok = matches!(
        value["trusted_checkpoint"]["status"].as_str(),
        Some("current" | "stale")
    );
    let mut missing = Vec::new();
    if value["connection_status"] != "connected" {
        missing.push("the repository is disconnected");
    }
    if !checkpoint_ok {
        missing.push("the trusted checkpoint is not usable");
    }
    if value["discovery"]["state"] == "never" {
        missing.push("discovery never ran");
    }
    if record["last_discovery"]["enforceable"] != true {
        missing.push("no language Crane can enforce contracts in");
    }
    value["branches"] = json!(branches);
    value["worktrees"] = json!(worktrees);
    value["checkpoints"] = json!(checkpoints);
    value["capabilities"] = json!({
        "ready": missing.is_empty(),
        "missing": missing,
        "git": {"commits": true, "remote": !value["remote"].is_null(), "default_branch": !value["default_branch"].is_null()},
        "provider": provider,
        "discovery": {"state": value["discovery"]["state"], "enforceable_languages": record["last_discovery"]["languages"].as_array().map(|languages| languages.iter().filter(|language| language["enforceable"] == true && language["files"].as_u64().unwrap_or(0) > 0).map(|language| language["language"].clone()).collect::<Vec<_>>())},
        "configuration": {
            "policies": fs::read_dir(crane.join("policies")).map(|entries| entries.count()).unwrap_or(0),
            "zones": fs::read_dir(crane.join("zones")).map(|entries| entries.count()).unwrap_or(0),
            "testing": configured("testing.json"),
            "delivery": configured("delivery.json"),
            "task_sources": configured("sources/config.json"),
            "autonomy": configured("autonomy.json"),
            "budget": configured("budget.json"),
        },
        "agents": {
            "claude": mentions(".claude/settings.json", "crane"),
            "codex": mentions(".codex/hooks.json", "crane"),
        },
    });
    value["history"] = record["history"].clone();
    Ok(value)
}

/** The repository's owner and name for consumers (evidence, task matching, delivery): from the
 * connection when connected, else parsed from the origin remote
 * Input
    - None
 * Output
    - (Option<String>, Option<String>)
*/
pub(crate) fn owner_and_name() -> (Option<String>, Option<String>) {
    if let Some(record) = connected() {
        return (
            record["owner"].as_str().map(String::from),
            record["name"].as_str().map(String::from),
        );
    }
    match git(&["remote", "get-url", "origin"])
        .ok()
        .as_deref()
        .and_then(parse)
    {
        Some(remote) => (remote.owner, Some(remote.name)),
        None => (None, None),
    }
}
