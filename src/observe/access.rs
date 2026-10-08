// Read authorization for the observability API: who may see which tenant's records. A request runs
// under exactly one scope, set by the router before anything is read and checked where the records
// are read (every session, task record, and repository passes through it), so no endpoint can
// return a record its scope does not admit. Without a scope nothing is admitted (fail closed).
//
// The operator (the token the server printed, or the local CLI) sees everything the repository
// records. A viewer is granted organizations, repositories, and optionally teams, in the viewer
// registry (.crane/runtime/observe/viewers.json, kept out of Git), which stores only each token's
// SHA-256 digest; the tenant keys are the organization and team the evidence records already
// carry. This grants reading only: there is no write path to grant.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::util::sha256;

/** Names no registered viewer may take */
pub(crate) const RESERVED: &[&str] = &["operator", "nobody"];

/** Version of the viewer registry layout */
pub(crate) const VIEWERS_FORMAT: u64 = 1;

/** Which values of one tenant key a scope admits
 * Variants
    - Any: every value, including none
    - Only(BTreeSet<String>): exactly these values (a record without the key is not admitted)
*/
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Grant {
    Any,
    Only(BTreeSet<String>),
}

impl Grant {
    /** Read a grant from a registry list: "*" admits any value, a missing list (when optional)
     * admits any value
     * Input
        - value: &Value - list of names
        - required: bool - whether the list must be present
        - key: &str - its name, for errors
     * Output
        - Result<Grant, String>
    */
    fn from_json(value: &Value, required: bool, key: &str) -> Result<Self, String> {
        if value.is_null() && !required {
            return Ok(Self::Any);
        }
        let names = value
            .as_array()
            .ok_or_else(|| format!("'{key}' must be a list of names (\"*\" for any)"))?
            .iter()
            .map(|name| {
                name.as_str()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(String::from)
                    .ok_or_else(|| format!("'{key}' must hold non-empty names"))
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        if names.is_empty() {
            return Err(format!(
                "'{key}' must name at least one value (\"*\" for any)"
            ));
        }
        Ok(if names.contains("*") {
            Self::Any
        } else {
            Self::Only(names)
        })
    }

    /** Check whether the grant admits a value
     * Input
        - value: Option<&str> - the record's value, None when it has none
     * Output
        - bool
    */
    pub(crate) fn admits(&self, value: Option<&str>) -> bool {
        match self {
            Self::Any => true,
            Self::Only(names) => value.is_some_and(|value| names.contains(value)),
        }
    }

    /** Describe the grant (for whoami and the registry listing)
     * Input
        - None (uses self)
     * Output
        - Value - ["*"] or the names
    */
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Any => json!(["*"]),
            Self::Only(names) => json!(names),
        }
    }
}

/** What one request may read
 * Fields
    - viewer: String - who reads ("operator" for the server's own token or the local CLI)
    - organizations: Grant - organizations it may read
    - repositories: Grant - repositories (full names, such as acme/shop) it may read
    - teams: Grant - teams whose sessions and tasks it may read
    - operator: bool - whether it is the operator (who is also told of records whose tenant is
      unknown, such as an unreadable session)
*/
#[derive(Clone, Debug)]
pub(crate) struct Scope {
    pub(crate) viewer: String,
    pub(crate) operator: bool,
    pub(crate) organizations: Grant,
    pub(crate) repositories: Grant,
    pub(crate) teams: Grant,
}

impl Scope {
    /** The operator's scope: everything this repository records
     * Input
        - None
     * Output
        - Scope
    */
    pub(crate) fn operator() -> Self {
        Self {
            viewer: "operator".into(),
            operator: true,
            organizations: Grant::Any,
            repositories: Grant::Any,
            teams: Grant::Any,
        }
    }

    /** The scope used when none was set: nothing is admitted
     * Input
        - None
     * Output
        - Scope
    */
    fn nobody() -> Self {
        let none = || Grant::Only(BTreeSet::new());
        Self {
            viewer: "nobody".into(),
            operator: false,
            organizations: none(),
            repositories: none(),
            teams: none(),
        }
    }

    /** Check whether the scope may read a repository at all
     * Input
        - organization: Option<&str> - the repository's organization
        - repository: &str - its full name
     * Output
        - bool
    */
    pub(crate) fn admits_repository(&self, organization: Option<&str>, repository: &str) -> bool {
        self.organizations.admits(organization) && self.repositories.admits(Some(repository))
    }

    /** Check whether the scope may read a session (or a task) of the repository
     * Input
        - organization: Option<&str> - the organization the record names
        - team: Option<&str> - the team it names
     * Output
        - bool
    */
    pub(crate) fn admits_record(&self, organization: Option<&str>, team: Option<&str>) -> bool {
        self.organizations.admits(organization) && self.teams.admits(team)
    }

    /** Check whether team rows are restricted (so tasks must be checked one by one)
     * Input
        - None (uses self)
     * Output
        - bool
    */
    pub(crate) fn restricts_teams(&self) -> bool {
        self.teams != Grant::Any
    }

    /** Describe the scope, never with any credential
     * Input
        - None (uses self)
     * Output
        - Value
    */
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "viewer": self.viewer,
            "operator": self.operator,
            "organizations": self.organizations.to_json(),
            "repositories": self.repositories.to_json(),
            "teams": self.teams.to_json(),
        })
    }
}

thread_local! {
    /** The scope of the request this thread is answering */
    static CURRENT: RefCell<Option<Arc<Scope>>> = const { RefCell::new(None) };
}

/** Restores the previous scope when a request ends, even when it panics
 * Fields
    - previous: Option<Arc<Scope>> - the scope before
*/
struct Restore {
    previous: Option<Arc<Scope>>,
}

impl Drop for Restore {
    /** Put the previous scope back
     * Input
        - None (uses self)
     * Output
        - None
    */
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|current| *current.borrow_mut() = previous);
    }
}

/** Answer one request under a scope
 * Input
    - scope: Scope - what the request may read
    - work: impl FnOnce() -> T - the request
 * Output
    - T
*/
pub(crate) fn within<T>(scope: Scope, work: impl FnOnce() -> T) -> T {
    let previous = CURRENT.with(|current| current.borrow_mut().replace(Arc::new(scope)));
    let _restore = Restore { previous };
    work()
}

/** Return the scope of the current request; nothing is admitted outside a request
 * Input
    - None
 * Output
    - Arc<Scope>
*/
pub(crate) fn current() -> Arc<Scope> {
    CURRENT
        .with(|current| current.borrow().clone())
        .unwrap_or_else(|| Arc::new(Scope::nobody()))
}

/** Return the viewer registry file, .crane/runtime/observe/viewers.json
 * Input
    - None
 * Output
    - Result<PathBuf, String>
*/
pub(crate) fn registry_file() -> Result<PathBuf, String> {
    Ok(super::crane_root()?
        .join("runtime")
        .join("observe")
        .join("viewers.json"))
}

/** Read the viewer registry (an absent registry has no viewers)
 * Input
    - None
 * Output
    - Result<Vec<(String, String, Scope)>, String> each viewer's name, token digest, and scope
*/
pub(crate) fn viewers() -> Result<Vec<(String, String, Scope)>, String> {
    let path = registry_file()?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let invalid = |problem: String| {
        format!(
            "the viewer registry {} is invalid: {problem}",
            path.display()
        )
    };
    let value: Value = serde_json::from_str(&text).map_err(|error| invalid(error.to_string()))?;
    if value["viewers_format"].as_u64() != Some(VIEWERS_FORMAT) {
        return Err(invalid(format!("expected viewers_format {VIEWERS_FORMAT}")));
    }
    value["viewers"]
        .as_array()
        .ok_or_else(|| invalid("'viewers' must be a list".into()))?
        .iter()
        .map(|viewer| {
            let name = viewer["name"]
                .as_str()
                .filter(|name| !name.is_empty() && !RESERVED.contains(name))
                .ok_or_else(|| {
                    invalid("every viewer needs a name other than operator or nobody".into())
                })?;
            let digest = viewer["token_sha256"]
                .as_str()
                .filter(|digest| digest.starts_with("sha256:") && digest.len() == 71)
                .ok_or_else(|| invalid(format!("viewer '{name}' needs token_sha256")))?;
            let grant = |key: &str, required: bool| {
                Grant::from_json(&viewer[key], required, key)
                    .map_err(|problem| invalid(format!("viewer '{name}': {problem}")))
            };
            Ok((
                name.to_string(),
                digest.to_string(),
                Scope {
                    viewer: name.to_string(),
                    operator: false,
                    organizations: grant("organizations", true)?,
                    repositories: grant("repositories", true)?,
                    teams: grant("teams", false)?,
                },
            ))
        })
        .collect()
}

/** Compare two strings in time independent of where they differ
 * Input
    - left: &str - secret or digest
    - right: &str - secret or digest
 * Output
    - bool
*/
fn same(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/** Authenticate a credential: the operator token, or a registered viewer's token (compared by
 * digest; every viewer is compared, so timing does not reveal which one matched)
 * Input
    - credential: &str - the token the request presented
    - operator: bool - whether it is the server's own token
 * Output
    - Result<Option<Scope>, String> the scope, None for an unknown credential, or an error when
      the registry cannot be read (the request is refused)
*/
pub(crate) fn authenticate(credential: &str, operator: bool) -> Result<Option<Scope>, String> {
    if operator {
        return Ok(Some(Scope::operator()));
    }
    if credential.len() < 16 {
        return Ok(None);
    }
    let digest = sha256(credential.as_bytes());
    let mut found = None;
    for (_, expected, scope) in viewers()? {
        if same(&digest, &expected) && found.is_none() {
            found = Some(scope);
        }
    }
    Ok(found)
}
