// .crane/checkpoints.json: the insert-only array of trusted baselines. Each entry records a Git
// commit and chains to the entry before it (previous digest), so an edited, reordered, or removed
// checkpoint breaks the chain; the chain head is also covered by the signed registry manifest.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::trust::crypto::digest_json;

/** Version of the checkpoint file layout */
pub(crate) const CHECKPOINTS_FORMAT: u64 = 1;

/** Chain value before the first checkpoint */
pub(crate) const GENESIS: &str = "genesis";

/** Digest domain of a checkpoint entry */
const CHECKPOINT_DOMAIN: &str = "checkpoint/entry/v1";

/** One trusted baseline
 * Fields
    - name: String - checkpoint name, unique
    - commit: String - full commit SHA
    - tree: String - tree SHA of the commit
    - branch: String - branch at creation (informational)
    - created_at: u64 - Unix seconds
    - created_by: String - actor
    - previous: String - digest of the previous entry, GENESIS for the first
    - digest: String - SHA-512 of this entry without the digest field
*/
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Checkpoint {
    pub(crate) name: String,
    pub(crate) commit: String,
    pub(crate) tree: String,
    pub(crate) branch: String,
    pub(crate) created_at: u64,
    pub(crate) created_by: String,
    pub(crate) previous: String,
    pub(crate) digest: String,
}

/** The checkpoint file
 * Fields
    - format: u64 - layout version
    - checkpoints: Vec<Checkpoint> - entries in insertion order
*/
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CheckpointLog {
    pub(crate) format: u64,
    pub(crate) checkpoints: Vec<Checkpoint>,
}

impl Default for CheckpointLog {
    /** Build an empty log
     * Input
        - None
     * Output
        - CheckpointLog
    */
    fn default() -> Self {
        Self {
            format: CHECKPOINTS_FORMAT,
            checkpoints: Vec::new(),
        }
    }
}

/** Compute an entry's digest from its fields other than the digest itself
 * Input
    - checkpoint: &Checkpoint - entry
 * Output
    - String of 128 hex characters
*/
fn entry_digest(checkpoint: &Checkpoint) -> String {
    digest_json(
        CHECKPOINT_DOMAIN,
        &json!({
            "name": checkpoint.name,
            "commit": checkpoint.commit,
            "tree": checkpoint.tree,
            "branch": checkpoint.branch,
            "created_at": checkpoint.created_at,
            "created_by": checkpoint.created_by,
            "previous": checkpoint.previous,
        }),
    )
}

impl CheckpointLog {
    /** Find a checkpoint by name
     * Input
        - name: &str - checkpoint name
     * Output
        - Option<&Checkpoint>
    */
    pub(crate) fn find(&self, name: &str) -> Option<&Checkpoint> {
        self.checkpoints
            .iter()
            .find(|checkpoint| checkpoint.name == name)
    }

    /** Return the chain head: the last entry's digest, GENESIS when empty
     * Input
        - None (uses self)
     * Output
        - String
    */
    pub(crate) fn head(&self) -> String {
        self.checkpoints.last().map_or_else(
            || GENESIS.to_string(),
            |checkpoint| checkpoint.digest.clone(),
        )
    }

    /** Append a checkpoint, refusing a duplicate name (checkpoints are never updated)
     * Input
        - name: &str - checkpoint name
        - commit: &str - commit SHA
        - tree: &str - tree SHA
        - branch: &str - branch name
        - created_at: u64 - Unix seconds
        - created_by: &str - actor
     * Output
        - Result<Checkpoint, String> the new entry
        - Error if the name is taken
    */
    pub(crate) fn append(
        &mut self,
        name: &str,
        commit: &str,
        tree: &str,
        branch: &str,
        created_at: u64,
        created_by: &str,
    ) -> Result<Checkpoint, String> {
        if self.find(name).is_some() {
            return Err(format!(
                "checkpoint '{name}' already exists; checkpoints cannot be updated, only inserted under a new name"
            ));
        }
        let mut checkpoint = Checkpoint {
            name: name.into(),
            commit: commit.into(),
            tree: tree.into(),
            branch: branch.into(),
            created_at,
            created_by: created_by.into(),
            previous: self.head(),
            digest: String::new(),
        };
        checkpoint.digest = entry_digest(&checkpoint);
        self.checkpoints.push(checkpoint.clone());
        Ok(checkpoint)
    }

    /** Verify the chain: every entry's digest is correct, chains from the one before, and names
     * are unique
     * Input
        - None (uses self)
     * Output
        - Result<(), String>
        - Error naming the first broken entry
    */
    pub(crate) fn verify_chain(&self) -> Result<(), String> {
        let mut previous = GENESIS.to_string();
        let mut names = std::collections::BTreeSet::new();
        for checkpoint in &self.checkpoints {
            if !names.insert(checkpoint.name.as_str()) {
                return Err(format!("checkpoint '{}' appears twice", checkpoint.name));
            }
            if checkpoint.previous != previous || entry_digest(checkpoint) != checkpoint.digest {
                return Err(format!(
                    "checkpoint '{}' was edited, reordered, or inserted outside Crane",
                    checkpoint.name
                ));
            }
            previous = checkpoint.digest.clone();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /** Check append-only behavior and that edits break the chain
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn checkpoints_are_insert_only() {
        let mut log = CheckpointLog::default();
        log.append("baseline", "a", "t", "main", 1, "me").unwrap();
        log.append("next", "b", "t", "main", 2, "me").unwrap();
        assert!(log.append("baseline", "c", "t", "main", 3, "me").is_err());
        assert!(log.verify_chain().is_ok());
        let mut edited = log.clone();
        edited.checkpoints[0].commit = "z".into();
        assert!(edited.verify_chain().is_err());
        let mut removed = log.clone();
        removed.checkpoints.remove(0);
        assert!(removed.verify_chain().is_err());
    }
}
