use std::path::Path;

use crate::policy;

pub(crate) fn run(path: &Path) -> Result<(), String> {
    // Parse and display one policy without evaluating repository state
    let parsed = policy::parse_file(path)?;
    policy::print(&parsed);
    Ok(())
}
