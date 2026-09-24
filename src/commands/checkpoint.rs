use std::fs;

use crate::model::Checkpoint;
use crate::repository::{checkpoint_json, ensure_repo, git, root};
use crate::util::{io_error, now_unix, option, validate_identifier};

pub(crate) fn run(args: &[String]) -> Result<(), String> {
    // Record an explicit Git commit as the trusted baseline for future checks
    ensure_repo()?;
    let root = root()?;
    let name = option(args, "--name").unwrap_or_else(|| "baseline".into());
    validate_identifier(&name)?;
    let commit = git(&["rev-parse", "HEAD"])?;
    let branch = git(&["branch", "--show-current"]).unwrap_or_else(|_| "DETACHED".into());
    if commit.is_empty() {
        return Err("Git HEAD is unavailable; commit the repository first".into());
    }
    let checkpoint = Checkpoint {
        name: name.clone(),
        commit,
        branch,
        created_at_unix: now_unix(),
    };
    fs::write(
        root.join("checkpoints").join(format!("{name}.json")),
        checkpoint_json(&checkpoint),
    )
    .map_err(io_error)?;
    println!(
        "Created checkpoint '{}' at {}",
        checkpoint.name, checkpoint.commit
    );
    println!("Branch metadata: {}", checkpoint.branch);
    Ok(())
}
