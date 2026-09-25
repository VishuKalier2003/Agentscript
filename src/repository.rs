use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::Checkpoint;
use crate::util::{io_error, json_field};

/** Locate the nearest initialized .crane directory from the current path, by first fetching the
 * current directory and then looping through its ancestors until a .crane directory is found
 * Input
    - None
 * Output
    - Result<PathBuf, String>
    - Error if crane init not invoked
*/
pub(crate) fn root() -> Result<PathBuf, String> {
    // Locate the nearest initialized Crane directory from the current path
    let mut directory = env::current_dir().map_err(io_error)?;
    loop {
        let candidate = directory.join(".crane");
        if candidate.is_dir() {
            return Ok(candidate);
        }
        if !directory.pop() {
            break;
        }
    }
    Err("could not find .crane; run 'crane init'".into())
}

/** Locate the .crane directory that init should use, by first fetching the current directory and
 * then looping through its ancestors, returning an existing .crane directory if one is found, or
 * the would-be .crane path under the starting directory once a .git marker proves we are in a repo
 * Input
    - None
 * Output
    - Result<PathBuf, String>
    - Error if the current path is not inside a Git repository
*/
pub(crate) fn root_allow_missing() -> Result<PathBuf, String> {
    // Locate a Git repository so init can create its Crane directory
    let mut directory = env::current_dir().map_err(io_error)?;  // ? return error if current_dir fails, else unwraps the value
    let original = directory.clone();
    loop {
        let candidate = directory.join(".crane");
        if candidate.is_dir() {
            return Ok(candidate);
        }
        if directory.join(".git").exists() {
            return Ok(original.join(".crane"));
        }
        if !directory.pop() {
            break;
        }
    }
    Err("not inside a Git repository".into())
}

/** Check that crane is initialized correctly, by first locating the .crane directory through root,
 * then confirming Git recognizes the repository, and finally validating .crane/config.toml
 * Input
    - None
 * Output
    - Result<(), String>
    - Error if .crane is missing, Git is unavailable, or the configuration is invalid
*/
pub(crate) fn ensure_initialized() -> Result<(), String> {
    // Require both Git and a valid Crane configuration before verification
    let crane_root = root()?;       // ? to indicate if success provide PathBuf, else return the error immediately
    ensure_repo().and_then(|()| validate_config(&crane_root))   // lambda chaining, |x| x+1
}

/** Validate the reserved config.toml file, by treating a missing file as valid and otherwise
 * reading it and rejecting any line that is neither blank nor a # comment
 * Input
    - crane_root: &Path - path of the .crane directory
 * Output
    - Result<(), String>
    - Error if the file cannot be read or contains unsupported configuration
*/
fn validate_config(crane_root: &Path) -> Result<(), String> {
    // Reject configuration syntax that v0.1 does not understand
    let path = crane_root.join("config.toml");
    // safe check if config.toml doesn't exist, still count the config as validated
    if !path.exists() {
        return Ok(());      // Unit value () indicates success, no value to return
    }
    let content = fs::read_to_string(&path).map_err(io_error)?;
    if content.lines().any(|line| {     // lambda call
        let line = line.trim();
        !line.is_empty() && !line.starts_with('#')
    }) {
        // Currently we make config.toml as empty, we will decide what to do later
        return Err(format!(
            "invalid Crane configuration at {}; v0.1.5 config accepts only blank lines and comments",
            path.display()
        ));
    }
    Ok(())      // safe check, if there are no errors
}

/** Confirm the current directory belongs to a Git repository, by running
 * git rev-parse --show-toplevel and discarding its output
 * Input
    - None
 * Output
    - Result<(), String>
    - Error if Git cannot run or the path is not inside a repository
*/
pub(crate) fn ensure_repo() -> Result<(), String> {
    // Use Git itself as the authority for repository membership
    let _ = git(&["rev-parse", "--show-toplevel"])?;
    Ok(())
}

/** Run a Git command directly (no shell), by spawning git with the given arguments, waiting for
 * it to finish, and returning trimmed stdout on success or trimmed stderr as the error
 * Input
    - args: &[&str] - arguments passed to git
 * Output
    - Result<String, String>
    - Error if git cannot be spawned or exits with a failure status
*/
pub(crate) fn git(args: &[&str]) -> Result<String, String> {
    // Run Git without shell interpolation so paths and arguments stay isolated
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|error| format!("failed to execute git: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

/** Load a named checkpoint, by first reading .crane/checkpoints/NAME.json and then extracting the
 * name, commit, and branch fields, requiring the commit field to be present
 * Input
    - name: &str - checkpoint name
 * Output
    - Result<Checkpoint, String>
    - Error if the checkpoint file does not exist or has no commit
*/
pub(crate) fn load_checkpoint(name: &str) -> Result<Checkpoint, String> {
    // Load checkpoint metadata without accepting a missing or partial baseline
    let path = root()?.join("checkpoints").join(format!("{name}.json"));
    let content = fs::read_to_string(path).map_err(|_| {
        format!("checkpoint '{name}' does not exist; run 'crane checkpoint --name {name}'")
    })?;
    Ok(Checkpoint {
        name: json_field(&content, "name").unwrap_or_else(|| name.into()),
        commit: json_field(&content, "commit").ok_or("checkpoint missing commit")?,
        branch: json_field(&content, "branch").unwrap_or_default(),
        created_at_unix: 0,
    })
}

/** Confirm a checkpoint commit exists locally, by first rejecting an empty reference and then
 * asking git rev-parse --verify to resolve COMMIT^{commit} without fetching anything
 * Input
    - commit: &str - commit SHA recorded in a checkpoint
 * Output
    - Result<(), String>
    - Error if the reference is empty, missing, or not a commit
*/
pub(crate) fn ensure_commit(commit: &str) -> Result<(), String> {
    // Confirm the checkpoint commit is locally available before resolving source
    if commit.trim().is_empty() {
        return Err("checkpoint has an empty Git commit reference".into());
    }
    let reference = format!("{commit}^{{commit}}");
    git(&["rev-parse", "--verify", &reference]).map_err(|_| {
        format!(
            "checkpoint commit '{commit}' is missing or invalid; restore the commit locally before running Crane"
        )
    })?;
    Ok(())
}

/** Serialize a checkpoint to its on-disk JSON format, by formatting a fixed field order and
 * escaping each string field
 * Input
    - checkpoint: &Checkpoint - checkpoint to serialize
 * Output
    - String containing the JSON document
*/
pub(crate) fn checkpoint_json(checkpoint: &Checkpoint) -> String {
    // Serialize checkpoint identity in the version-controlled metadata format
    format!(
        "{{\n  \"name\":\"{}\",\n  \"commit\":\"{}\",\n  \"branch\":\"{}\",\n  \"created_at_unix\":{}\n}}\n",
        crate::util::escape_json(&checkpoint.name),
        crate::util::escape_json(&checkpoint.commit),
        crate::util::escape_json(&checkpoint.branch),
        checkpoint.created_at_unix
    )
}
