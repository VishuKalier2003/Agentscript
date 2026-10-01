// fetch the necessary modules
mod adapter;
mod agent_session;
mod authority;
mod commands;
mod effects;
mod inventory;
mod ir;
mod model;
mod orchestration;
mod policy;
mod proposals;
mod repository;
mod resolver;
mod scope;
mod session;
mod tasks;
mod util;
mod verify;
mod zones;

/** Entry point of the crane binary, dispatches the command line through commands::run and converts
 * any returned error into a printed message and a process exit code, using exit code 2 for errors
 * prefixed with HOOK_BLOCK: (the blocking convention of Claude Code and Codex hooks, with the
 * marker itself left out of the printed message) and exit code 1 for every other error
 * Input
    - None (arguments are read from the process environment)
 * Output
    - None
    - Exits the process with code 2 for hook blocks, 1 for other failures
*/
fn main() {
    // Keep CLI failures visible to both humans and hook runners
    if let Err(error) = commands::run() {
        eprintln!(
            "crane: {}",
            error.strip_prefix("HOOK_BLOCK:").unwrap_or(&error)
        );
        std::process::exit(if error.starts_with("HOOK_BLOCK:") {
            2
        } else {
            1
        });
    }
}
