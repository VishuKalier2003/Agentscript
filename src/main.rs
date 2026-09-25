// fetch the necessary modules
mod adapter;
mod commands;
mod model;
mod policy;
mod repository;
mod resolver;
mod util;

/** Entry point of the crane binary, dispatches the command line through commands::run and converts
 * any returned error into a printed message and a process exit code, using exit code 2 for errors
 * prefixed with HOOK_BLOCK: (Claude Code blocking convention) and exit code 1 for every other error
 * Input
    - None (arguments are read from the process environment)
 * Output
    - None
    - Exits the process with code 2 for hook blocks, 1 for other failures
*/
fn main() {
    // Keep CLI failures visible to both humans and hook runners
    if let Err(error) = commands::run() {
        eprintln!("crane: {error}");
        std::process::exit(if error.starts_with("HOOK_BLOCK:") {
            2
        } else {
            1
        });
    }
}
