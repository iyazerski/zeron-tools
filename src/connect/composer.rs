//! What the desktop composer shows around a chat's message box, for the
//! phone: the checkout and branch, how full the context window is, and the
//! model and reasoning, which the phone can change like the desktop picker.
//! A new chat's composer shows the footer of the project it will run in.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use zeron_mcp::Zeron;
use zeron_proto::{ChatConfig, Model, ReasoningLevel, RepoRef};

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
    let chat = zeron.resolve_chat(chat_arg(args)?).await?;
    let spaces = zeron.spaces().await?;
    // Like the desktop footer: checkout and branch only show in a git project.
    let space = chat
        .space_id
        .as_deref()
        .and_then(|id| spaces.iter().find(|s| s.id == id))
        .filter(|s| s.git_detected);
    let checkout = space.map(|space| {
        let worktree = chat.cwd.as_deref().is_some_and(|cwd| cwd != space.path);
        if worktree { "worktree" } else { "local" }
    });
    let config = chat.config.as_ref();
    Ok(json!({
        "checkout": checkout,
        "branch": space.and(chat.branch.as_deref()),
        "contextUsage": context_usage(zeron, &chat.id).await?,
        "harness": config.map(|c| c.harness),
        "model": config.and_then(|c| c.model.as_deref()),
        "reasoning": config.and_then(|c| c.reasoning),
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
    let refs: Vec<RepoRef> = serde_json::from_value(
        zeron
            .call(
                zeron_rpc::methods::LIST_REFS,
                json!({ "repoPath": space.path }),
            )
            .await?,
    )
    .context("unexpected ListRefs reply")?;
    // A detached HEAD has no current branch.
    let branch = refs.into_iter().find(|r| r.current).map(|r| r.name);
    Ok(json!({ "checkout": "local", "branch": branch }))
}

/// The context usage the host recorded for a chat: the opening frame of its
/// transcript stream carries it. `openingTail` keeps that frame to the last
/// entries instead of the whole transcript.
async fn context_usage(zeron: &Zeron, chat_id: &str) -> Result<Value> {
    let mut stream = zeron
        .subscribe(
            zeron_rpc::methods::WATCH_DOC_MESSAGES,
            json!({ "chatId": chat_id, "openingTail": true }),
        )
        .await?;
    let first = tokio::time::timeout(USAGE_TIMEOUT, stream.recv())
        .await
        .context("Zeron didn't send the transcript in time")?
        .context("the transcript stream ended early")?;
    Ok(first.get("contextUsage").cloned().unwrap_or(Value::Null))
}

/// Change a chat's `model` and/or `reasoning` for its next turns, like the
/// desktop picker: the model must be in the harness's catalog, reasoning is
/// healed to a level the model offers, and options it doesn't offer drop.
pub async fn set_model(zeron: &Zeron, args: &Value) -> Result<Value> {
    let chat = zeron.resolve_chat(chat_arg(args)?).await?;
    let Some(mut config) = chat.config.clone() else {
        bail!("this chat has no model yet; send it a message first");
    };
    let models = zeron.models(config.harness).await?;
    if let Some(id) = args.get("model").and_then(Value::as_str) {
        if !models.iter().any(|m| m.id == id) {
            bail!("{id} isn't one of this agent's models");
        }
        config.model = Some(id.to_owned());
    }
    if let Some(level) = args.get("reasoning").filter(|v| !v.is_null()) {
        let level: ReasoningLevel =
            serde_json::from_value(level.clone()).context("unknown reasoning level")?;
        config.reasoning = Some(level);
    }
    let harness_levels = zeron
        .harnesses()
        .await?
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
fn fit_to_model(config: &mut ChatConfig, model: Option<&Model>, harness_levels: &[ReasoningLevel]) {
    let ladder = model
        .map(|m| m.reasoning_levels.as_slice())
        .filter(|levels| !levels.is_empty())
        .unwrap_or(harness_levels);
    if !ladder.is_empty() && !config.reasoning.is_some_and(|r| ladder.contains(&r)) {
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

/// The desktop's default level for a ladder: High, else Medium, else the first.
fn default_reasoning(ladder: &[ReasoningLevel]) -> Option<ReasoningLevel> {
    [ReasoningLevel::High, ReasoningLevel::Medium]
        .into_iter()
        .find(|level| ladder.contains(level))
        .or_else(|| ladder.first().copied())
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
    use zeron_proto::{HarnessId, SandboxLevel};

    fn model(levels: Vec<ReasoningLevel>, options: Value) -> Model {
        serde_json::from_value(json!({
            "id": "m", "label": "M", "reasoningLevels": levels, "options": options,
        }))
        .unwrap()
    }

    fn config(reasoning: Option<ReasoningLevel>, options: Value) -> ChatConfig {
        ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("m".into()),
            reasoning,
            model_options: serde_json::from_value(options).unwrap(),
            sandbox: SandboxLevel::WorkspaceWrite,
        }
    }

    #[test]
    fn a_model_switch_heals_reasoning_and_drops_foreign_options() {
        use ReasoningLevel::*;
        let options = json!([{ "id": "ctx", "label": "Context", "defaultChoice": "200k",
            "choices": [{ "id": "200k", "label": "200K" }, { "id": "1m", "label": "1M" }] }]);
        let picked = model(vec![Low, Medium, High], options);

        let mut kept = config(Some(Low), json!({ "ctx": "1m" }));
        fit_to_model(&mut kept, Some(&picked), &[]);
        assert_eq!(kept.reasoning, Some(Low));
        assert_eq!(
            kept.model_options,
            serde_json::from_value(json!({ "ctx": "1m" })).unwrap()
        );

        let mut healed = config(Some(Max), json!({ "ctx": "2m", "fast": "on" }));
        fit_to_model(&mut healed, Some(&picked), &[]);
        assert_eq!(healed.reasoning, Some(High));
        assert!(healed.model_options.is_empty());

        // A model without its own ladder uses the harness's.
        let mut fallback = config(Some(XHigh), json!({}));
        fit_to_model(
            &mut fallback,
            Some(&model(vec![], json!([]))),
            &[Minimal, Low],
        );
        assert_eq!(fallback.reasoning, Some(Minimal));
    }
}
