use std::fs;

use crate::model::{ChangeType, Rule};
use crate::repository::{ensure_initialized, load_checkpoint, root};
use crate::scope::{locate_target, ScopeContext};
use crate::util::{io_error, keyword_option, option, sanitize, validate_identifier};

use super::protect::{item_option, scope_option};

/** Create a target policy for an item, by first reading and validating the item flag
 * (--function, --data, --variable, --class, or --interface) with its target, --policy,
 * --checkpoint, the optional scope ("scope VALUE" or "--scope VALUE"), and the optional change
 * type ("change_type VALUE", "--change_type VALUE", or "--change-type VALUE"), then requiring the
 * target to exist exactly once in the checkpoint, and finally writing the policy file to
 * .crane/policies; unlike protect, the current code is not compared, because a target is expected
 * to change
 * Input
    - args: &[String] - command arguments with an item flag and TARGET, and optional --policy,
      --checkpoint, scope, and change_type
 * Output
    - Result<(), String>
    - Error if an option is missing or invalid, the checkpoint does not exist, or the target is
      missing or ambiguous in the checkpoint
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    let (kind, target) = item_option(args, "target")?;
    let policy_name = option(args, "--policy")
        .unwrap_or_else(|| format!("target_{}", sanitize(&target).to_lowercase()));
    validate_identifier(&policy_name)?; // the name becomes a file name, so no paths
    let checkpoint_name = option(args, "--checkpoint").unwrap_or_else(|| "baseline".into());
    validate_identifier(&checkpoint_name)?;
    let scope = scope_option(args)?;
    let change_type = keyword_option(
        args,
        &["change_type", "--change_type", "--change-type"],
        "logical_bn, logical_cn, logical_sn, or semantic",
    )?
    .map(|value| ChangeType::parse(&value))
    .transpose()?;

    // The rule must point at a real item in the trusted baseline, or it could never pass
    let checkpoint = load_checkpoint(&checkpoint_name)?;
    let path = locate_target(&mut ScopeContext::new(), &checkpoint.commit, kind, &target)?;

    let rule = Rule::Target {
        kind,
        target: target.clone(),
        scope,
        change_type,
    };
    fs::write(
        root()?
            .join("policies")
            .join(format!("{policy_name}.crane")),
        format!(
            "policy {policy_name} {{\n    checkpoint {checkpoint_name};\n    {};\n}}\n",
            rule.describe()
        ),
    )
    .map_err(io_error)?;
    println!(
        "Created target policy '{policy_name}' for {target} in {path} (scope {}, change_type {})",
        scope.name(),
        change_type.map_or("any", ChangeType::name)
    );
    println!("Checkpoint: {} ({})", checkpoint.name, checkpoint.commit);
    Ok(())
}
