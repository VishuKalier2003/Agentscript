// Policy activation state: which persistent policies are active in the repository, in which
// layer (organization or repository), at which digests, and the policy version task contracts bind
// to. Task contracts are a layer of their own and are left out of that version, so approving or
// retiring one never invalidates another task's contract. Every change of the persistent version
// is recorded in .crane/policy-activation.json.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;

use serde_json::{json, Value};

use crate::evidence::{load_organization, ORGANIZATION_FILE};
use crate::policy::parse;
use crate::proposals::store::{agent_environment, Proposal};
use crate::repository::root;
use crate::util::{io_error, now_unix, sha256};

/** File in .crane recording the activation state and its history */
pub(crate) const STATE_FILE: &str = "policy-activation.json";

/** Version of the activation state layout */
const STATE_FORMAT: u64 = 1;

/** Read the organization configuration: the organization and team sessions belong to, and the
 * policies the organization declares as its own (.crane/organization.json "policies"), with a
 * digest covering all of it
 * Input
    - None
 * Output
    - Result<Value, String> {organization, team, policies, digest}
    - Error if the file is invalid
*/
pub(crate) fn organization() -> Result<Value, String> {
    let identity = load_organization()?;
    let policies = match fs::read_to_string(root()?.join(ORGANIZATION_FILE)) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text)
                .map_err(|error| format!(".crane/{ORGANIZATION_FILE}: {error}"))?;
            match value.get("policies") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|item| {
                        item.as_str().map(String::from).ok_or_else(|| {
                            format!(".crane/{ORGANIZATION_FILE}: policies must be a list of policy names")
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Some(_) => {
                    return Err(format!(
                        ".crane/{ORGANIZATION_FILE}: policies must be a list of policy names"
                    ))
                }
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(io_error(error)),
    };
    let policies = policies.into_iter().collect::<BTreeSet<_>>();
    let mut value = json!({
        "organization": identity["organization"],
        "team": identity["team"],
        "policies": policies,
    });
    value["digest"] = json!(sha256(value.to_string().as_bytes()));
    Ok(value)
}

/** Decide a policy's layer: organization when the organization declares it, task when it is a
 * task contract (approved from a task proposal, or named task_*), repository otherwise
 * Input
    - name: &str - policy name (file stem)
    - declared: &BTreeSet<String> - organization policies
    - proposal: Option<&Proposal> - the proposal it was approved from, if any
 * Output
    - &'static str
*/
fn layer(name: &str, declared: &BTreeSet<String>, proposal: Option<&Proposal>) -> &'static str {
    if declared.contains(name) {
        "organization"
    } else if name.starts_with("task_")
        || proposal.is_some_and(|proposal| proposal.document["origin"]["kind"] == "task")
    {
        "task"
    } else {
        "repository"
    }
}

/** Compute the activation state of the policies in .crane/policies: each policy with its layer,
 * digest, checkpoint, rule count, and origin (the approved proposal or a hand-written file), the
 * version of each persistent layer, and the persistent policy version (organization and
 * repository layers; malformed policies count, since they fail closed)
 * Input
    - None
 * Output
    - Result<Value, String>
    - Error if .crane or its policies cannot be read
*/
pub(crate) fn current() -> Result<Value, String> {
    let organization = organization()?;
    let declared = organization["policies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str().map(String::from))
        .collect::<BTreeSet<_>>();
    let directory = root()?.join("policies");
    let mut paths = match fs::read_dir(&directory) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("crane"))
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(io_error(error)),
    };
    paths.sort();
    let mut policies = Vec::new();
    for path in paths {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string();
        let bytes = fs::read(&path).map_err(io_error)?;
        let proposal = Proposal::load(&name)
            .ok()
            .filter(|proposal| proposal.status() == "approved");
        let mut entry = json!({
            "policy": name,
            "layer": layer(&name, &declared, proposal.as_ref()),
            "file": format!("policies/{name}.crane"),
            "digest": sha256(&bytes),
            "origin": match &proposal {
                Some(proposal) => json!({
                    "proposal": proposal.name,
                    "kind": proposal.document["origin"]["kind"],
                    "task_id": proposal.document["origin"]["task_id"],
                    "approver": proposal.document["activation"]["approver"],
                    "at": proposal.document["activation"]["at"],
                }),
                None => json!("hand-written"),
            },
        });
        match String::from_utf8(bytes)
            .map_err(|error| error.to_string())
            .and_then(|text| parse(&text))
        {
            Ok(policy) => {
                entry["checkpoint"] = json!(policy.checkpoint);
                entry["rules"] = json!(policy.rules.len());
            }
            Err(error) => entry["malformed"] = json!(error),
        }
        policies.push(entry);
    }
    let version_of = |layers: &[&str]| {
        let mut manifest = String::from("crane-policies 1\n");
        for policy in &policies {
            if layers.contains(&policy["layer"].as_str().unwrap_or_default()) {
                manifest.push_str(&format!(
                    "{} {} {}\n",
                    policy["layer"].as_str().unwrap_or_default(),
                    policy["policy"].as_str().unwrap_or_default(),
                    policy["digest"].as_str().unwrap_or_default()
                ));
            }
        }
        sha256(manifest.as_bytes())
    };
    let names = |layer: &str| {
        policies
            .iter()
            .filter(|policy| policy["layer"] == layer)
            .map(|policy| policy["policy"].clone())
            .collect::<Vec<_>>()
    };
    let missing = declared
        .iter()
        .filter(|name| {
            !policies
                .iter()
                .any(|policy| policy["policy"] == name.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    Ok(json!({
        "policy_activation_format": STATE_FORMAT,
        "policy_version": version_of(&["organization", "repository"]),
        "layers": {
            "organization": {"version": version_of(&["organization"]), "policies": names("organization"), "declared_but_missing": missing},
            "repository": {"version": version_of(&["repository"]), "policies": names("repository")},
            "task": {"policies": names("task")},
        },
        "policies": policies,
        "organization": organization,
    }))
}

/** Return the persistent policy version task contracts bind to
 * Input
    - None
 * Output
    - Result<String, String>
*/
pub(crate) fn policy_version() -> Result<String, String> {
    Ok(current()?["policy_version"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

/** Compute the activation state and record it: when the persistent policy version differs from
 * the last one recorded, a history entry lists which policies were activated, changed, or
 * deactivated; nothing is written on behalf of an agent (the state is still returned)
 * Input
    - None
 * Output
    - Result<Value, String> the state with its history
*/
pub(crate) fn record() -> Result<Value, String> {
    let mut state = current()?;
    let path = root()?.join(STATE_FILE);
    let stored = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map_err(|error| format!("{STATE_FILE}: {error}"))?,
        Err(error) if error.kind() == ErrorKind::NotFound => json!({}),
        Err(error) => return Err(io_error(error)),
    };
    let mut history = stored["history"].as_array().cloned().unwrap_or_default();
    let previous = stored["policy_version"].as_str().map(String::from);
    let changed = previous.as_deref() != state["policy_version"].as_str();
    if changed {
        let persistent = |value: &Value| {
            value["policies"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|policy| policy["layer"] != "task")
                .map(|policy| {
                    (
                        policy["policy"].as_str().unwrap_or_default().to_string(),
                        (policy["layer"].clone(), policy["digest"].clone()),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let before = persistent(&stored);
        let after = persistent(&state);
        let mut changes = Vec::new();
        for (name, (layer, digest)) in &after {
            match before.get(name) {
                None => changes.push(json!({"policy": name, "layer": layer, "change": "activated", "digest": digest})),
                Some(old) if old != &(layer.clone(), digest.clone()) => changes.push(json!({"policy": name, "layer": layer, "change": "changed", "digest": digest, "previous_digest": old.1})),
                _ => {}
            }
        }
        for (name, (layer, digest)) in &before {
            if !after.contains_key(name) {
                changes.push(json!({"policy": name, "layer": layer, "change": "deactivated", "previous_digest": digest}));
            }
        }
        history.push(json!({
            "at": now_unix(),
            "policy_version": state["policy_version"],
            "previous_version": previous,
            "changes": changes,
        }));
    }
    state["history"] = json!(history);
    let recorded = agent_environment().is_none();
    if changed && recorded {
        let temporary = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(
            &temporary,
            serde_json::to_string_pretty(&state).map_err(io_error)? + "\n",
        )
        .map_err(io_error)?;
        fs::rename(&temporary, &path).map_err(io_error)?;
    }
    state["recorded"] = json!(recorded);
    Ok(state)
}
