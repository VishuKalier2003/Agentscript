use std::env;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

use super::engine::{candidates, Confidence};
use crate::ir::checkpoint_commit;
use crate::policy::parse;
use crate::repository::{git, load_checkpoint, root};
use crate::util::{io_error, now_unix, sha256, validate_identifier};
use crate::zones::inspect;

/** Version of the proposal file layout */
pub(crate) const PROPOSAL_FORMAT: u64 = 1;

/** Environment variables set by agent hosts (Claude Code, Codex) or by an adapter for agent tool
 * processes; their presence means the command is running on behalf of an agent */
const AGENT_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CRANE_AGENT",
];

/** Number of digest characters an approver must quote */
const CONFIRM_LENGTH: usize = 12;

/** Return the first agent marker set in the environment
 * Input
    - None
 * Output
    - Option<&'static str>
*/
pub(crate) fn agent_environment() -> Option<&'static str> {
    AGENT_MARKERS
        .iter()
        .copied()
        .find(|marker| env::var_os(marker).is_some_and(|value| !value.is_empty()))
}

/** Refuse a review action when it runs on behalf of an agent: an agent may generate proposals,
 * but only a human or a trusted organization process may approve, reject, edit, or regenerate
 * them (the pre-tool hook also denies these commands to agents)
 * Input
    - action: &str - review action
 * Output
    - Result<(), String>
    - Error naming the agent marker found
*/
pub(crate) fn require_human(action: &str) -> Result<(), String> {
    match agent_environment() {
        Some(marker) => Err(format!(
            "crane policy {action} refuses to run in an agent environment ({marker} is set); a human or a trusted organization process must {action} proposals"
        )),
        None => Ok(()),
    }
}

/** Return the proposals directory, .crane/proposals
 * Input
    - None
 * Output
    - Result<PathBuf, String>
    - Error if .crane cannot be found
*/
fn directory() -> Result<PathBuf, String> {
    Ok(root()?.join("proposals"))
}

/** Generate the content of a proposal from the current repository: candidates at or above the
 * minimum confidence, the AgentScript policy made of the uncovered candidates' suggested rules
 * (validated with the policy parser), zone suggestions for regions, and the source state
 * Input
    - name: &str - policy name
    - checkpoint: &str - checkpoint the policy binds to (must exist and be valid)
    - minimum: Confidence - lowest confidence included
 * Output
    - Result<(Value, String, Value, Value), String> candidates, policy text, zone suggestions, and
      source
    - Error if the checkpoint is unusable or no candidate yields a rule
*/
fn generate(
    name: &str,
    checkpoint: &str,
    minimum: Confidence,
) -> Result<(Value, String, Value, Value), String> {
    let stored = load_checkpoint(checkpoint)?;
    checkpoint_commit(checkpoint, &stored.name, &stored.commit)?;
    let zones = inspect()?;
    let found = candidates(&zones.inventory, Some(&zones.resolution));
    let selected = found
        .iter()
        .filter(|candidate| candidate.confidence >= minimum)
        .collect::<Vec<_>>();
    let mut rules: Vec<&str> = Vec::new();
    for candidate in &selected {
        if let Some(rule) = candidate
            .suggested_rule
            .as_deref()
            .filter(|_| candidate.covered_by.is_empty())
        {
            if !rules.contains(&rule) {
                rules.push(rule);
            }
        }
    }
    if rules.is_empty() {
        return Err(format!(
            "no uncovered candidate at {} confidence or above has a rule to propose",
            minimum.name()
        ));
    }
    let mut policy = format!("policy {name} {{\n    checkpoint {checkpoint};\n");
    for rule in &rules {
        policy.push_str(&format!("    {rule}\n"));
    }
    policy.push_str("}\n");
    parse(&policy).map_err(|error| format!("generated policy is invalid: {error}"))?;
    let listed = selected
        .iter()
        .map(|candidate| {
            let mut value = candidate.to_json();
            value["included"] = json!(
                candidate.covered_by.is_empty()
                    && candidate
                        .suggested_rule
                        .as_deref()
                        .is_some_and(|rule| rules.contains(&rule))
            );
            value
        })
        .collect::<Vec<_>>();
    let regions = selected
        .iter()
        .filter_map(|candidate| {
            Some(json!({
                "candidate": candidate.id,
                "confidence": candidate.confidence.name(),
                "reason": candidate.reason(),
                "zone": candidate.suggested_zone.as_ref()?,
            }))
        })
        .collect::<Vec<_>>();
    let source = json!({
        "head": zones.inventory.head,
        "contract_version": zones.inventory.contract_version,
        "zone_set_version": zones.version,
        "generated_in_agent_environment": agent_environment(),
    });
    Ok((json!(listed), policy, json!(regions), source))
}

/** Identify who runs a review action: the given name, else Git's user.email
 * Input
    - given: Option<String> - --approver or --by value
 * Output
    - String
*/
fn actor(given: Option<String>) -> String {
    given.unwrap_or_else(|| git(&["config", "user.email"]).unwrap_or_else(|_| "unknown".into()))
}

/** A proposal on disk: the JSON record and the candidate AgentScript next to it
 * Fields
    - name: String - proposal id, which is also the policy name
    - document: Value - .crane/proposals/NAME.json
*/
pub(crate) struct Proposal {
    pub(crate) name: String,
    pub(crate) document: Value,
}

impl Proposal {
    /** Return the JSON record path
     * Input
        - name: &str - proposal id
     * Output
        - Result<PathBuf, String>
    */
    fn record(name: &str) -> Result<PathBuf, String> {
        Ok(directory()?.join(format!("{name}.json")))
    }

    /** Return the candidate policy path, .crane/proposals/NAME.crane (not in .crane/policies,
     * so it is never compiled or enforced)
     * Input
        - name: &str - proposal id
     * Output
        - Result<PathBuf, String>
    */
    fn candidate_file(name: &str) -> Result<PathBuf, String> {
        Ok(directory()?.join(format!("{name}.crane")))
    }

    /** Load a proposal
     * Input
        - name: &str - proposal id
     * Output
        - Result<Proposal, String>
        - Error if there is no such proposal or its record is invalid
    */
    pub(crate) fn load(name: &str) -> Result<Self, String> {
        validate_identifier(name)?;
        let content =
            fs::read_to_string(Self::record(name)?).map_err(|error| match error.kind() {
                ErrorKind::NotFound => {
                    format!("no proposal '{name}'; list them with 'crane policy proposals'")
                }
                _ => io_error(error),
            })?;
        let document: Value = serde_json::from_str(&content)
            .map_err(|error| format!("proposal {name} is invalid: {error}"))?;
        if document["proposal_format"].as_u64() != Some(PROPOSAL_FORMAT) {
            return Err(format!("proposal {name} has an unsupported format"));
        }
        Ok(Self {
            name: name.into(),
            document,
        })
    }

    /** List every proposal, sorted by name
     * Input
        - None
     * Output
        - Result<Vec<Proposal>, String>
    */
    pub(crate) fn all() -> Result<Vec<Self>, String> {
        let mut names = match fs::read_dir(directory()?) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .and_then(|name| name.strip_suffix(".json"))
                        .map(String::from)
                })
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(io_error(error)),
        };
        names.sort();
        names.iter().map(|name| Self::load(name)).collect()
    }

    /** Return the proposal's status
     * Input
        - None (uses self)
     * Output
        - &str - pending, approved, or rejected
    */
    pub(crate) fn status(&self) -> &str {
        self.document["status"].as_str().unwrap_or("invalid")
    }

    /** Return the recorded policy digest
     * Input
        - None (uses self)
     * Output
        - &str
    */
    pub(crate) fn digest(&self) -> &str {
        self.document["policy_digest"].as_str().unwrap_or_default()
    }

    /** Write the JSON record (and, when given, the candidate policy) through temporary files
     * Input
        - policy: Option<&str> - new candidate policy text
     * Output
        - Result<(), String>
    */
    fn save(&self, policy: Option<&str>) -> Result<(), String> {
        fs::create_dir_all(directory()?).map_err(io_error)?;
        let write = |path: PathBuf, text: &str| -> Result<(), String> {
            let temporary = path.with_extension(format!("tmp{}", std::process::id()));
            fs::write(&temporary, text).map_err(io_error)?;
            fs::rename(&temporary, &path).map_err(io_error)
        };
        if let Some(policy) = policy {
            write(Self::candidate_file(&self.name)?, policy)?;
        }
        let text = serde_json::to_string_pretty(&self.document).map_err(io_error)? + "\n";
        write(Self::record(&self.name)?, &text)
    }

    /** Append a history entry and move the proposal to a new status
     * Input
        - action: &str - generated, regenerated, edited, approved, or rejected
        - actor: &str - who acted
        - detail: Value - action details
     * Output
        - None
    */
    fn log(&mut self, action: &str, actor: &str, detail: Value) {
        let entry = json!({
            "revision": self.document["revision"],
            "action": action,
            "actor": actor,
            "at": now_unix(),
            "policy_digest": self.document["policy_digest"],
            "detail": detail,
        });
        if let Some(history) = self.document["history"].as_array_mut() {
            history.push(entry);
        }
    }

    /** Generate a new proposal from the current repository and store it, without activating it:
     * the candidate policy goes to .crane/proposals/NAME.crane, never to .crane/policies
     * Input
        - name: &str - proposal id and policy name
        - checkpoint: &str - checkpoint for the policy
        - minimum: Confidence - lowest confidence included
     * Output
        - Result<Proposal, String>
        - Error if the name is taken by a proposal or an active policy, or nothing can be proposed
    */
    pub(crate) fn propose(
        name: &str,
        checkpoint: &str,
        minimum: Confidence,
    ) -> Result<Self, String> {
        Self::ensure_free(name)?;
        let content = generate(name, checkpoint, minimum)?;
        Self::create(
            name,
            checkpoint,
            json!({"kind": "discovery", "min_confidence": minimum.name()}),
            content,
            "crane policy propose",
        )
    }

    /** Refuse a proposal name already used by a proposal or an active policy
     * Input
        - name: &str - proposal id and policy name
     * Output
        - Result<(), String>
        - Error if the name is invalid or taken
    */
    pub(crate) fn ensure_free(name: &str) -> Result<(), String> {
        validate_identifier(name)?;
        if let Ok(existing) = Self::load(name) {
            return Err(format!(
                "proposal {name} already exists ({}); use 'crane policy regenerate {name}' or choose another name",
                existing.status()
            ));
        }
        if root()?
            .join("policies")
            .join(format!("{name}.crane"))
            .exists()
        {
            return Err(format!(
                "an active policy named {name} already exists; choose another name"
            ));
        }
        Ok(())
    }

    /** Store a new pending proposal from generated content, recording who generated it (and
     * whether that happened in an agent environment); nothing is activated
     * Input
        - name: &str - proposal id and policy name (checked by ensure_free)
        - checkpoint: &str - checkpoint the policy binds to
        - origin: Value - where the content came from: {"kind": "discovery", "min_confidence"}
          or {"kind": "task", "task_id"}
        - content: (Value, String, Value, Value) - candidates, policy text, zone suggestions, and
          source state
        - generator: &str - command that generated it
     * Output
        - Result<Proposal, String>
        - Error if the files cannot be written
    */
    pub(crate) fn create(
        name: &str,
        checkpoint: &str,
        origin: Value,
        content: (Value, String, Value, Value),
        generator: &str,
    ) -> Result<Self, String> {
        let (listed, policy, regions, source) = content;
        let mut proposal = Self {
            name: name.into(),
            document: json!({
                "proposal_format": PROPOSAL_FORMAT,
                "proposal_id": name,
                "policy_name": name,
                "checkpoint": checkpoint,
                "origin": origin,
                "min_confidence": origin["min_confidence"],
                "status": "pending",
                "active": false,
                "revision": 1,
                "policy_file": format!("proposals/{name}.crane"),
                "policy": policy,
                "policy_digest": sha256(policy.as_bytes()),
                "candidates": listed,
                "zone_suggestions": regions,
                "source": source,
                "history": [],
                "activation": null,
            }),
        };
        let generator = match agent_environment() {
            Some(marker) => format!("{generator} (agent environment: {marker})"),
            None => generator.into(),
        };
        proposal.log("generated", &generator, json!({}));
        proposal.save(Some(&policy))?;
        Ok(proposal)
    }

    /** Regenerate a pending or rejected proposal from the current repository as a new pending
     * revision (manual edits are replaced; the history keeps the previous digest)
     * Input
        - by: Option<String> - who regenerates
     * Output
        - Result<(), String>
        - Error in an agent environment, for an approved proposal, or if nothing can be proposed
    */
    pub(crate) fn regenerate(&mut self, by: Option<String>) -> Result<(), String> {
        require_human("regenerate")?;
        if self.status() == "approved" {
            return Err(format!(
                "proposal {} is already active; propose a new one instead",
                self.name
            ));
        }
        let checkpoint = self.document["checkpoint"]
            .as_str()
            .unwrap_or("baseline")
            .to_string();
        let (listed, policy, regions, source) = match self.document["origin"]["task_id"].as_str() {
            // A task contract is regenerated by planning its task again
            Some(task) => crate::tasks::proposal_content(task, &self.name, &checkpoint)?,
            None => {
                let minimum = Confidence::parse(
                    self.document["min_confidence"].as_str().unwrap_or("medium"),
                )?;
                generate(&self.name, &checkpoint, minimum)?
            }
        };
        let previous = self.digest().to_string();
        let revision = self.document["revision"].as_u64().unwrap_or(1) + 1;
        self.document["revision"] = json!(revision);
        self.document["status"] = json!("pending");
        self.document["policy"] = json!(policy);
        self.document["policy_digest"] = json!(sha256(policy.as_bytes()));
        self.document["candidates"] = listed;
        self.document["zone_suggestions"] = regions;
        self.document["source"] = source;
        self.document["rejection"] = Value::Null;
        let changed = previous != self.digest();
        self.log(
            "regenerated",
            &actor(by),
            json!({"previous_digest": previous, "policy_changed": changed}),
        );
        self.save(Some(&policy))
    }

    /** Record a human edit of the candidate policy: read the edited text (from a file, or the
     * proposal's own .crane file), require it to parse as a policy with the proposal's name and a
     * valid checkpoint, and store it as a new pending revision
     * Input
        - file: Option<String> - edited policy file, default .crane/proposals/NAME.crane
        - by: Option<String> - who edited
     * Output
        - Result<(), String>
        - Error in an agent environment, for a proposal that is not pending, or for an invalid
          policy
    */
    pub(crate) fn edit(&mut self, file: Option<String>, by: Option<String>) -> Result<(), String> {
        require_human("edit")?;
        if self.status() != "pending" {
            return Err(format!(
                "proposal {} is {}; only pending proposals can be edited",
                self.name,
                self.status()
            ));
        }
        let path = match file {
            Some(path) => PathBuf::from(path),
            None => Self::candidate_file(&self.name)?,
        };
        let text = fs::read_to_string(&path).map_err(io_error)?;
        let parsed = parse(&text).map_err(|error| format!("edited policy is invalid: {error}"))?;
        if parsed.name != self.name {
            return Err(format!(
                "edited policy is named {}, not {}",
                parsed.name, self.name
            ));
        }
        let stored = load_checkpoint(&parsed.checkpoint)?;
        checkpoint_commit(&parsed.checkpoint, &stored.name, &stored.commit)?;
        let previous = self.digest().to_string();
        let digest = sha256(text.as_bytes());
        if digest == previous {
            return Err(format!(
                "the policy of proposal {} did not change",
                self.name
            ));
        }
        let revision = self.document["revision"].as_u64().unwrap_or(1) + 1;
        self.document["revision"] = json!(revision);
        self.document["checkpoint"] = json!(parsed.checkpoint);
        self.document["policy"] = json!(text);
        self.document["policy_digest"] = json!(digest);
        self.log(
            "edited",
            &actor(by),
            json!({"previous_digest": previous, "rules": parsed.rules.len()}),
        );
        self.save(Some(&text))
    }

    /** Reject a pending proposal
     * Input
        - approver: Option<String> - who rejects (required)
        - reason: Option<String> - why
     * Output
        - Result<(), String>
        - Error in an agent environment, without an approver, or for a proposal not pending
    */
    pub(crate) fn reject(
        &mut self,
        approver: Option<String>,
        reason: Option<String>,
    ) -> Result<(), String> {
        require_human("reject")?;
        let approver = approver
            .filter(|value| !value.trim().is_empty())
            .ok_or("crane policy reject requires --approver NAME")?;
        if self.status() != "pending" {
            return Err(format!(
                "proposal {} is {}; only pending proposals can be rejected",
                self.name,
                self.status()
            ));
        }
        self.document["status"] = json!("rejected");
        self.document["rejection"] = json!({"by": approver, "reason": reason});
        self.log("rejected", &approver, json!({"reason": reason}));
        self.save(None)
    }

    /** Mark a pending proposal superseded by a newer version of the same task contract, so it can
     * no longer be approved; done by the orchestrator, never by an agent
     * Input
        - by: &str - newer proposal or cause
     * Output
        - Result<bool, String> whether the proposal was pending and is now superseded
        - Error in an agent environment or if the record cannot be written
    */
    pub(crate) fn supersede(&mut self, by: &str) -> Result<bool, String> {
        require_human("supersede")?;
        if self.status() != "pending" {
            return Ok(false);
        }
        self.document["status"] = json!("superseded");
        self.log("superseded", "crane task orchestration", json!({"by": by}));
        self.save(None)?;
        Ok(true)
    }

    /** Retire an approved task contract when its task completes, is cancelled, or is replaced by
     * an approved newer version: the policy moves from .crane/policies to .crane/retired, so it is
     * no longer enforced but stays as evidence; done by the orchestrator, never by an agent
     * Input
        - reason: &str - why
     * Output
        - Result<bool, String> whether an active policy was retired
        - Error in an agent environment or if the files cannot be moved
    */
    pub(crate) fn retire(&mut self, reason: &str) -> Result<bool, String> {
        require_human("retire")?;
        if self.status() != "approved" {
            return Ok(false);
        }
        let crane = root()?;
        let active = crane.join("policies").join(format!("{}.crane", self.name));
        let retired = crane.join("retired");
        fs::create_dir_all(&retired).map_err(io_error)?;
        match fs::rename(&active, retired.join(format!("{}.crane", self.name))) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
        self.document["status"] = json!("retired");
        self.document["active"] = json!(false);
        self.log(
            "retired",
            "crane task orchestration",
            json!({"reason": reason}),
        );
        self.save(None)?;
        Ok(true)
    }

    /** Approve and activate a pending proposal: refuse in an agent environment, require a named
     * approver and a confirmation quoting the start of the reviewed policy digest, require the
     * candidate file to be exactly the recorded policy (an unrecorded edit cannot be activated)
     * and to parse, and only then write it to .crane/policies/NAME.crane, which is what makes it
     * an enforced contract (sessions already running keep their bound contract and report drift)
     * Input
        - approver: Option<String> - who approves (required)
        - confirm: Option<String> - at least the first 12 characters of the policy digest
     * Output
        - Result<PathBuf, String> the activated policy file
        - Error for any failed check; nothing is activated then
    */
    pub(crate) fn approve(
        &mut self,
        approver: Option<String>,
        confirm: Option<String>,
    ) -> Result<PathBuf, String> {
        require_human("approve")?;
        let approver = approver
            .filter(|value| !value.trim().is_empty())
            .ok_or("crane policy approve requires --approver NAME")?;
        if self.status() != "pending" {
            return Err(format!(
                "proposal {} is {}; only pending proposals can be approved",
                self.name,
                self.status()
            ));
        }
        let digest = self.digest().trim_start_matches("sha256:").to_string();
        let quoted = confirm.unwrap_or_default();
        let quoted = quoted.trim_start_matches("sha256:");
        if quoted.len() < CONFIRM_LENGTH || !digest.starts_with(quoted) {
            return Err(format!(
                "crane policy approve requires --confirm with at least the first {CONFIRM_LENGTH} characters of the reviewed policy digest (see 'crane policy show {}')",
                self.name
            ));
        }
        let text = fs::read_to_string(Self::candidate_file(&self.name)?).map_err(io_error)?;
        if sha256(text.as_bytes()) != self.digest() {
            return Err(format!(
                "the candidate policy file changed after the proposal was recorded; review it and run 'crane policy edit {}' first",
                self.name
            ));
        }
        let parsed =
            parse(&text).map_err(|error| format!("candidate policy is invalid: {error}"))?;
        if parsed.name != self.name {
            return Err(format!(
                "candidate policy is named {}, not {}",
                parsed.name, self.name
            ));
        }
        let target = root()?
            .join("policies")
            .join(format!("{}.crane", self.name));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| match error.kind() {
                ErrorKind::AlreadyExists => {
                    format!("an active policy named {} already exists", self.name)
                }
                _ => io_error(error),
            })?;
        file.write_all(text.as_bytes()).map_err(io_error)?;
        self.document["status"] = json!("approved");
        self.document["active"] = json!(true);
        self.document["activation"] = json!({
            "policy_path": format!("policies/{}.crane", self.name),
            "approver": approver,
            "git_user": git(&["config", "user.email"]).ok(),
            "at": now_unix(),
            "policy_digest": self.digest(),
        });
        self.log("approved", &approver, json!({}));
        self.save(None)?;
        Ok(target)
    }
}
