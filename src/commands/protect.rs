use std::fs;

use crate::repository::{ensure_commit, ensure_initialized, load_checkpoint, root};
use crate::resolver::{resolve_git, resolve_worktree};
use crate::util::{io_error, option, sanitize, validate_function_target};

pub(crate) fn run(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    let target = option(args, "--function").ok_or("protect requires --function TARGET")?;
    validate_function_target(&target)?;
    let policy_name = option(args, "--policy")
        .unwrap_or_else(|| format!("preserve_{}", sanitize(&target).to_lowercase()));
    let checkpoint_name = option(args, "--checkpoint").unwrap_or_else(|| "baseline".into());
    let checkpoint = load_checkpoint(&checkpoint_name)?;
    ensure_commit(&checkpoint.commit)?;
    let current = resolve_worktree(&target)?
        .ok_or_else(|| format!("could not resolve '{target}' in current working tree"))?;
    let baseline = resolve_git(&checkpoint.commit, &target)?.ok_or_else(|| {
        format!(
            "could not resolve {target} in checkpoint {}",
            checkpoint.name
        )
    })?;
    if current.snippet != baseline.snippet {
        return Err(format!(
            "current {target} differs from checkpoint {}; commit or refresh the checkpoint before protecting it",
            checkpoint.name
        ));
    }
    let root = root()?;
    fs::write(
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
