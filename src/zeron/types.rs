//! Zeron's entities as this tool reads them from the engine's watch streams
//! (`crates/proto/src/entities.rs`, `agent.rs` in Zeron).
//!
//! Each type holds only the fields used here. Unknown fields are ignored and
//! unknown enum values parse as `Other`, so additive Zeron changes keep
//! parsing; only a removed or retyped field breaks it. Harness ids, models,
//! and reasoning levels stay strings: they're passed back as they came.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Chat ids of Zeron's hidden voice orchestrators (`zeron_proto::voice`).
const VOICE_ORCHESTRATOR_PREFIX: &str = "voice-orchestrator-";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub id: String,
    /// The device that hosts the chat's runs.
    pub device_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub config: Option<ChatConfig>,
    #[serde(default)]
    pub last_message_preview: Option<String>,
    #[serde(default)]
    pub last_message_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub harness_session_cwd: Option<String>,
    #[serde(default)]
    pub space_id: Option<String>,
    #[serde(default)]
    pub parent_chat_id: Option<String>,
}

impl Chat {
    /// The chat's newest message, else its creation: the sidebar's order.
    pub fn last_activity(&self) -> DateTime<Utc> {
        self.last_message_at.unwrap_or(self.created_at)
    }

    /// Whether the sidebar lists it: not a side chat or a voice orchestrator.
    pub fn is_top_level(&self) -> bool {
        self.parent_chat_id.is_none() && !self.id.starts_with(VOICE_ORCHESTRATOR_PREFIX)
    }

    /// The title, if it has a non-blank one.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref().filter(|t| !t.trim().is_empty())
    }

    pub fn harness(&self) -> Option<&str> {
        self.config.as_ref().map(|c| c.harness.as_str())
    }

    pub fn model(&self) -> Option<&str> {
        self.config.as_ref().and_then(|c| c.model.as_deref())
    }

    pub fn reasoning(&self) -> Option<&str> {
        self.config.as_ref().and_then(|c| c.reasoning.as_deref())
    }
}

/// A chat's agent settings. Fields not named here (`sandbox`, …) ride in
/// `rest`, so writing a config back keeps them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatConfig {
    pub harness: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub model_options: Map<String, Value>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// A project: a folder on one device.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Space {
    pub id: String,
    pub device_id: String,
    pub path: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub git_detected: bool,
    /// Shared by every checkout of one repository.
    #[serde(default)]
    pub repository_id: Option<String>,
}

impl Space {
    /// Its name, else its folder's name (`Space::display_name` in Zeron).
    pub fn display_name(&self) -> &str {
        if let Some(name) = self.name.as_deref().filter(|n| !n.trim().is_empty()) {
            return name;
        }
        let trimmed = self.path.trim_end_matches(['/', '\\']);
        trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
    }

    /// The sidebar's project: every checkout of one repository is one
    /// (`zeron_proto::view::project_key`).
    pub fn project_key(&self) -> String {
        match self.repository_id.as_deref() {
            Some(repository) => format!("repo:{repository}"),
            None => self.id.clone(),
        }
    }
}

/// A chat's live state on one device.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub chat_id: String,
    pub device_id: String,
    pub status: SessionStatus,
    /// Changes when a turn completes.
    #[serde(default)]
    pub last_completed_turn: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionStatus {
    Idle,
    Working,
    AwaitingInput,
    Errored,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    /// The Zeron version it last ran.
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// A `ListHarnesses` row.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Harness {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub supports_steering: bool,
    #[serde(default)]
    pub steering_mode: Option<String>,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    #[serde(default = "yes")]
    pub installed: bool,
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn yes() -> bool {
    true
}

impl Harness {
    /// Offered here: installed and not switched off in Settings.
    pub fn available(&self) -> bool {
        self.installed && self.enabled.unwrap_or(true)
    }

    /// Whether it takes a prompt inside a running turn (the composer's "Steer").
    pub fn steers_mid_turn(&self) -> bool {
        self.supports_steering && self.steering_mode.as_deref() == Some("step-boundary")
    }
}

/// A `ListModels` row.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    #[serde(default)]
    pub options: Vec<ModelOption>,
}

/// A `ListSkills` row: a skill the composer's `$` menu offers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Skill {
    pub name: String,
    /// Its `SKILL.md`, or `harness-skill:<harness>:<name>` for one the agent
    /// reports itself.
    pub path: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Off when it's switched off in the agent's own settings.
    pub enabled: bool,
    /// How the agent runs it as a command, if it does.
    #[serde(default)]
    pub command: Option<SkillCommand>,
}

/// A skill's command; serialized in Zeron's field order, as prompt
/// references carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillCommand {
    pub name: String,
    pub harness: String,
}

/// A model setting with fixed choices, like its context window.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelOption {
    pub id: String,
    #[serde(default)]
    pub choices: Vec<Choice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    pub id: String,
}

/// Pinned chats and user-made sidebar sections.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidebarPreferences {
    #[serde(default)]
    pub pinned_session_ids: Vec<String>,
    #[serde(default)]
    pub sections: Vec<SidebarSection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidebarSection {
    #[serde(default)]
    pub session_ids: Vec<String>,
}

impl SidebarPreferences {
    /// Chats the user placed: pinned or in a section.
    pub fn placed(&self) -> impl Iterator<Item = &str> {
        self.pinned_session_ids
            .iter()
            .chain(self.sections.iter().flat_map(|s| &s.session_ids))
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_fields_and_values_still_parse() {
        let chat: Chat = serde_json::from_value(json!({
            "id": "c", "deviceId": "d", "createdAt": "2026-10-19T10:00:00Z",
            "someNewField": { "x": 1 },
            "config": { "harness": "a-new-harness", "model": null, "sandbox": "workspace-write", "newKnob": 3 },
        }))
        .unwrap();
        assert!(chat.is_top_level() && chat.title().is_none());
        let config = chat.config.unwrap();
        assert_eq!(config.harness, "a-new-harness");
        // What isn't named here goes back unchanged.
        let back = serde_json::to_value(&config).unwrap();
        assert_eq!(back["sandbox"], "workspace-write");
        assert_eq!(back["newKnob"], 3);

        let session: Session = serde_json::from_value(json!({
            "chatId": "c", "deviceId": "d", "status": "somethingNew",
            "updatedAt": "2026-10-19T10:00:00Z",
        }))
        .unwrap();
        assert_eq!(session.status, SessionStatus::Other);
    }

    #[test]
    fn projects_group_checkouts_of_one_repository() {
        let space = |id: &str, repo: Option<&str>| -> Space {
            serde_json::from_value(json!({
                "id": id, "deviceId": "d", "path": format!("/w/{id}/"), "repositoryId": repo,
            }))
            .unwrap()
        };
        assert_eq!(space("a", Some("r")).project_key(), "repo:r");
        assert_eq!(space("b", None).project_key(), "b");
        assert_eq!(space("b", None).display_name(), "b");
    }
}
