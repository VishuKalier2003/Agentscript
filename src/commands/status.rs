use std::fs;

use crate::repository::{ensure_initialized, git, root};
use crate::util::{io_error, json_field};

pub(crate) fn run() -> Result<(), String> {
    // Summarize repository identity, checkpoints, and policies without evaluating rules
    ensure_initialized()?;
    let root = root()?;
    println!("Crane status\n============");
    println!("Git HEAD: {}", git(&["rev-parse", "HEAD"])?);
    println!(
        "Branch: {}",
        git(&["branch", "--show-current"]).unwrap_or_else(|_| "DETACHED".into())
    );
    println!("Checkpoints:");
    let mut any = false;
    for entry in fs::read_dir(root.join("checkpoints")).map_err(io_error)? {
        let path = entry.map_err(io_error)?.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            any = true;
            let commit = json_field(&fs::read_to_string(&path).map_err(io_error)?, "commit")
                .unwrap_or_default();
            println!(
                "  {} -> {commit}",
                path.file_stem().unwrap().to_string_lossy()
            );
        }
    }
    if !any {
        println!("  (none)");
    }
    println!("Policies:");
    let mut any = false;
    for entry in fs::read_dir(root.join("policies")).map_err(io_error)? {
        let path = entry.map_err(io_error)?.path();
        if path.extension().and_then(|value| value.to_str()) == Some("crane") {
            any = true;
            println!("  {}", path.file_stem().unwrap().to_string_lossy());
        }
    }
    if !any {
        println!("  (none)");
    }
    Ok(())
}
