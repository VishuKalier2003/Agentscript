use crate::commands::{initialize_for_agent, verify_for_agent};

/** The agent profile selected with --profile or --agent
 * Variants
    - Generic - any agent host, verification through the CLI only
    - Claude - Claude Code, which also supports hook installation
    - Codex - Codex, verification through the CLI only
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentKind {
    Generic,
    Claude,
    Codex,
}

impl AgentKind {
    /** Parse an agent profile name, by defaulting to generic when none is given, lowercasing the
     * value, and mapping known aliases (universal, claude-code) to their AgentKind
     * Input
        - value: Option<&str> - profile name from --profile or --agent
     * Output
        - Result<AgentKind, String>
        - Error if the profile is not generic, claude, or codex
    */
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

    /** Return the stable profile name used in activation messages, by matching the variant to its
     * lowercase string
     * Input
        - None (uses self)
     * Output
        - &'static str profile name
    */
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/** Common interface for agent integrations; initialize, verify, and feedback_contract have shared
 * default implementations, so each adapter only has to report its kind
*/
pub(crate) trait AgentAdapter {
    /** Report which agent profile this adapter serves, implemented by each adapter
     * Input
        - None (uses self)
     * Output
        - AgentKind of the adapter
    */
    fn kind(&self) -> AgentKind;

    /** Initialize an agent integration, by first creating the .crane repository structure and then
     * immediately running verification so the agent sees the current state
     * Input
        - None (uses self)
     * Output
        - Result<(), String>
        - Error if initialization fails or verification finds violations
    */
    fn initialize(&self) -> Result<(), String> {
        initialize_for_agent()?;
        self.verify()
    }

    /** Run verification for the agent, by delegating to the shared verifier so every profile gets
     * identical policy semantics and JSON output
     * Input
        - None (uses self)
     * Output
        - Result<(), String>
        - Error if any policy fails or verification cannot complete
    */
    fn verify(&self) -> Result<(), String> {
        verify_for_agent()
    }

    /** Describe the output and exit-code contract expected by external agents, by returning a fixed
     * explanatory string
     * Input
        - None (uses self)
     * Output
        - &'static str contract description
    */
    fn feedback_contract(&self) -> &'static str {
        "stdout contains Crane's stable JSON verification result; non-zero exit means repair is required"
    }
}

/** Adapter that uses the shared behavior unchanged, used directly for the generic profile and
 * wrapped by the Claude and Codex adapters
 * Fields
    - kind: AgentKind - profile this adapter reports
*/
pub(crate) struct UniversalAgentAdapter {
    kind: AgentKind,
}

impl UniversalAgentAdapter {
    /** Create a universal adapter, by storing the selected profile so shared lifecycle behavior can
     * still report which agent it serves
     * Input
        - kind: AgentKind - selected agent profile
     * Output
        - UniversalAgentAdapter
    */
    pub(crate) fn new(kind: AgentKind) -> Self {
        Self { kind }
    }
}

impl AgentAdapter for UniversalAgentAdapter {
    /** Report the profile this universal adapter was created with, by returning the stored kind
     * Input
        - None (uses self)
     * Output
        - AgentKind stored at construction
    */
    fn kind(&self) -> AgentKind {
        self.kind
    }
}

/** Adapter for Claude Code, wrapping the universal adapter so it has its own type
 * Fields
    - 0: UniversalAgentAdapter - wrapped adapter providing the shared behavior
*/
pub(crate) struct ClaudeAdapter(UniversalAgentAdapter);

/** Adapter for Codex, wrapping the universal adapter so it has its own type
 * Fields
    - 0: UniversalAgentAdapter - wrapped adapter providing the shared behavior
*/
pub(crate) struct CodexAdapter(UniversalAgentAdapter);

impl AgentAdapter for ClaudeAdapter {
    /** Report the Claude profile, by delegating to the wrapped universal adapter; Claude uses the
     * universal verifier with Claude-specific hook wiring
     * Input
        - None (uses self)
     * Output
        - AgentKind of the wrapped adapter
    */
    fn kind(&self) -> AgentKind {
        self.0.kind()
    }
}

impl AgentAdapter for CodexAdapter {
    /** Report the Codex profile, by delegating to the wrapped universal adapter; Codex uses the
     * universal verifier without local hook installation
     * Input
        - None (uses self)
     * Output
        - AgentKind of the wrapped adapter
    */
    fn kind(&self) -> AgentKind {
        self.0.kind()
    }
}

/** Construct the adapter for a profile, by matching the kind and boxing the Claude, Codex, or
 * universal adapter behind the AgentAdapter trait
 * Input
    - kind: AgentKind - selected agent profile
 * Output
    - Box<dyn AgentAdapter> for that profile
*/
pub(crate) fn adapter(kind: AgentKind) -> Box<dyn AgentAdapter> {
    match kind {
        AgentKind::Claude => Box::new(ClaudeAdapter(UniversalAgentAdapter::new(kind))),
        AgentKind::Codex => Box::new(CodexAdapter(UniversalAgentAdapter::new(kind))),
        AgentKind::Generic => Box::new(UniversalAgentAdapter::new(kind)),
    }
}
