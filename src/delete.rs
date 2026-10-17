//! Spot chats the agent never finished: you sent a prompt, and every reply was
//! interrupted, failed, or never came.

use zeron_doc::{MessageRole, MessageStatus, SessionMessageEntry};
use zeron_mcp::{RenderOptions, render_entries};

/// Why a chat counts as unfinished, for the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unfinished {
    pub prompts: usize,
    /// What happened to the last reply: `no reply`, `interrupted`, `cut off`,
    /// `no answer`, or `error: <message>`.
    pub outcome: String,
}

/// `Some` if no assistant turn in `entries` ever completed with an answer
/// (text, no error). A chat without any prompt counts too.
pub fn unfinished(entries: &[SessionMessageEntry]) -> Option<Unfinished> {
    // Zeron's renderer folds continuation entries into their turn.
    let messages = render_entries(
        entries,
        RenderOptions {
            include_reasoning: false,
            include_tools: false,
        },
    );
    let replies: Vec<_> = messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .collect();
    let answered = replies.iter().any(|m| {
        // Docs older than message status have none; their turns finished.
        m.status.is_none_or(|s| s == MessageStatus::Complete)
            && m.errors.is_empty()
            && !m.text.trim().is_empty()
    });
    if answered {
        return None;
    }
    let outcome = match replies.last() {
        None => "no reply".into(),
        Some(m) => match (m.errors.first(), m.status) {
            (Some(error), _) => format!("error: {}", crate::one_line(error, 80)),
            (None, Some(MessageStatus::Aborted)) => "interrupted".into(),
            (None, Some(MessageStatus::Streaming)) => "cut off".into(),
            _ => "no answer".into(),
        },
    };
    Some(Unfinished {
        prompts: messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .count(),
        outcome,
    })
}

#[cfg(test)]
mod tests {
    use zeron_doc::MessagePart;

    use super::*;

    fn entry(
        role: MessageRole,
        status: Option<MessageStatus>,
        parts: Vec<MessagePart>,
    ) -> SessionMessageEntry {
        SessionMessageEntry {
            id: format!("{role:?}{}", parts.len()),
            role,
            parts,
            created_at: 0,
            device_id: "d".into(),
            status,
            continuation_of: None,
            duration_ms: None,
        }
    }

    fn text(t: &str) -> Vec<MessagePart> {
        vec![MessagePart::Text {
            id: "p".into(),
            text: t.into(),
        }]
    }

    fn outcome(entries: &[SessionMessageEntry]) -> Option<String> {
        unfinished(entries).map(|u| format!("{} {}", u.prompts, u.outcome))
    }

    #[test]
    fn chats_without_a_finished_answer_are_unfinished() {
        let prompt = entry(MessageRole::User, None, text("Fix the bug"));
        let reply = |status, parts| entry(MessageRole::Assistant, status, parts);
        let complete = Some(MessageStatus::Complete);
        assert_eq!(outcome(&[]).as_deref(), Some("0 no reply"));
        assert_eq!(
            outcome(std::slice::from_ref(&prompt)).as_deref(),
            Some("1 no reply")
        );
        let interrupted = reply(Some(MessageStatus::Aborted), text("I'm looking…"));
        assert_eq!(
            outcome(&[prompt.clone(), interrupted.clone()]).as_deref(),
            Some("1 interrupted")
        );
        let failed = reply(
            complete,
            vec![MessagePart::Error {
                id: "e".into(),
                message: "404 model: gone".into(),
            }],
        );
        assert_eq!(
            outcome(&[prompt.clone(), failed]).as_deref(),
            Some("1 error: 404 model: gone")
        );
        // One finished answer anywhere keeps the chat.
        let answer = reply(complete, text("Fixed."));
        assert_eq!(
            outcome(&[prompt.clone(), interrupted, prompt.clone(), answer]),
            None
        );
        assert_eq!(outcome(&[prompt, reply(None, text("Old doc"))]), None);
    }
}
