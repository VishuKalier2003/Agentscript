use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::model::{ChangeType, ItemKind, Rule, Scope};
use crate::policy::parse;
use crate::repository::{load_checkpoint, root};
use crate::util::{io_error, sha256};

/** Version of the IR layout stored in sessions; a session written with another layout is rejected
 * (2 added contract hashes and made the set version cover every clause)
*/
pub(crate) const IR_FORMAT: u64 = 2;

/** What a clause lets an agent do before a tool runs
 * Variants
    - DenyWrite - the agent has no authority to mutate the covered code (from preserve)
    - PermitWrite - the agent is authorized to mutate the covered code (from target); this is
      not an allowlist, code covered by no clause stays writable
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Permission {
    DenyWrite,
    PermitWrite,
}

/** What must hold for the covered code once the agent is done, compared with the checkpoint
 * Variants
    - Unchanged - the code must equal its checkpoint version (from preserve)
    - Changed(Option<ChangeType>) - the code must differ, with that kind of change when given
      (from target)
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Postcondition {
    Unchanged,
    Changed(Option<ChangeType>),
}

/** One compiled rule as a two-sided constraint: a runtime side (may the agent mutate the covered
 * code) and a postcondition side (what must hold at the end); both sides come from the same rule
 * and are private, so a clause can only be built by from_rule and its sides can never disagree
 * (preserve is DenyWrite plus Unchanged, target is PermitWrite plus Changed)
 * Fields
    - kind: ItemKind - kind of the item the clause is anchored on
    - target: String - qualified name of that item
    - scope: Scope - how much code around the item the clause covers
    - permission: Permission - runtime authority over the covered code
    - postcondition: Postcondition - state the covered code must reach
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Clause {
    pub(crate) kind: ItemKind,
    pub(crate) target: String,
    pub(crate) scope: Scope,
    permission: Permission,
    postcondition: Postcondition,
}

impl Clause {
    /** Compile a parsed rule into a clause, by mapping preserve to DenyWrite plus Unchanged and
     * target to PermitWrite plus Changed with its change type
     * Input
        - rule: &Rule - rule from the parser
     * Output
        - Clause
    */
    pub(crate) fn from_rule(rule: &Rule) -> Self {
        match rule {
            Rule::Preserve {
                kind,
                target,
                scope,
            } => Self {
                kind: *kind,
                target: target.clone(),
                scope: *scope,
                permission: Permission::DenyWrite,
                postcondition: Postcondition::Unchanged,
            },
            Rule::Target {
                kind,
                target,
                scope,
                change_type,
            } => Self {
                kind: *kind,
                target: target.clone(),
                scope: *scope,
                permission: Permission::PermitWrite,
                postcondition: Postcondition::Changed(*change_type),
            },
        }
    }

    /** Return the runtime side of the clause
     * Input
        - None (uses self)
     * Output
        - Permission, DenyWrite for preserve and PermitWrite for target
    */
    pub(crate) fn permission(&self) -> Permission {
        self.permission
    }

    /** Return the postcondition side of the clause
     * Input
        - None (uses self)
     * Output
        - Postcondition, Unchanged for preserve and Changed for target
    */
    pub(crate) fn postcondition(&self) -> Postcondition {
        self.postcondition
    }

    /** Turn the clause back into the rule it was compiled from, for policy-syntax descriptions
     * Input
        - None (uses self)
     * Output
        - Rule
    */
    pub(crate) fn rule(&self) -> Rule {
        match self.postcondition {
            Postcondition::Unchanged => Rule::Preserve {
                kind: self.kind,
                target: self.target.clone(),
                scope: self.scope,
            },
            Postcondition::Changed(change_type) => Rule::Target {
                kind: self.kind,
                target: self.target.clone(),
                scope: self.scope,
                change_type,
            },
        }
    }

    /** Return the policy keyword the clause came from, used as the rule field of violations
     * Input
        - None (uses self)
     * Output
        - &'static str, "preserve" or "target"
    */
    pub(crate) fn keyword(&self) -> &'static str {
        match self.postcondition {
            Postcondition::Unchanged => "preserve",
            Postcondition::Changed(_) => "target",
        }
    }

    /** Serialize the clause, by writing its rule fields plus the derived runtime and
     * postcondition fields so a reader of a session file sees both sides of the contract
     * Input
        - None (uses self)
     * Output
        - Value JSON object
    */
    fn to_json(&self) -> Value {
        let change_type = match self.postcondition {
            Postcondition::Changed(Some(change_type)) => Value::from(change_type.name()),
            _ => Value::Null,
        };
        json!({
            "rule": self.keyword(),
            "kind": self.kind.noun(),
            "target": self.target,
            "scope": self.scope.name(),
            "change_type": change_type,
            "runtime": match self.permission {
                Permission::DenyWrite => "deny_write",
                Permission::PermitWrite => "permit_write",
            },
            "postcondition": match self.postcondition {
                Postcondition::Unchanged => "unchanged",
                Postcondition::Changed(_) => "changed",
            },
        })
    }

    /** Read a clause back from a session file, by rebuilding it from its rule fields and then
     * rejecting it when the stored runtime or postcondition fields disagree with that rule
     * Input
        - value: &Value - JSON object written by to_json
     * Output
        - Result<Clause, String>
        - Error if a field is missing or invalid
    */
    fn from_json(value: &Value) -> Result<Self, String> {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("clause is missing '{key}'"))
        };
        let kind = ItemKind::from_flag(&format!("--{}", text("kind")?))
            .ok_or("clause has an invalid kind")?;
        let target = text("target")?.to_string();
        let scope = Scope::parse(text("scope")?)?;
        let rule = match text("rule")? {
            "preserve" => Rule::Preserve {
                kind,
                target,
                scope,
            },
            "target" => Rule::Target {
                kind,
                target,
                scope,
                change_type: value
                    .get("change_type")
                    .and_then(Value::as_str)
                    .map(ChangeType::parse)
                    .transpose()?,
            },
            other => return Err(format!("clause has an invalid rule '{other}'")),
        };
        let clause = Self::from_rule(&rule);
        if clause.to_json() != *value {
            return Err("clause runtime and postcondition fields do not match its rule".into());
        }
        Ok(clause)
    }
}

/** One compiled policy file
 * Fields
    - policy_id: String - policy name
    - version: String - SHA-256 of the policy file's bytes
    - checkpoint: String - checkpoint name
    - checkpoint_sha: Result<String, String> - commit recorded in the checkpoint, or why the
      checkpoint could not be loaded
    - clauses: Vec<Clause> - compiled rules in source order
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Contract {
    pub(crate) policy_id: String,
    pub(crate) version: String,
    pub(crate) checkpoint: String,
    pub(crate) checkpoint_sha: Result<String, String>,
    pub(crate) clauses: Vec<Clause>,
}

impl Contract {
    /** Compute the contract hash, by hashing a canonical JSON manifest (sorted keys) of the
     * contract id, policy version, checkpoint name and commit, and every clause with both of its
     * sides; it depends only on the compiled contract, never on file order or on how a session
     * file was written, so it is stable across compilations and machines
     * Input
        - None (uses self)
     * Output
        - String SHA-256 digest
    */
    pub(crate) fn hash(&self) -> String {
        let manifest = json!({
            "ir_format": IR_FORMAT,
            "contract_id": self.policy_id,
            "policy_version": self.version,
            "checkpoint": self.checkpoint,
            "checkpoint_sha": self.checkpoint_sha.as_deref().ok(),
            "clauses": self.clauses.iter().map(Clause::to_json).collect::<Vec<_>>(),
        });
        sha256(manifest.to_string().as_bytes())
    }
}

/** A policy file that did not parse, kept so it fails closed instead of disappearing
 * Fields
    - policy_id: String - file name without the extension
    - version: String - SHA-256 of the file's bytes
    - message: String - "PATH: parse error"
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Malformed {
    pub(crate) policy_id: String,
    pub(crate) version: String,
    pub(crate) message: String,
}

/** Every policy of the repository, compiled; this is what runtime authority, verification, and
 * sessions share
 * Fields
    - version: String - SHA-256 over every contract hash and malformed policy, so any policy
      edit, clause change, or re-baseline produces a new version
    - contracts: Vec<Contract> - parsed policies sorted by name
    - malformed: Vec<Malformed> - policies that did not parse, in file order
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContractSet {
    pub(crate) version: String,
    pub(crate) contracts: Vec<Contract>,
    pub(crate) malformed: Vec<Malformed>,
}

impl ContractSet {
    /** Create a set from compiled contracts and malformed policies, computing its version
     * Input
        - contracts: Vec<Contract> - compiled policies
        - malformed: Vec<Malformed> - unparsable policies
     * Output
        - ContractSet
    */
    pub(crate) fn new(contracts: Vec<Contract>, malformed: Vec<Malformed>) -> Self {
        Self {
            version: set_version(&contracts, &malformed),
            contracts,
            malformed,
        }
    }

    /** Visit every clause with its contract and its position in policy order, the order that
     * runtime grants and verification results use
     * Input
        - None (uses self)
     * Output
        - Iterator of (index, &Contract, &Clause)
    */
    pub(crate) fn clauses(&self) -> impl Iterator<Item = (usize, &Contract, &Clause)> {
        self.contracts
            .iter()
            .flat_map(|contract| {
                contract
                    .clauses
                    .iter()
                    .map(move |clause| (contract, clause))
            })
            .enumerate()
            .map(|(index, (contract, clause))| (index, contract, clause))
    }

    /** Serialize the set for a session file
     * Input
        - None (uses self)
     * Output
        - Value JSON object
    */
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "ir_format": IR_FORMAT,
            "version": self.version,
            "contracts": self.contracts.iter().map(|contract| {
                let (sha, error) = match &contract.checkpoint_sha {
                    Ok(sha) => (Value::from(sha.as_str()), Value::Null),
                    Err(error) => (Value::Null, Value::from(error.as_str())),
                };
                json!({
                    "policy_id": contract.policy_id,
                    "contract_hash": contract.hash(),
                    "version": contract.version,
                    "checkpoint": contract.checkpoint,
                    "checkpoint_sha": sha,
                    "checkpoint_error": error,
                    "clauses": contract.clauses.iter().map(Clause::to_json).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>(),
            "malformed": self.malformed.iter().map(|policy| json!({
                "policy_id": policy.policy_id,
                "version": policy.version,
                "message": policy.message,
            })).collect::<Vec<_>>(),
        })
    }

    /** Read a set back from a session file, by checking the IR format, rebuilding every contract
     * (rejecting one whose stored hash differs from its recomputed hash) and malformed entry, and
     * finally recomputing the set version so a tampered file is rejected
     * Input
        - value: &Value - JSON object written by to_json
     * Output
        - Result<ContractSet, String>
        - Error if the format is unknown, a field is invalid, or the version does not match
    */
    pub(crate) fn from_json(value: &Value) -> Result<Self, String> {
        if value.get("ir_format").and_then(Value::as_u64) != Some(IR_FORMAT) {
            return Err(format!(
                "unsupported contract IR format; expected {IR_FORMAT}"
            ));
        }
        let text = |value: &Value, key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(String::from)
                .ok_or_else(|| format!("contract IR is missing '{key}'"))
        };
        let list = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_array)
                .ok_or_else(|| format!("contract IR is missing '{key}'"))
        };
        let mut contracts = Vec::new();
        for contract in list("contracts")? {
            let checkpoint_sha = match text(contract, "checkpoint_sha") {
                Ok(sha) => Ok(sha),
                Err(_) => Err(text(contract, "checkpoint_error")?),
            };
            let compiled = Contract {
                policy_id: text(contract, "policy_id")?,
                version: text(contract, "version")?,
                checkpoint: text(contract, "checkpoint")?,
                checkpoint_sha,
                clauses: contract
                    .get("clauses")
                    .and_then(Value::as_array)
                    .ok_or("contract IR is missing 'clauses'")?
                    .iter()
                    .map(Clause::from_json)
                    .collect::<Result<_, _>>()?,
            };
            if text(contract, "contract_hash")? != compiled.hash() {
                return Err(format!(
                    "contract {} hash does not match its contents",
                    compiled.policy_id
                ));
            }
            contracts.push(compiled);
        }
        let malformed = list("malformed")?
            .iter()
            .map(|policy| {
                Ok(Malformed {
                    policy_id: text(policy, "policy_id")?,
                    version: text(policy, "version")?,
                    message: text(policy, "message")?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let version = set_version(&contracts, &malformed);
        if value.get("version").and_then(Value::as_str) != Some(version.as_str()) {
            return Err("contract IR version does not match its contents".into());
        }
        Ok(Self {
            version,
            contracts,
            malformed,
        })
    }

    /** Describe how another contract set (usually the one on disk now) differs from this one, by
     * matching contracts by id and reporting removed and added policies, changed policy versions,
     * checkpoints that were swapped or moved to another commit, and changed malformed policies
     * Input
        - other: &ContractSet - set to compare against
     * Output
        - Vec<String>, one line per difference, empty when the versions are equal
    */
    pub(crate) fn drift(&self, other: &ContractSet) -> Vec<String> {
        if self.version == other.version {
            return Vec::new();
        }
        let short = |sha: &Result<String, String>| match sha {
            Ok(sha) => sha.chars().take(12).collect::<String>(),
            Err(_) => "unavailable".into(),
        };
        let mut lines = Vec::new();
        for bound in &self.contracts {
            let Some(now) = other
                .contracts
                .iter()
                .find(|contract| contract.policy_id == bound.policy_id)
            else {
                lines.push(format!("policy {} was removed", bound.policy_id));
                continue;
            };
            if now.version != bound.version {
                lines.push(format!(
                    "policy {} changed (version {} -> {})",
                    bound.policy_id, bound.version, now.version
                ));
            }
            if now.checkpoint != bound.checkpoint {
                lines.push(format!(
                    "policy {} now uses checkpoint {} instead of {}",
                    bound.policy_id, now.checkpoint, bound.checkpoint
                ));
            } else if now.checkpoint_sha != bound.checkpoint_sha {
                lines.push(format!(
                    "checkpoint {} of policy {} moved from {} to {}",
                    bound.checkpoint,
                    bound.policy_id,
                    short(&bound.checkpoint_sha),
                    short(&now.checkpoint_sha)
                ));
            }
        }
        for added in &other.contracts {
            if !self
                .contracts
                .iter()
                .any(|contract| contract.policy_id == added.policy_id)
            {
                lines.push(format!("policy {} was added", added.policy_id));
            }
        }
        let malformed = |set: &ContractSet| {
            set.malformed
                .iter()
                .map(|policy| format!("{} {}", policy.policy_id, policy.version))
                .collect::<Vec<_>>()
        };
        if malformed(self) != malformed(other) {
            lines.push("the set of malformed policies changed".into());
        }
        if lines.is_empty() {
            lines.push(format!(
                "contract version changed from {} to {}",
                self.version, other.version
            ));
        }
        lines
    }
}

/** Compile every policy in .crane/policies, by first listing the .crane files in path order, then
 * hashing and parsing each one (keeping unparsable files as Malformed), binding each parsed policy
 * to the full commit SHA of its checkpoint, sorting policies by name, and finally computing the
 * set version
 * Input
    - None
 * Output
    - Result<ContractSet, String>
    - Error if .crane or its policies directory cannot be read
*/
pub(crate) fn compile() -> Result<ContractSet, String> {
    let directory = root()?.join("policies");
    let mut paths = fs::read_dir(directory)
        .map_err(io_error)?
        .map(|entry| entry.map(|value| value.path()).map_err(io_error))
        .collect::<Result<Vec<PathBuf>, _>>()?;
    paths.sort(); // gives deterministic order, preventing any ambiguity
    let mut contracts = Vec::new();
    let mut malformed = Vec::new();
    for path in paths {
        if path.extension().and_then(|value| value.to_str()) != Some("crane") {
            continue;
        }
        let bytes = fs::read(&path).map_err(io_error)?;
        let version = sha256(&bytes);
        let parsed = String::from_utf8(bytes)
            .map_err(io_error)
            .and_then(|content| parse(&content))
            .map_err(|error| format!("{}: {error}", path.display()));
        match parsed {
            Ok(policy) => contracts.push(Contract {
                checkpoint_sha: load_checkpoint(&policy.checkpoint).and_then(|value| {
                    checkpoint_commit(&policy.checkpoint, &value.name, &value.commit)
                }),
                clauses: policy.rules.iter().map(Clause::from_rule).collect(),
                policy_id: policy.name,
                version,
                checkpoint: policy.checkpoint,
            }),
            Err(message) => malformed.push(Malformed {
                policy_id: path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("unknown")
                    .into(),
                version,
                message,
            }),
        }
    }
    contracts.sort_by(|left, right| left.policy_id.cmp(&right.policy_id));
    Ok(ContractSet::new(contracts, malformed))
}

/** Check that a checkpoint names itself and records an immutable baseline, by requiring the name
 * stored in the checkpoint file to match the file and the commit to be a full lowercase SHA-1 or
 * SHA-256 object id; a branch, tag, HEAD, or abbreviated id could later resolve to a different
 * commit, so it is rejected instead of being bound
 * Input
    - name: &str - checkpoint name used by the policy (the file name)
    - stored_name: &str - name recorded inside the checkpoint file
    - commit: &str - commit recorded inside the checkpoint file
 * Output
    - Result<String, String> the commit SHA
    - Error if the names differ or the commit is not a full object id
*/
pub(crate) fn checkpoint_commit(
    name: &str,
    stored_name: &str,
    commit: &str,
) -> Result<String, String> {
    if stored_name != name {
        return Err(format!(
            "checkpoint file {name}.json records checkpoint '{stored_name}'; recreate it with 'crane checkpoint --name {name}'"
        ));
    }
    let full = matches!(commit.len(), 40 | 64)
        && commit
            .chars()
            .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character));
    if !full {
        return Err(format!(
            "checkpoint '{name}' records '{commit}', which is not a full commit SHA; a baseline must not move, so recreate it with 'crane checkpoint --name {name}'"
        ));
    }
    Ok(commit.into())
}

/** Compute the version of a contract set, by hashing one line per contract (id and contract hash,
 * which covers the policy version, checkpoint, and every clause) and per malformed policy (id and
 * version)
 * Input
    - contracts: &[Contract] - compiled policies
    - malformed: &[Malformed] - unparsable policies
 * Output
    - String SHA-256 digest
*/
fn set_version(contracts: &[Contract], malformed: &[Malformed]) -> String {
    let mut manifest = format!("crane-ir {IR_FORMAT}\n");
    for contract in contracts {
        manifest.push_str(&format!(
            "contract {} {}\n",
            contract.policy_id,
            contract.hash()
        ));
    }
    for policy in malformed {
        manifest.push_str(&format!(
            "malformed {} {}\n",
            policy.policy_id, policy.version
        ));
    }
    sha256(manifest.as_bytes())
}
