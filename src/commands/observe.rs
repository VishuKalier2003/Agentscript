use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};

use serde_json::{json, Value};

use crate::observe::access::{registry_file, viewers, RESERVED, VIEWERS_FORMAT};
use crate::observe::{route, server::serve};
use crate::proposals::store::require_human;
use crate::repository::ensure_initialized;
use crate::util::{option, sha256};

/** Usage of crane observe */
const USAGE: &str = "crane observe [PATH] (for example /api/v1/overview or /api/v1/decisions?decision=deny) | crane observe serve [--addr HOST:PORT] [--once] | crane observe viewers | crane observe viewer add NAME --organization ORG[,ORG] --repository OWNER/NAME[,...] [--team TEAM[,TEAM]] | crane observe viewer remove NAME";

/** Answer one read-only observability query from the command line, through the same GET-only
 * router the observability server uses, serve that API, or manage who may read it
 * Input
    - args: &[String] - an API path (default /api/v1/overview), serve and its options, viewers,
      or viewer add/remove
 * Output
    - Result<(), String>
    - Error for an API status other than 200
*/
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    ensure_initialized()?;
    match args.first().map(String::as_str) {
        Some("serve") => serve(
            &option(args, "--addr").unwrap_or_else(|| "127.0.0.1:8791".into()),
            args.iter().any(|argument| argument == "--once"),
        ),
        Some("viewers") => list(),
        Some("viewer") => match args.get(1).map(String::as_str) {
            Some("add") => add(args),
            Some("remove") => remove(args),
            _ => Err(format!("use '{USAGE}'")),
        },
        Some(flag) if flag.starts_with("--") => {
            Err(format!("unknown observe option '{flag}'; use '{USAGE}'"))
        }
        path => {
            if args.len() > 1 {
                return Err(format!("use '{USAGE}'"));
            }
            let path = path.unwrap_or("/api/v1/overview");
            let (status, value) = route("GET", path);
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
            );
            if status == 200 {
                Ok(())
            } else {
                Err(format!("GET {path} answered {status}"))
            }
        }
    }
}

/** List the registered viewers and what each may read (never a token or its digest)
 * Input
    - None
 * Output
    - Result<(), String>
*/
fn list() -> Result<(), String> {
    let registered = viewers()?;
    if registered.is_empty() {
        println!(
            "no viewers; the operator token printed by 'crane observe serve' reads everything"
        );
    }
    for (name, _, scope) in registered {
        let names = |value: Value| {
            value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default()
        };
        println!(
            "{name}: organizations {} · repositories {} · teams {}",
            names(scope.organizations.to_json()),
            names(scope.repositories.to_json()),
            names(scope.teams.to_json())
        );
    }
    Ok(())
}

/** Read a comma-separated option as a list of names
 * Input
    - args: &[String] - arguments
    - key: &str - option
 * Output
    - Option<Vec<String>>
*/
fn names(args: &[String], key: &str) -> Option<Vec<String>> {
    option(args, key).map(|text| {
        text.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(String::from)
            .collect()
    })
}

/** Draw a new viewer token: 256 bits from the operating system's randomness (each standard
 * library hasher is keyed from it), as hex
 * Input
    - None
 * Output
    - String
*/
fn token() -> String {
    (0..4)
        .map(|index| {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_usize(index);
            format!("{:016x}", hasher.finish())
        })
        .collect()
}

/** Write the viewer registry, through a temporary file so a reader never sees half of it, in the
 * runtime directory kept out of Git
 * Input
    - registry: &Value - the registry
 * Output
    - Result<(), String>
*/
fn save(registry: &Value) -> Result<(), String> {
    let path = registry_file()?;
    let directory = path.parent().ok_or("invalid registry path")?;
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let runtime = directory.parent().ok_or("invalid registry path")?;
    if !runtime.join(".gitignore").exists() {
        fs::write(runtime.join(".gitignore"), "*\n").map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(registry).map_err(|error| error.to_string())?;
    fs::write(&temporary, text + "\n").map_err(|error| error.to_string())?;
    fs::rename(&temporary, &path).map_err(|error| error.to_string())
}

/** Read the registry document as stored (an absent registry is empty)
 * Input
    - None
 * Output
    - Result<Value, String>
*/
fn stored() -> Result<Value, String> {
    viewers()?;
    match fs::read_to_string(registry_file()?) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| error.to_string()),
        Err(_) => Ok(json!({"viewers_format": VIEWERS_FORMAT, "viewers": []})),
    }
}

/** Register a viewer: a new token (printed once; only its SHA-256 digest is kept) that may read
 * the given organizations and repositories, and optionally only some teams' sessions and tasks
 * Input
    - args: &[String] - viewer add NAME --organization ... --repository ... [--team ...]
 * Output
    - Result<(), String>
*/
fn add(args: &[String]) -> Result<(), String> {
    require_human("grant read access to the observability API")?;
    let name = args
        .get(2)
        .filter(|name| !name.starts_with("--") && !RESERVED.contains(&name.as_str()))
        .filter(|name| {
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
        .ok_or_else(|| format!("name the viewer with letters, digits, '-', '_', or '.' (not operator or nobody); use '{USAGE}'"))?;
    let organizations = names(args, "--organization")
        .filter(|list| !list.is_empty())
        .ok_or("a viewer needs --organization (\"*\" for any)")?;
    let repositories = names(args, "--repository")
        .filter(|list| !list.is_empty())
        .ok_or("a viewer needs --repository OWNER/NAME (\"*\" for any)")?;
    let teams = names(args, "--team");
    let mut registry = stored()?;
    let list = registry["viewers"]
        .as_array_mut()
        .ok_or("the viewer registry has no viewer list")?;
    if list.iter().any(|viewer| viewer["name"] == name.as_str()) {
        return Err(format!(
            "viewer '{name}' exists; remove it first to issue a new token"
        ));
    }
    let secret = token();
    let mut viewer = json!({
        "name": name,
        "token_sha256": sha256(secret.as_bytes()),
        "organizations": organizations,
        "repositories": repositories,
        "created_at": crate::util::now_unix(),
    });
    if let Some(teams) = teams.filter(|list| !list.is_empty()) {
        viewer["teams"] = json!(teams);
    }
    list.push(viewer);
    save(&registry)?;
    println!("viewer '{name}' may read the observability API with this token (shown once; Crane keeps only its SHA-256 digest):");
    println!("{secret}");
    Ok(())
}

/** Remove a viewer, revoking its token
 * Input
    - args: &[String] - viewer remove NAME
 * Output
    - Result<(), String>
*/
fn remove(args: &[String]) -> Result<(), String> {
    require_human("revoke read access to the observability API")?;
    let name = args.get(2).ok_or_else(|| format!("use '{USAGE}'"))?;
    let mut registry = stored()?;
    let list = registry["viewers"]
        .as_array_mut()
        .ok_or("the viewer registry has no viewer list")?;
    let before = list.len();
    list.retain(|viewer| viewer["name"] != name.as_str());
    if list.len() == before {
        return Err(format!("no viewer '{name}'"));
    }
    save(&registry)?;
    println!("viewer '{name}' removed; its token no longer reads anything");
    Ok(())
}
