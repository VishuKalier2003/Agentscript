use std::path::Path;

use crate::policy;

/** Parse and print one policy file without checking the repository, by calling parse_file and then
 * printing the parsed policy
 * Input
    - path: &Path - .crane policy file
 * Output
    - Result<(), String>
    - Error if the file cannot be read or is not a valid policy
*/
pub(crate) fn run(path: &Path) -> Result<(), String> {
    // Parse and display one policy without evaluating repository state
    let parsed = policy::parse_file(path)?;
    policy::print(&parsed);
    Ok(())
}
