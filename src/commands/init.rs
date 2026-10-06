use std::fs;

use crate::repository::root_allow_missing;
use crate::util::io_error;

/** Initialize Crane metadata for the repository, by first locating where .crane belongs, then
 * creating the policies, checkpoints, zones, and tasks directories, and finally writing a default
 * config.toml
 * only if one does not already exist
 * Input
    - None
 * Output
    - Result<(), String>
    - Error if not inside a Git repository or the directories cannot be created
*/
pub(crate) fn run() -> Result<(), String> {
    println!("Initialized {}", initialize()?.display());
    Ok(())
}

/** Create Crane's metadata directories (policies, checkpoints, zones, tasks) and a default
 * config.toml where missing, without printing; safe to repeat
 * Input
    - None
 * Output
    - Result<PathBuf, String> the .crane directory
    - Error if not inside a Git repository or the directories cannot be created
*/
pub(crate) fn initialize() -> Result<std::path::PathBuf, String> {
    // Create only Crane metadata directories and never alter application source
    let root = root_allow_missing()?;
    fs::create_dir_all(root.join("policies")).map_err(io_error)?;
    fs::create_dir_all(root.join("checkpoints")).map_err(io_error)?;
    fs::create_dir_all(root.join("zones")).map_err(io_error)?;
    fs::create_dir_all(root.join("tasks")).map_err(io_error)?;
    let config = root.join("config.toml");
    if !config.exists() {
        fs::write(config, "# Crane MVP configuration\n").map_err(io_error)?;
    }
    Ok(root)
}
