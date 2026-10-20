//! What the desktop composer shows around a chat's message box, for the
//! phone: the checkout and branch, how full the context window is, and the
//! model and reasoning, which the phone can change like the desktop picker
//! (`crates/ui/src/pickers.rs` `update_chat_config`). A new chat's composer
//! shows the footer of the project it will run in.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::zeron::types::{ChatConfig, Model};
use crate::zeron::{Zeron, methods};

/// How long to wait for the transcript's first frame, which carries the
/// context usage.
const USAGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Return the chat's composer footer: `checkout` (`local` or `worktree`,
/// only in a git project), `branch`, `contextUsage` (`{tokens, window}`),
/// and its saved `harness`, `model`, and `reasoning`. Without `chat`, return
/// the footer of a chat not created yet ([`draft_info`]).
pub async fn info(zeron: &Zeron, args: &Value) -> Result<Value> {
    if args.get("chat").is_none() {
        return draft_info(zeron, args).await;
    }
    let chat = zeron.chat(chat_arg(args)?).await?;
    let spaces = zeron.spaces().await?;
    // Like the desktop footer: checkout and branch only show in a git project.
    let space = super::tools::space_of(&chat, &spaces).filter(|s| s.git_detected);
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

/// The footer for a new chat in the `project` argument: like the desktop's,
/// the project folder's checkout on its checked-out branch, and nothing
/// without a project or outside git. The phone records `branch` on the chat
/// it creates, as the desktop does.
async fn draft_info(zeron: &Zeron, args: &Value) -> Result<Value> {
    let spaces = zeron.spaces().await?;
    let space = args
        .get("project")
        .and_then(Value::as_str)
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

/// Change a chat's `model` and/or `reasoning` for its next turns, like the
/// desktop picker: the model must be in the harness's catalog, reasoning is
/// healed to a level the model offers, and options it doesn't offer drop.
pub async fn set_model(zeron: &Zeron, args: &Value) -> Result<Value> {
    let chat = zeron.chat(chat_arg(args)?).await?;
    let Some(mut config) = chat.config.clone() else {
        bail!("this chat has no model yet; send it a message first");
    };
    let (models, harnesses) = tokio::try_join!(zeron.models(&config.harness), zeron.harnesses())?;
    if let Some(id) = args.get("model").and_then(Value::as_str) {
        if !models.iter().any(|m| m.id == id) {
            bail!("{id} isn't one of this agent's models");
        }
        config.model = Some(id.to_owned());
    }
    if let Some(level) = args.get("reasoning").filter(|v| !v.is_null()) {
        let level = level.as_str().context("reasoning must be a level name")?;
        config.reasoning = Some(level.to_owned());
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
        let options = std::mem::take(&mut config.model_options);
        config.model_options = options
            .into_iter()
            .filter(|(id, choice)| {
                model.options.iter().any(|option| {
                    option.id == *id
                        && choice
                            .as_str()
                            .is_some_and(|choice| option.choices.iter().any(|c| c.id == choice))
                })
            })
            .collect::<Map<_, _>>();
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

/// The `chat` argument: the chat's id.
pub(super) fn chat_arg(args: &Value) -> Result<&str> {
    args.get("chat")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("chat is required")
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
