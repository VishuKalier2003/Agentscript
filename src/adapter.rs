use crate::commands::{initialize_for_agent, verify_for_agent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentKind {
    Generic,
    Claude,
    Codex,
}

impl AgentKind {
    // Normalize profile aliases before selecting an adapter implementation
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("generic").to_ascii_lowercase().as_str() {
            "generic" | "universal" => Ok(Self::Generic),
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            value => Err(format!(
                "unsupported agent profile '{value}'; expected generic, claude, or codex"
            )),
        }
    }

    // Expose the stable profile name used in activation messages
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

pub(crate) trait AgentAdapter {
    fn kind(&self) -> AgentKind;

    // Initialize an agent integration and immediately verify its repository state
    fn initialize(&self) -> Result<(), String> {
        initialize_for_agent()?;
        self.verify()
    }

    // Run the same independent verifier for every supported agent profile
    fn verify(&self) -> Result<(), String> {
        verify_for_agent()
    }

    // Describe the output and exit-code contract expected by external agents
    fn feedback_contract(&self) -> &'static str {
        "stdout contains Crane's stable JSON verification result; non-zero exit means repair is required"
    }
}

pub(crate) struct UniversalAgentAdapter {
    kind: AgentKind,
}

impl UniversalAgentAdapter {
    // Retain the selected profile while sharing generic lifecycle behavior
    pub(crate) fn new(kind: AgentKind) -> Self {
        Self { kind }
    }
}

impl AgentAdapter for UniversalAgentAdapter {
    fn kind(&self) -> AgentKind {
        self.kind
    }
}

pub(crate) struct ClaudeAdapter(UniversalAgentAdapter);

pub(crate) struct CodexAdapter(UniversalAgentAdapter);

impl AgentAdapter for ClaudeAdapter {
    // Claude uses the universal verifier with Claude-specific hook wiring
    fn kind(&self) -> AgentKind {
        self.0.kind()
    }
}

impl AgentAdapter for CodexAdapter {
    // Codex currently uses the universal verifier without local hook installation
    fn kind(&self) -> AgentKind {
        self.0.kind()
    }
}

// Construct the narrow adapter surface without duplicating verification semantics
pub(crate) fn adapter(kind: AgentKind) -> Box<dyn AgentAdapter> {
    match kind {
        AgentKind::Claude => Box::new(ClaudeAdapter(UniversalAgentAdapter::new(kind))),
        AgentKind::Codex => Box::new(CodexAdapter(UniversalAgentAdapter::new(kind))),
        AgentKind::Generic => Box::new(UniversalAgentAdapter::new(kind)),
    }
}
