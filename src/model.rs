/** A parsed .crane policy, produced by policy::parse from a "policy NAME { ... }" block and
 * evaluated rule by rule during crane check
 * Fields
    - name: String - policy identifier, used as policy_id in reports
    - checkpoint: String - name of the checkpoint the rules compare against
    - rules: Vec<Rule> - rules in source order, at least one
*/
#[derive(Debug, Clone)]
pub(crate) struct Policy {
    pub(crate) name: String,
    pub(crate) checkpoint: String,
    pub(crate) rules: Vec<Rule>,
}

/** A single policy rule; the language currently supports only preserve --function
 * Variants
    - PreserveFunction { target } - the target function must match its checkpoint version token
      for token (ignoring formatting and comments)
*/
#[derive(Debug, Clone)]
pub(crate) enum Rule {
    PreserveFunction { target: String },
}

/** A trusted baseline stored in .crane/checkpoints/NAME.json, written by crane checkpoint and read
 * back by repository::load_checkpoint
 * Fields
    - name: String - checkpoint identifier
    - commit: String - Git commit SHA used as the baseline
    - branch: String - branch at creation time (DETACHED if none), informational only
    - created_at_unix: u64 - creation time in seconds; not parsed back when loading, so it is 0 then
*/
#[derive(Debug, Clone)]
pub(crate) struct Checkpoint {
    pub(crate) name: String,
    pub(crate) commit: String,
    pub(crate) branch: String,
    pub(crate) created_at_unix: u64,
}

/** One failed rule or setup problem, serialized into the stable JSON contract by check::render_json
 * (repair_owner is derived from these fields at render time, not stored)
 * Fields
    - policy_id: String - failing policy, or "crane" for setup-level failures
    - rule: String - rule kind, such as preserve, policy, or verification
    - target: String - protected function, empty for policy and setup failures
    - checkpoint: String - checkpoint name, empty when not applicable
    - violation_type: String - machine-readable category, such as source_changed or malformed_policy
    - message: String - human-readable explanation
*/
#[derive(Debug, Clone)]
pub(crate) struct Violation {
    pub(crate) policy_id: String,
    pub(crate) rule: String,
    pub(crate) target: String,
    pub(crate) checkpoint: String,
    pub(crate) violation_type: String,
    pub(crate) message: String,
}

/** A resolved function definition, reduced to its canonical form for comparison
 * Fields
    - snippet: String - the definition's tokens in source order, comments dropped, each token
      followed by a \0 separator; two snippets are equal only if the code tokens are identical
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceTarget {
    pub(crate) snippet: String,
}
