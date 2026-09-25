use std::fs;

use crate::model::Checkpoint;
use crate::repository::{checkpoint_json, ensure_repo, git, root};
use crate::util::{io_error, now_unix, option, validate_identifier};

/** Record the current Git HEAD as a trusted baseline, by first validating the --name option
 * (default baseline), then reading HEAD and the current branch from Git, and finally writing the
 * checkpoint JSON to .crane/checkpoints/NAME.json
 * Input
    - args: &[String] - command arguments, optionally --name NAME
 * Output
    - Result<(), String>
    - Error if not initialized, the name is invalid, HEAD is unavailable, or the file cannot be written
*/
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
