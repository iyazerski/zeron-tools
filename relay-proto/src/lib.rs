//! Messages between `zeron-tools connect` (on each Mac) and `zeron-relay`.
//!
//! The Mac opens one WebSocket to the relay and keeps it open. Both sides
//! send JSON text frames: the relay sends [`Call`]s, the Mac answers each with
//! an [`Up::Reply`] and reports chat changes with [`Up::Notify`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped when a message changes shape; the relay refuses other versions.
pub const PROTOCOL: u32 = 1;

/// The path the Mac connects to, with `Authorization: Bearer <token>`.
pub const CONNECT_PATH: &str = "/connect";

/// Relay → Mac: run one Zeron tool (`list_chats`, `send_message`, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub id: u64,
    pub tool: String,
    pub args: Value,
}

/// Mac → relay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Up {
    /// The first message after connecting.
    Hello {
        protocol: u32,
        /// The zeron-tools version.
        version: String,
        /// The Zeron version it was built for.
        zeron: String,
    },
    /// The answer to the [`Call`] with this `id`: the tool's result or its error.
    Reply {
        id: u64,
        result: Result<Value, String>,
    },
    /// A chat finished, needs an answer, or failed.
    Notify(Notice),
    /// Zeron's state on the Mac: sent after [`Up::Hello`] and on each change.
    Engine(Engine),
}

/// Whether the Mac's Zeron can take calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Engine {
    Running,
    /// Not open; the `open_zeron` call starts it.
    Closed,
    /// Open, but at a version this zeron-tools doesn't target, so calls are
    /// refused until zeron-tools is updated.
    Outdated {
        running: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub kind: NoticeKind,
    pub chat: String,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NoticeKind {
    Finished,
    Input,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The wire shapes both sides rely on.
    #[test]
    fn messages_keep_their_wire_shape() {
        let reply = Up::Reply {
            id: 7,
            result: Err("no such chat".into()),
        };
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            json!({ "type": "reply", "id": 7, "result": { "Err": "no such chat" } })
        );
        let notify = Up::Notify(Notice {
            kind: NoticeKind::Input,
            chat: "c".into(),
            title: "t".into(),
            body: "b".into(),
        });
        let value = serde_json::to_value(&notify).unwrap();
        assert_eq!(
            value,
            json!({ "type": "notify", "kind": "input", "chat": "c", "title": "t", "body": "b" })
        );
        assert_eq!(serde_json::from_value::<Up>(value).unwrap(), notify);
        let engine = Up::Engine(Engine::Outdated {
            running: "0.3.0".into(),
        });
        let value = serde_json::to_value(&engine).unwrap();
        assert_eq!(
            value,
            json!({ "type": "engine", "state": "outdated", "running": "0.3.0" })
        );
        assert_eq!(serde_json::from_value::<Up>(value).unwrap(), engine);
    }
}
