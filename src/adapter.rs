use crate::commands::{initialize_for_agent, verify_for_agent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentKind {
    Generic,
    Claude,
    Codex,
}

impl AgentKind {
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

    fn initialize(&self) -> Result<(), String> {
        initialize_for_agent()?;
        self.verify()
    }

    fn verify(&self) -> Result<(), String> {
        verify_for_agent()
    }

    fn feedback_contract(&self) -> &'static str {
        "stdout contains Crane's stable JSON verification result; non-zero exit means repair is required"
    }
}

pub(crate) struct UniversalAgentAdapter {
    kind: AgentKind,
}

impl UniversalAgentAdapter {
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
    fn kind(&self) -> AgentKind {
        self.0.kind()
    }
}

impl AgentAdapter for CodexAdapter {
    fn kind(&self) -> AgentKind {
        self.0.kind()
    }
}

pub(crate) fn adapter(kind: AgentKind) -> Box<dyn AgentAdapter> {
    match kind {
        AgentKind::Claude => Box::new(ClaudeAdapter(UniversalAgentAdapter::new(kind))),
        AgentKind::Codex => Box::new(CodexAdapter(UniversalAgentAdapter::new(kind))),
        AgentKind::Generic => Box::new(UniversalAgentAdapter::new(kind)),
    }
}
