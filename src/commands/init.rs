use std::fs;

use crate::repository::root_allow_missing;
use crate::util::io_error;

/** Initialize Crane metadata for the repository, by first locating where .crane belongs, then
 * creating the policies and checkpoints directories, and finally writing a default config.toml
 * only if one does not already exist
 * Input
    - None
 * Output
    - Result<(), String>
    - Error if not inside a Git repository or the directories cannot be created
*/
pub(crate) fn run() -> Result<(), String> {
    // Create only Crane metadata directories and never alter application source
    let root = root_allow_missing()?;
    fs::create_dir_all(root.join("policies")).map_err(io_error)?;
    fs::create_dir_all(root.join("checkpoints")).map_err(io_error)?;
    let config = root.join("config.toml");
    if !config.exists() {
        fs::write(config, "# Crane MVP configuration\n").map_err(io_error)?;
    }
    println!("Initialized {}", root.display());
    Ok(())
}
