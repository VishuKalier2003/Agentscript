#[derive(Debug, Clone)]
pub(crate) struct Policy {
    pub(crate) name: String,
    pub(crate) checkpoint: String,
    pub(crate) rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
pub(crate) enum Rule {
    PreserveFunction { target: String },
}

#[derive(Debug, Clone)]
pub(crate) struct Checkpoint {
    pub(crate) name: String,
    pub(crate) commit: String,
    pub(crate) branch: String,
    pub(crate) created_at_unix: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct Violation {
    pub(crate) policy_id: String,
    pub(crate) rule: String,
    pub(crate) target: String,
    pub(crate) checkpoint: String,
    pub(crate) violation_type: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceTarget {
    pub(crate) snippet: String,
}
