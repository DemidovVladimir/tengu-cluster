//! Cursive view builders and UI update helpers.

use super::app::{BubbleRole, ChatRequest, SkillCommand};
use cursive::event::Event;
use cursive::theme::{BaseColor, Color, Effect, Style};
use cursive::traits::*;
use cursive::utils::markup::StyledString;
use cursive::view::ScrollStrategy;
use cursive::views::{Dialog, EditView, LinearLayout, ScrollView, TextView};
use cursive::Cursive;
use std::sync::mpsc;
use std::time::Duration;

/// Build the full UI view tree and register callbacks.
pub fn build_ui(siv: &mut Cursive, request_tx: mpsc::Sender<ChatRequest>) {
    // Global quit shortcut
    siv.add_global_callback(Event::CtrlChar('q'), |s| s.quit());
    siv.add_global_callback(Event::CtrlChar('c'), |s| s.quit());

    let tx = request_tx;
    let layout = LinearLayout::vertical()
        .child(build_header())
        .child(build_chat())
        .child(build_input(tx))
        .child(build_status());

    siv.add_fullscreen_layer(layout);
}

fn build_header() -> impl cursive::View {
    TextView::new("").with_name("header").full_width()
}

fn build_chat() -> impl cursive::View {
    ScrollView::new(LinearLayout::vertical().with_name("chat_content"))
        .scroll_strategy(ScrollStrategy::StickToBottom)
        .with_name("chat_scroll")
        .full_screen()
}

fn build_input(tx: mpsc::Sender<ChatRequest>) -> impl cursive::View {
    LinearLayout::horizontal().child(TextView::new("> ")).child(
        EditView::new()
            .on_submit(move |siv, text| {
                on_submit(siv, text, &tx);
            })
            .with_name("input")
            .full_width(),
    )
}

fn build_status() -> impl cursive::View {
    TextView::new(" 0 in / 0 out  |  /help  |  Ctrl+Q quit")
        .with_name("status")
        .full_width()
}

/// Try to parse a skill management slash command.
/// Returns `Some(SkillCommand)` for `/skills`, `/enable <name>`, `/disable <name>`.
fn parse_skill_slash_command(text: &str) -> Option<SkillCommand> {
    let trimmed = text.trim();
    if trimmed == "/skills" {
        return Some(SkillCommand::List);
    }
    if let Some(name) = trimmed.strip_prefix("/enable ") {
        let name = name.trim();
        if !name.is_empty() {
            return Some(SkillCommand::Enable(name.to_string()));
        }
    }
    if let Some(name) = trimmed.strip_prefix("/disable ") {
        let name = name.trim();
        if !name.is_empty() {
            return Some(SkillCommand::Disable(name.to_string()));
        }
    }
    None
}

/// Handle user message submission.
fn on_submit(siv: &mut Cursive, text: &str, tx: &mpsc::Sender<ChatRequest>) {
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }

    // Clear input
    siv.call_on_name("input", |view: &mut EditView| {
        view.set_content("");
    });

    // Skill management commands — routed to the engine thread for synchronous response.
    if let Some(cmd) = parse_skill_slash_command(&text) {
        let (resp_tx, resp_rx) = mpsc::channel();
        let _ = tx.send(ChatRequest::SkillCommand {
            command: cmd,
            response_tx: resp_tx,
        });
        if let Ok(response) = resp_rx.recv_timeout(Duration::from_secs(2)) {
            push_bubble(siv, BubbleRole::System, &response);
        } else {
            push_bubble(siv, BubbleRole::System, "Skill command timed out.");
        }
        return;
    }

    // Handle other slash commands via the engine thread (has access to real runtime state).
    if text.starts_with('/') {
        let (resp_tx, resp_rx) = mpsc::channel();
        let _ = tx.send(ChatRequest::SlashCommand {
            text: text.clone(),
            response_tx: resp_tx,
        });
        if let Ok(response) = resp_rx.recv_timeout(Duration::from_secs(2)) {
            push_bubble(siv, BubbleRole::System, &response);
        } else {
            push_bubble(siv, BubbleRole::System, "Slash command timed out.");
        }
        return;
    }

    // Show user message
    push_bubble(siv, BubbleRole::User, &text);

    // Show thinking indicator
    show_thinking(siv);

    // Send to engine thread
    let _ = tx.send(ChatRequest::UserMessage { user_text: text });
}

/// Update the header bar with agent/engine info.
pub fn update_header(siv: &mut Cursive, agent_name: &str, engine_label: &str, lens: &str) {
    let text = format!("  TENGU  {}  {}  {}", agent_name, engine_label, lens);
    siv.call_on_name("header", |view: &mut TextView| {
        let mut styled = StyledString::new();
        styled.append_styled(
            "  TENGU",
            Style::from(Color::Light(BaseColor::Cyan)).combine(Effect::Bold),
        );
        styled.append_plain(format!("  {}  {}  {}", agent_name, engine_label, lens));
        view.set_content(styled);
    });
    let _ = text;
}

/// Append a chat bubble to the chat area.
pub fn push_bubble(siv: &mut Cursive, role: BubbleRole, text: &str) {
    siv.call_on_name("chat_content", |layout: &mut LinearLayout| {
        let styled = format_bubble(role, text);
        layout.add_child(TextView::new(styled));
    });
}

/// Show the thinking indicator at the bottom of chat.
pub fn show_thinking(siv: &mut Cursive) {
    siv.call_on_name("chat_content", |layout: &mut LinearLayout| {
        let mut styled = StyledString::new();
        styled.append_styled(
            "  ⠋ thinking...",
            Style::from(Color::Light(BaseColor::Black)),
        );
        layout.add_child(TextView::new(styled).with_name("thinking_indicator"));
    });
    siv.set_autorefresh(true);
}

/// Remove the thinking indicator.
pub fn hide_thinking(siv: &mut Cursive) {
    siv.call_on_name("chat_content", |layout: &mut LinearLayout| {
        // Find and remove the thinking indicator (last child)
        let count = layout.len();
        if count > 0 {
            // Check if the last child is the thinking indicator by trying to find it
            if layout.find_child_from_name("thinking_indicator").is_some() {
                layout.remove_child(count - 1);
            }
        }
    });
    siv.set_autorefresh(false);
}

/// Update the status bar with token counts.
pub fn update_status(siv: &mut Cursive, input_tokens: u32, output_tokens: u32) {
    let text = format!(
        " {} in / {} out  |  /help  |  Ctrl+Q quit",
        input_tokens, output_tokens
    );
    siv.call_on_name("status", |view: &mut TextView| {
        view.set_content(text);
    });
}

/// Format a chat bubble with role-based styling.
fn format_bubble(role: BubbleRole, text: &str) -> StyledString {
    let mut styled = StyledString::new();
    styled.append_plain("\n");

    match role {
        BubbleRole::User => {
            styled.append_styled(
                "  You: ",
                Style::from(Color::Light(BaseColor::Cyan)).combine(Effect::Bold),
            );
            styled.append_plain(format!("\n  {}\n", text));
        }
        BubbleRole::Assistant => {
            styled.append_styled(
                "  Tengu: ",
                Style::from(Color::Light(BaseColor::Green)).combine(Effect::Bold),
            );
            styled.append_plain(format!("\n  {}\n", text));
        }
        BubbleRole::System => {
            styled.append_styled(
                format!("  {}\n", text),
                Style::from(Color::Light(BaseColor::Black)).combine(Effect::Italic),
            );
        }
    }

    styled
}

/// Push the welcome message into the chat area.
pub fn push_welcome(siv: &mut Cursive) {
    siv.call_on_name("chat_content", |layout: &mut LinearLayout| {
        let mut styled = StyledString::new();
        styled.append_styled(
            "\n  Start typing to begin a conversation.\n",
            Style::from(Color::Light(BaseColor::Black)),
        );
        layout.add_child(TextView::new(styled));
    });
}

/// Show a tool activity line in the chat (e.g. "read_file: src/main.rs").
pub fn push_tool_activity(siv: &mut Cursive, tool_name: &str, detail: &str) {
    let text = format!("  [tool] {} {}", tool_name, detail);
    siv.call_on_name("chat_content", |layout: &mut LinearLayout| {
        let mut styled = StyledString::new();
        styled.append_styled(
            &text,
            Style::from(Color::Light(BaseColor::Blue)).combine(Effect::Italic),
        );
        styled.append_plain("\n");
        layout.add_child(TextView::new(styled));
    });
}

/// Show a write confirmation dialog. Sends `true` (allow) or `false` (deny)
/// back through the provided sender.
pub fn show_write_confirmation(
    siv: &mut Cursive,
    path: &str,
    content_preview: &str,
    response_tx: mpsc::Sender<bool>,
) {
    let preview = if content_preview.len() > 200 {
        let mut end = 200;
        while end > 0 && !content_preview.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &content_preview[..end])
    } else {
        content_preview.to_string()
    };

    let tx_allow = response_tx.clone();
    let tx_deny = response_tx;

    let dialog = Dialog::text(format!(
        "Allow write to '{}'?\n\nPreview:\n{}",
        path, preview
    ))
    .title("Write Confirmation")
    .button("Allow", move |s| {
        let _ = tx_allow.send(true);
        s.pop_layer();
    })
    .button("Deny", move |s| {
        let _ = tx_deny.send(false);
        s.pop_layer();
    });

    siv.add_layer(dialog);
}
