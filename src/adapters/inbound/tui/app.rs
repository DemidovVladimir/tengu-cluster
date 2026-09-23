//! TUI application state model — pure data, no widget state.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BubbleRole {
    User,
    Assistant,
    System,
}

/// Request sent from the cursive UI thread to the engine thread.
pub enum ChatRequest {
    /// A normal user message to process as a chat turn.
    UserMessage { user_text: String },
    /// A skill management slash command. The engine thread sends a response string back.
    SkillCommand {
        command: SkillCommand,
        response_tx: std::sync::mpsc::Sender<String>,
    },
    /// A generic slash command (e.g. /cost, /context, /reset) routed to engine thread
    /// so it can access the real runtime state.
    SlashCommand {
        text: String,
        response_tx: std::sync::mpsc::Sender<String>,
    },
}

/// Skill management commands triggered by `/skills`, `/enable`, `/disable`.
#[derive(Debug)]
pub enum SkillCommand {
    List,
    Enable(String),
    Disable(String),
}

/// Theme mode for the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}
