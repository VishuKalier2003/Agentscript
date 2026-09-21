use std::fs;

use crate::model::Rule;
use crate::policy::parse_file;
use crate::repository::{ensure_initialized, root};
use crate::util::io_error;

pub(crate) fn run() -> Result<(), String> {
    ensure_initialized()?;
    let directory = root()?.join("policies");
    let paths = fs::read_dir(directory)
        .map_err(io_error)?
        .filter_map(|entry| entry.ok().map(|value| value.path()))
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("crane"))
        .collect::<Vec<_>>();
    let mut policies = paths
        .into_iter()
        .map(|path| parse_file(&path))
        .collect::<Result<Vec<_>, _>>()?;
    policies.sort_by(|left, right| left.name.cmp(&right.name));
    println!("CRANE_CONTEXT_V1");
    for policy in policies {
        println!(
            "\npolicy_id: {}\ncheckpoint: {}",
            policy.name, policy.checkpoint
        );
        for rule in policy.rules {
            let Rule::PreserveFunction { target } = rule;
            println!("rule: preserve\ntarget: {target}");
        }
    }
    println!("\nverification: crane check --agent");
    Ok(())
}
