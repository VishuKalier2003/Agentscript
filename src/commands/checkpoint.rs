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
    let name = option(args, "--name").unwrap_or_else(|| "baseline".into());
    let checkpoint = create(&name)?;
    println!(
        "Created checkpoint '{}' at {}",
        checkpoint.name, checkpoint.commit
    );
    println!("Branch metadata: {}", checkpoint.branch);
    Ok(())
}

/** Record the current Git HEAD as a trusted checkpoint, by validating the name, reading HEAD and
 * the current branch from Git, and writing the checkpoint JSON to .crane/checkpoints/NAME.json
 * Input
    - name: &str - checkpoint name
 * Output
    - Result<Checkpoint, String>
    - Error if not initialized, the name is invalid, HEAD is unavailable, or the file cannot be written
*/
pub(crate) fn create(name: &str) -> Result<Checkpoint, String> {
    ensure_repo()?;
    let commit = git(&["rev-parse", "HEAD"])?;
    let branch = git(&["branch", "--show-current"]).unwrap_or_else(|_| "DETACHED".into());
    if commit.is_empty() {
        return Err("Git HEAD is unavailable; commit the repository first".into());
    }
    create_at(name, &commit, &branch)
}

/** Record a given commit as a trusted checkpoint (a verified merge commit, for example)
 * Input
    - name: &str - checkpoint name
    - commit: &str - full commit SHA
    - branch: &str - branch metadata
 * Output
    - Result<Checkpoint, String>
    - Error if the name is invalid, the commit does not exist, or the file cannot be written
*/
pub(crate) fn create_at(name: &str, commit: &str, branch: &str) -> Result<Checkpoint, String> {
    let root = root()?;
    validate_identifier(name)?;
    let commit = git(&["rev-parse", "--verify", &format!("{commit}^{{commit}}")])?;
    let branch = branch.to_string();
    let checkpoint = Checkpoint {
        name: name.to_string(),
        commit,
        branch,
        created_at_unix: now_unix(),
    };
    fs::write(
        root.join("checkpoints").join(format!("{name}.json")),
        checkpoint_json(&checkpoint),
    )
    .map_err(io_error)?;
    Ok(checkpoint)
}
