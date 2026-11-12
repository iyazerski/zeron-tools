//! What the desktop composer shows around a chat's message box, for the
//! phone: checkout and branch, context usage, model and reasoning, and the
//! agent's skills for the `$` menu.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::tools::default_harness;
use crate::zeron::types::{ChatConfig, Model, Skill, SkillCommand};
use crate::zeron::{SkillScope, Zeron, methods, space_of};

/// How long to wait for the transcript's first frame, which carries the
/// context usage.
const USAGE_TIMEOUT: Duration = Duration::from_secs(10);

/// The `composer_info` arguments: a chat, or the project of a chat not
/// created yet.
#[derive(Debug, Deserialize)]
pub struct InfoArgs {
    chat: Option<String>,
    /// A project (space) id.
    project: Option<String>,
}

/// Return the chat's composer footer: `checkout` (`local` or `worktree`,
/// only in a git project), `branch`, `contextUsage` (`{tokens, window}`),
/// and its saved `harness`, `model`, and `reasoning`. Without `chat`, return
/// the footer of a chat not created yet ([`draft_info`]).
pub async fn info(zeron: &Zeron, args: InfoArgs) -> Result<Value> {
    let Some(chat) = args.chat else {
        return draft_info(zeron, args.project.as_deref()).await;
    };
    let chat = zeron.chat(&chat).await?;
    let spaces = zeron.spaces().await?;
    // Like the desktop footer: checkout and branch only show in a git project.
    let space = space_of(&chat, &spaces).filter(|s| s.git_detected);
    let checkout = space.map(|space| {
        let worktree = chat.cwd.as_deref().is_some_and(|cwd| cwd != space.path);
        if worktree { "worktree" } else { "local" }
    });
    Ok(json!({
        "checkout": checkout,
        "branch": space.and(chat.branch.as_deref()),
        "contextUsage": context_usage(zeron, &chat.id).await?,
        "harness": chat.harness(),
        "model": chat.model(),
        "reasoning": chat.reasoning(),
    }))
}

/// The footer for a new chat in `project`: like the desktop's, the project
/// folder's checked-out branch, and nothing outside git.
async fn draft_info(zeron: &Zeron, project: Option<&str>) -> Result<Value> {
    let spaces = zeron.spaces().await?;
    let space = project
        .and_then(|id| spaces.iter().find(|s| s.id == id))
        .filter(|s| s.git_detected);
    let Some(space) = space else {
        return Ok(json!({ "checkout": null, "branch": null }));
    };
    let refs = zeron
        .call(methods::LIST_REFS, json!({ "repoPath": space.path }))
        .await?;
    // A detached HEAD has no current branch.
    let branch = refs
        .as_array()
        .context("ListRefs: unexpected reply")?
        .iter()
        .find(|r| r["current"] == true)
        .and_then(|r| r["name"].as_str());
    Ok(json!({ "checkout": "local", "branch": branch }))
}

/// The context usage the host recorded for a chat: the opening frame of its
/// transcript stream carries it. `openingTail` keeps that frame to the last
/// entries instead of the whole transcript.
async fn context_usage(zeron: &Zeron, chat_id: &str) -> Result<Value> {
    let mut stream = zeron
        .subscribe(
            methods::WATCH_DOC_MESSAGES,
            json!({ "chatId": chat_id, "openingTail": true }),
        )
        .await?;
    let first = tokio::time::timeout(USAGE_TIMEOUT, stream.next())
        .await
        .context("Zeron didn't send the transcript in time")??
        .context("the transcript stream ended early")?;
    Ok(first.get("contextUsage").cloned().unwrap_or(Value::Null))
}

/// The `list_skills` arguments: a chat, or the `harness` and `project` of
/// a chat not created yet.
#[derive(Debug, Deserialize)]
pub struct SkillsArgs {
    chat: Option<String>,
    harness: Option<String>,
    /// A project (space) id.
    project: Option<String>,
}

/// Return the enabled skills of the chat's agent in its checkout, or without
/// `chat`, of the `harness` a new chat will use in `project` (the global ones
/// without a project). Each has the `ref` that invokes it from the prompt.
pub async fn skills(zeron: &Zeron, args: SkillsArgs) -> Result<Value> {
    let skills = if let Some(chat) = args.chat {
        let chat = zeron.chat(&chat).await?;
        // A chat that never ran gets the default agent on its first turn.
        let harness = match chat.harness() {
            Some(harness) => harness.to_owned(),
            None => default_harness(&zeron.harnesses().await?)?,
        };
        zeron.skills(&harness, SkillScope::Chat(&chat.id)).await?
    } else {
        let harness = args
            .harness
            .filter(|h| !h.is_empty())
            .context("harness is required")?;
        let scope = match args.project.as_deref() {
            Some(id) => SkillScope::Project(id),
            None => SkillScope::Global,
        };
        zeron.skills(&harness, scope).await?
    };
    let skills: Vec<Value> = skills
        .iter()
        .filter(|s| s.enabled)
        .map(|s| json!({ "name": s.name, "description": s.description, "ref": invoke_ref(s) }))
        .collect();
    Ok(json!({ "skills": skills }))
}

/// What a prompt reference invokes (`Invocation::Skill` in Zeron, tagged
/// `"kind": "skill"`), in the desktop composer's field order.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename = "skill")]
struct SkillInvocation<'a> {
    name: &'a str,
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<&'a SkillCommand>,
}

/// The prompt reference the desktop composer writes for a picked skill:
/// `[$name](zeron-invoke:<hex of the invocation JSON>)`. The engine reads it
/// when the turn starts and runs the skill the agent's way.
fn invoke_ref(skill: &Skill) -> String {
    let invocation = SkillInvocation {
        name: &skill.name,
        path: &skill.path,
        command: skill.command.as_ref(),
    };
    // Straight from the structs: a `Value` would sort the keys.
    let json = serde_json::to_vec(&invocation).expect("a struct of strings always serializes");
    let hex: String = json.iter().map(|b| format!("{b:02x}")).collect();
    format!("[${}](zeron-invoke:{hex})", skill.name)
}

/// The `set_chat_model` arguments; a missing `model` or `reasoning` keeps
/// the chat's.
#[derive(Debug, Deserialize)]
pub struct SetModelArgs {
    chat: String,
    model: Option<String>,
    reasoning: Option<String>,
}

/// Change a chat's `model` and/or `reasoning` for its next turns, like the
/// desktop picker: the model must be in the harness's catalog, reasoning is
/// healed to a level the model offers, and options it doesn't offer drop.
pub async fn set_model(zeron: &Zeron, args: SetModelArgs) -> Result<Value> {
    let chat = zeron.chat(&args.chat).await?;
    let Some(mut config) = chat.config else {
        bail!("this chat has no model yet; send it a message first");
    };
    let (models, harnesses) = tokio::try_join!(zeron.models(&config.harness), zeron.harnesses())?;
    if let Some(id) = args.model {
        if !models.iter().any(|m| m.id == id) {
            bail!("{id} isn't one of this agent's models");
        }
        config.model = Some(id);
    }
    if let Some(level) = args.reasoning {
        config.reasoning = Some(level);
    }
    let harness_levels = harnesses
        .into_iter()
        .find(|h| h.id == config.harness)
        .map(|h| h.reasoning_levels)
        .unwrap_or_default();
    let model = config
        .model
        .as_deref()
        .and_then(|id| models.iter().find(|m| m.id == id));
    fit_to_model(&mut config, model, &harness_levels);
    zeron
        .mutate(json!({ "op": "setChatConfig", "chatId": chat.id, "config": config }))
        .await?;
    Ok(json!({ "model": config.model, "reasoning": config.reasoning }))
}

/// Keep `config` valid for its model, as the desktop picker does: reasoning
/// on the model's ladder (else the harness's), and only offered options.
fn fit_to_model(config: &mut ChatConfig, model: Option<&Model>, harness_levels: &[String]) {
    let ladder = model
        .map(|m| m.reasoning_levels.as_slice())
        .filter(|levels| !levels.is_empty())
        .unwrap_or(harness_levels);
    if !ladder.is_empty()
        && !config
            .reasoning
            .as_ref()
            .is_some_and(|r| ladder.contains(r))
    {
        config.reasoning = default_reasoning(ladder);
    }
    if let Some(model) = model {
        config.model_options.retain(|id, choice| {
            model.options.iter().any(|option| {
                option.id == *id
                    && choice
                        .as_str()
                        .is_some_and(|choice| option.choices.iter().any(|c| c.id == choice))
            })
        });
    }
}

/// The desktop's default level for a ladder: high, else medium, else the first.
fn default_reasoning(ladder: &[String]) -> Option<String> {
    ["high", "medium"]
        .into_iter()
        .find(|level| ladder.iter().any(|l| l == level))
        .map(str::to_owned)
        .or_else(|| ladder.first().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(levels: &[&str], options: Value) -> Model {
        serde_json::from_value(json!({
            "id": "m", "label": "M", "reasoningLevels": levels, "options": options,
        }))
        .unwrap()
    }

    fn config(reasoning: Option<&str>, options: Value) -> ChatConfig {
        serde_json::from_value(json!({
            "harness": "claude-code", "model": "m", "reasoning": reasoning,
            "modelOptions": options, "sandbox": "workspace-write",
        }))
        .unwrap()
    }

    #[test]
    fn a_skill_ref_is_the_desktops() {
        let skill: Skill = serde_json::from_value(json!({
            "name": "second-opinion", "path": "harness-skill:pi:second-opinion",
            "description": "Consult", "enabled": true,
            "command": { "name": "skill:second-opinion", "harness": "pi" },
        }))
        .unwrap();
        // Copied from a prompt the desktop composer sent.
        assert_eq!(
            invoke_ref(&skill),
            "[$second-opinion](zeron-invoke:7b226b696e64223a22736b696c6c222c226e616d65223a227365636f6e642d6f70696e696f6e222c2270617468223a226861726e6573732d736b696c6c3a70693a7365636f6e642d6f70696e696f6e222c22636f6d6d616e64223a7b226e616d65223a22736b696c6c3a7365636f6e642d6f70696e696f6e222c226861726e657373223a227069227d7d)"
        );
    }

    #[test]
    fn a_model_switch_heals_reasoning_and_drops_foreign_options() {
        let options = json!([{ "id": "ctx", "label": "Context", "defaultChoice": "200k",
            "choices": [{ "id": "200k", "label": "200K" }, { "id": "1m", "label": "1M" }] }]);
        let picked = model(&["low", "medium", "high"], options);

        let mut kept = config(Some("low"), json!({ "ctx": "1m" }));
        fit_to_model(&mut kept, Some(&picked), &[]);
        assert_eq!(kept.reasoning.as_deref(), Some("low"));
        assert_eq!(Value::Object(kept.model_options), json!({ "ctx": "1m" }));

        let mut healed = config(Some("max"), json!({ "ctx": "2m", "fast": "on" }));
        fit_to_model(&mut healed, Some(&picked), &[]);
        assert_eq!(healed.reasoning.as_deref(), Some("high"));
        assert!(healed.model_options.is_empty());
        // Fields this tool doesn't know go back as they were.
        assert_eq!(
            serde_json::to_value(&healed).unwrap()["sandbox"],
            "workspace-write"
        );

        // A model without its own ladder uses the harness's.
        let mut fallback = config(Some("xhigh"), json!({}));
        let harness = ["minimal".to_owned(), "low".to_owned()];
        fit_to_model(&mut fallback, Some(&model(&[], json!([]))), &harness);
        assert_eq!(fallback.reasoning.as_deref(), Some("minimal"));
    }
}
