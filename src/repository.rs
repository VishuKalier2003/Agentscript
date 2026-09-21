use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::Checkpoint;
use crate::util::{io_error, json_field};

pub(crate) fn root() -> Result<PathBuf, String> {
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

pub(crate) fn root_allow_missing() -> Result<PathBuf, String> {
    let mut directory = env::current_dir().map_err(io_error)?;
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

pub(crate) fn ensure_initialized() -> Result<(), String> {
    let crane_root = root()?;
    ensure_repo().and_then(|()| validate_config(&crane_root))
}

fn validate_config(crane_root: &Path) -> Result<(), String> {
    let path = crane_root.join("config.toml");
    if !path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(&path).map_err(io_error)?;
    if content.lines().any(|line| {
        let line = line.trim();
        !line.is_empty() && !line.starts_with('#')
    }) {
        return Err(format!(
            "invalid Crane configuration at {}; v0.1 config accepts only blank lines and comments",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn ensure_repo() -> Result<(), String> {
    let _ = git(&["rev-parse", "--show-toplevel"])?;
    Ok(())
}

pub(crate) fn git(args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|error| format!("failed to execute git: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

pub(crate) fn load_checkpoint(name: &str) -> Result<Checkpoint, String> {
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

pub(crate) fn ensure_commit(commit: &str) -> Result<(), String> {
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

pub(crate) fn checkpoint_json(checkpoint: &Checkpoint) -> String {
    format!(
        "{{\n  \"name\":\"{}\",\n  \"commit\":\"{}\",\n  \"branch\":\"{}\",\n  \"created_at_unix\":{}\n}}\n",
        crate::util::escape_json(&checkpoint.name),
        crate::util::escape_json(&checkpoint.commit),
        crate::util::escape_json(&checkpoint.branch),
        checkpoint.created_at_unix
    )
}
