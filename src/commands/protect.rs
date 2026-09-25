use std::fs;

use crate::repository::{ensure_commit, ensure_initialized, load_checkpoint, root};
use crate::resolver::{resolve_git, resolve_worktree, supported_extensions, Resolution};
use crate::util::{io_error, option, sanitize, validate_function_target};

/** Create a preserve policy for a function, by first reading and validating --function, --policy,
 * and --checkpoint, then resolving the target in both the worktree and the checkpoint commit,
 * requiring the two to match exactly, and finally writing the policy file to .crane/policies
 * Input
    - args: &[String] - command arguments with --function TARGET and optional --policy, --checkpoint
 * Output
    - Result<(), String>
    - Error if the target cannot be resolved uniquely or differs from the checkpoint
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    // Create a preserve policy only when current code matches its checkpoint
    ensure_initialized()?;
    let target = option(args, "--function").ok_or("protect requires --function TARGET")?;
    validate_function_target(&target)?;
    let policy_name = option(args, "--policy")
        .unwrap_or_else(|| format!("preserve_{}", sanitize(&target).to_lowercase()));
    let checkpoint_name = option(args, "--checkpoint").unwrap_or_else(|| "baseline".into());
    let checkpoint = load_checkpoint(&checkpoint_name)?;
    ensure_commit(&checkpoint.commit)?;
    let current = match resolve_worktree(&target)? {
        Resolution::Found(value) => value,
        Resolution::Missing => {
            return Err(format!(
            "could not resolve '{target}' in current working tree; supported source extensions: {}",
            supported_extensions()
            ))
        }
        Resolution::Duplicate(count) => {
            return Err(format!(
                "could not resolve '{target}': {count} duplicate targets"
            ))
        }
        Resolution::Unsupported => {
            return Err(format!("source language is unsupported for '{target}'"))
        }
        Resolution::ParseFailure(error) => {
            return Err(format!("source could not be parsed: {error}"))
        }
    };
    let baseline = match resolve_git(&checkpoint.commit, &target)? {
        Resolution::Found(value) => value,
        Resolution::Missing => {
            return Err(format!(
                "could not resolve {target} in checkpoint {}",
                checkpoint.name
            ))
        }
        Resolution::Duplicate(count) => {
            return Err(format!(
                "checkpoint contains {count} duplicate targets for {target}"
            ))
        }
        Resolution::Unsupported => {
            return Err(format!(
                "checkpoint source language is unsupported for '{target}'"
            ))
        }
        Resolution::ParseFailure(error) => {
            return Err(format!("checkpoint source could not be parsed: {error}"))
        }
    };
    if current.snippet != baseline.snippet {
        return Err(format!(
            "current {target} differs from checkpoint {}; commit or refresh the checkpoint before protecting it",
            checkpoint.name
        ));
    }
    let root = root()?;
    fs::write(      // Create the file
        root.join("policies").join(format!("{policy_name}.crane")),
        format!(
            "policy {policy_name} {{\n    checkpoint {checkpoint_name}\n    preserve --function {target}\n}}\n"
        ),
    )
    .map_err(io_error)?;
    println!("Created policy '{policy_name}' for {target}");
    println!("Checkpoint: {} ({})", checkpoint.name, checkpoint.commit);
    Ok(())
}
