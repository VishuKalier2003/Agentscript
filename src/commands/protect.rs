use std::fs;

use crate::model::{ItemKind, Scope};
use crate::repository::{ensure_commit, ensure_initialized, load_checkpoint, root};
use crate::resolver::{resolve_git, resolve_worktree, supported_extensions, Resolution};
use crate::scope::{verify_scope, ScopeContext};
use crate::util::{
    io_error, keyword_option, option, sanitize, validate_function_target, validate_identifier,
};

/** Read the scope of a protect or target command, by accepting "scope VALUE" or "--scope VALUE"
 * and defaulting to block
 * Input
    - args: &[String] - command arguments
 * Output
    - Result<Scope, String>
    - Error if the scope has no value or an invalid one
*/
pub(crate) fn scope_option(args: &[String]) -> Result<Scope, String> {
    keyword_option(
        args,
        &["scope", "--scope"],
        "block, file, flow, folder, or all",
    )?
    .map_or(Ok(Scope::Block), |value| Scope::parse(&value))
}

/** Read which item a protect or target command points at, by finding the one item flag
 * (--function, --data, --variable, --class, or --interface) and the target after it
 * Input
    - args: &[String] - command arguments
    - command: &str - command name, used in messages
 * Output
    - Result<(ItemKind, String), String> item kind and validated target
    - Error if no item flag, more than one, or no valid target is given
*/
pub(crate) fn item_option(args: &[String], command: &str) -> Result<(ItemKind, String), String> {
    let found = ItemKind::ALL
        .into_iter()
        .filter_map(|kind| option(args, kind.flag()).map(|target| (kind, target)))
        .collect::<Vec<_>>();
    let [(kind, target)] = found.as_slice() else {
        return Err(format!(
            "{command} requires exactly one of {} followed by TARGET",
            ItemKind::FLAGS
        ));
    };
    validate_function_target(target)?;
    Ok((*kind, target.clone()))
}

/** Create a preserve policy for an item, by first reading and validating the item flag and
 * target, --policy, --checkpoint, and the optional trailing scope, then resolving the item in both
 * the worktree and the checkpoint commit and requiring the two to match exactly (plus the whole
 * scope, when it is wider than block), and finally writing the policy file to .crane/policies
 * Input
    - args: &[String] - command arguments with one of --function, --data, --variable, --class, or
      --interface followed by TARGET, and optional --policy, --checkpoint, and "scope SCOPE"
      (or "--scope SCOPE")
 * Output
    - Result<(), String>
    - Error if the target cannot be resolved uniquely, the scope is invalid, or the code differs
      from the checkpoint
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    // Create a preserve policy only when current code matches its checkpoint
    ensure_initialized()?;
    let (kind, target) = item_option(args, "protect")?;
    let policy_name = option(args, "--policy")
        .unwrap_or_else(|| format!("preserve_{}", sanitize(&target).to_lowercase()));
    validate_identifier(&policy_name)?; // the name becomes a file name, so no paths
    let checkpoint_name = option(args, "--checkpoint").unwrap_or_else(|| "baseline".into());
    validate_identifier(&checkpoint_name)?;
    // Scope may be written like the policy language ("scope file") or as a flag ("--scope file")
    let scope = scope_option(args)?;
    let checkpoint = load_checkpoint(&checkpoint_name)?;
    ensure_commit(&checkpoint.commit)?;
    let current = match resolve_worktree(kind, &target)? {
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
    let baseline = match resolve_git(&checkpoint.commit, kind, &target)? {
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
    if scope != Scope::Block {
        // A wider scope is only protectable if all of its current code matches the checkpoint
        verify_scope(&mut ScopeContext::new(), scope, &checkpoint.commit, kind, &target).map_err(|error| {
            format!(
                "current {} scope of {target} differs from checkpoint {}: {error}; commit or refresh the checkpoint before protecting it",
                scope.name(),
                checkpoint.name
            )
        })?;
    }
    let scope_suffix = if scope == Scope::Block {
        String::new()
    } else {
        format!(" scope {}", scope.name())
    };
    let root = root()?;
    fs::write(      // Create the file
        root.join("policies").join(format!("{policy_name}.crane")),
        format!(
            "policy {policy_name} {{\n    checkpoint {checkpoint_name};\n    preserve {} {target}{scope_suffix};\n}}\n",
            kind.flag()
        ),
    )
    .map_err(io_error)?;
    println!(
        "Created policy '{policy_name}' for {target} (scope {})",
        scope.name()
    );
    println!("Checkpoint: {} ({})", checkpoint.name, checkpoint.commit);
    Ok(())
}
