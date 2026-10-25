//! Report chats that finish, need an answer, or fail, so the relay can
//! notify the phone.
//!
//! It follows the engine's `WatchSessions` stream and reports a status
//! change. By default it stays quiet while the Mac is in use, since then
//! you're at the Mac, not on the phone.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::sync::{mpsc, watch};
use zeron_relay_proto::{Engine, Notice, NoticeKind};

use crate::text::one_line;
use crate::zeron::transcript::{self, RenderOptions};
use crate::zeron::types::{Session, SessionStatus};
use crate::zeron::{Zeron, methods};

/// The Mac counts as in use if it had keyboard or mouse input this recently.
const AWAY_AFTER: Duration = Duration::from_secs(120);
/// Wait before resubscribing after the stream failed while Zeron runs.
const RETRY: Duration = Duration::from_secs(5);
/// How long building one notice (reading its chat) may take.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long reading the Mac's idle time may take.
const IOREG_TIMEOUT: Duration = Duration::from_secs(5);
/// The most characters in a notice's body.
const BODY_CHARS: usize = 240;

/// When to report chats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyWhen {
    /// Only while nobody uses the Mac: at it, you see the chat yourself.
    Away,
    /// Whether or not someone uses the Mac.
    Always,
}

impl NotifyWhen {
    /// Whether a notice may go out now.
    async fn allows_now(self) -> bool {
        match self {
            Self::Always => true,
            Self::Away => away().await,
        }
    }
}

/// Follow chat statuses whenever Zeron runs, and send notices for changes.
pub async fn run(
    zeron: Arc<Zeron>,
    mut engine: watch::Receiver<Engine>,
    notices: mpsc::Sender<Notice>,
    notify: NotifyWhen,
) {
    loop {
        // An outdated Zeron may speak another IPC, so it isn't watched either.
        if engine.wait_for(Engine::is_running).await.is_err() {
            return;
        }
        if let Err(e) = watch_once(&zeron, &notices, notify).await {
            // Zeron quitting ends the stream too; the engine log covers that.
            if engine.borrow().is_running() {
                eprintln!("notifications paused: {e:#}");
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Follow one `WatchSessions` stream until it ends.
async fn watch_once(
    zeron: &Arc<Zeron>,
    notices: &mpsc::Sender<Notice>,
    notify: NotifyWhen,
) -> Result<()> {
    let mut stream = zeron.subscribe(methods::WATCH_SESSIONS, json!({})).await?;
    // The first snapshot is the baseline: nothing has changed yet.
    let mut known: Option<HashMap<String, Session>> = None;
    while let Some(item) = stream.next().await? {
        let sessions: Vec<Session> =
            serde_json::from_value(item).context("WatchSessions: bad snapshot")?;
        let current: HashMap<String, Session> = sessions
            .into_iter()
            .map(|s| (format!("{}/{}", s.chat_id, s.device_id), s))
            .collect();
        if let Some(previous) = &known {
            let changes = changes(previous, &current);
            if !changes.is_empty() && notify.allows_now().await {
                for (chat, kind) in changes {
                    spawn_notice(zeron.clone(), notices.clone(), chat, kind);
                }
            }
        }
        known = Some(current);
    }
    bail!("the engine closed the session stream")
}

/// The notices the move from `previous` to `current` sessions (by
/// `chat/device`) deserves, as chat ids and kinds.
fn changes(
    previous: &HashMap<String, Session>,
    current: &HashMap<String, Session>,
) -> Vec<(String, NoticeKind)> {
    current
        .iter()
        .filter_map(|(key, now)| {
            let kind = change(previous.get(key)?, now)?;
            Some((now.chat_id.clone(), kind))
        })
        .collect()
}

/// Build and queue the notice for `chat` in its own task.
fn spawn_notice(zeron: Arc<Zeron>, notices: mpsc::Sender<Notice>, chat: String, kind: NoticeKind) {
    // Read the chat off the stream's path: the engine's status watch keeps
    // only its latest value, so a slow read there would make it skip
    // transitions.
    tokio::spawn(async move {
        match tokio::time::timeout(NOTICE_TIMEOUT, notice(&zeron, &chat, kind)).await {
            // A full queue means the relay has been away for long; drop it.
            Ok(Ok(notice)) => _ = notices.try_send(notice),
            Ok(Err(e)) => eprintln!("notification for {chat}: {e:#}"),
            Err(_) => eprintln!("notification for {chat}: Zeron didn't answer in time"),
        }
    });
}

/// Which notice, if any, a session's move from `before` to `now` deserves.
///
/// A turn finished when the session settles in `idle` with a newer
/// `last_completed_turn`, whatever it was before: the status watch can skip
/// a short `working` (say, right after you answered a question). The engine
/// sets both together. An interrupted turn also ends in `idle`, but without
/// advancing the marker; you stopped it yourself, so it stays quiet.
fn change(before: &Session, now: &Session) -> Option<NoticeKind> {
    match now.status {
        SessionStatus::AwaitingInput if before.status != SessionStatus::AwaitingInput => {
            Some(NoticeKind::Input)
        }
        SessionStatus::Errored if before.status != SessionStatus::Errored => {
            Some(NoticeKind::Failed)
        }
        SessionStatus::Idle
            if now.last_completed_turn.is_some()
                && now.last_completed_turn != before.last_completed_turn =>
        {
            Some(NoticeKind::Finished)
        }
        _ => None,
    }
}

/// Build the notice for `chat_id` from the chat's current state: the
/// question it asks, or its last message.
async fn notice(zeron: &Zeron, chat_id: &str, kind: NoticeKind) -> Result<Notice> {
    let chat = zeron.chat(chat_id).await?;
    let body = match kind {
        NoticeKind::Failed => "The turn failed.".to_owned(),
        NoticeKind::Input => {
            let entries = zeron.transcript(&chat).await?;
            let rendered = transcript::render(&entries, RenderOptions::default());
            let pending = transcript::pending_input(&rendered).unwrap_or_default();
            let question = pending["questions"][0]["question"]
                .as_str()
                .unwrap_or_default();
            format!("Needs your answer: {}", question.trim())
        }
        NoticeKind::Finished => {
            let entries = zeron.transcript(&chat).await?;
            let rendered = transcript::render(&entries, RenderOptions::default());
            rendered
                .last()
                .map(|m| m.text.trim().to_owned())
                .unwrap_or_default()
        }
    };
    Ok(Notice {
        kind,
        chat: chat.id.clone(),
        title: chat.title().unwrap_or("Untitled chat").trim().to_owned(),
        body: one_line(&body, BODY_CHARS),
    })
}

/// Whether nobody has touched the Mac's keyboard or mouse for [`AWAY_AFTER`].
/// If idle time can't be read, assume away: a missed notice is worse.
async fn away() -> bool {
    let output = tokio::process::Command::new("ioreg")
        .args(["-c", "IOHIDSystem", "-d", "4"])
        .kill_on_drop(true)
        .output();
    let idle = match tokio::time::timeout(IOREG_TIMEOUT, output).await {
        Ok(Ok(o)) => hid_idle(&String::from_utf8_lossy(&o.stdout)),
        _ => None,
    };
    idle.is_none_or(|idle| idle >= AWAY_AFTER)
}

/// Read `"HIDIdleTime" = <nanoseconds>` from `ioreg` output.
fn hid_idle(ioreg: &str) -> Option<Duration> {
    let line = ioreg.lines().find(|l| l.contains("\"HIDIdleTime\""))?;
    let nanos = line.rsplit('=').next()?.trim().parse().ok()?;
    Some(Duration::from_nanos(nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(status: SessionStatus, completed: Option<&str>) -> Session {
        serde_json::from_value(json!({
            "chatId": "c", "deviceId": "d", "status": status,
            "lastCompletedTurn": completed, "updatedAt": "2026-10-19T10:00:00Z",
        }))
        .unwrap()
    }

    #[test]
    fn status_changes_pick_the_notice() {
        use NoticeKind::*;
        use SessionStatus::*;
        let cases = [
            (
                session(Working, Some("t1")),
                session(Idle, Some("t2")),
                Some(Finished),
            ),
            (
                session(Working, None),
                session(Idle, Some("t1")),
                Some(Finished),
            ),
            // The watch skipped `working` after an answered question.
            (
                session(AwaitingInput, Some("t1")),
                session(Idle, Some("t2")),
                Some(Finished),
            ),
            // Interrupted: the completed-turn marker stays put.
            (
                session(AwaitingInput, Some("t1")),
                session(Idle, Some("t1")),
                None,
            ),
            (
                session(Working, Some("t1")),
                session(Idle, Some("t1")),
                None,
            ),
            (
                session(Working, None),
                session(AwaitingInput, None),
                Some(Input),
            ),
            (
                session(AwaitingInput, None),
                session(AwaitingInput, None),
                None,
            ),
            (session(Working, None), session(Errored, None), Some(Failed)),
            (
                session(Idle, Some("t1")),
                session(Working, Some("t1")),
                None,
            ),
        ];
        for (before, now, want) in cases {
            assert_eq!(
                change(&before, &now),
                want,
                "{:?} -> {:?}",
                before.status,
                now.status
            );
        }
    }

    #[test]
    fn idle_time_comes_from_ioreg() {
        let out = "    | |   \"HIDIdleSomething\" = 1\n    | |     \"HIDIdleTime\" = 19338182208\n";
        assert_eq!(hid_idle(out), Some(Duration::from_nanos(19_338_182_208)));
        assert_eq!(hid_idle("nothing"), None);
    }
}
