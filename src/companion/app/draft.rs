//! A new chat until its first message creates it: its project, agent, model
//! and reasoning, and each agent's catalog of models to pick from.

use serde_json::json;

use super::{Page, State, Target};
use crate::companion::data::{
    Created, Harnesses, Model, Models, Picked, Project, Projects, levels_of,
};
use crate::companion::prefs::{NewChat, ProjectPick};

/// The picks of a chat that isn't created yet.
pub struct Draft {
    pub projects: Vec<Project>,
    pub harnesses: Vec<String>,
    pub project: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub load: Load,
    pub creating: bool,
    saved_project: ProjectPick,
}

/// Where loading a draft's projects and agents is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Load {
    Loading,
    Ready,
    /// A load failed; it runs again when Zeron next opens.
    Failed,
}

/// The harness, model and reasoning the composer shows.
#[derive(Clone, Copy, Default)]
pub struct Config<'a> {
    pub harness: Option<&'a str>,
    pub model: Option<&'a str>,
    pub reasoning: Option<&'a str>,
}

/// One pick in the model sheet.
pub enum Pick {
    /// A new chat's agent.
    Harness(String),
    Model(String),
    Reasoning(String),
}

pub enum Catalog {
    Loading,
    Ready {
        models: Vec<Model>,
        levels: Vec<String>,
    },
    /// Loading failed; the next [`State::catalog`] call tries again.
    Failed(String),
}

impl Draft {
    /// A draft with the last new chat's picks, kept if they're still valid once loaded.
    pub(super) fn new(saved: NewChat) -> Self {
        let NewChat {
            project,
            harness,
            model,
            reasoning,
        } = saved;
        Self {
            projects: Vec::new(),
            harnesses: Vec::new(),
            project: match &project {
                ProjectPick::Project(id) => Some(id.clone()),
                ProjectPick::Recent | ProjectPick::NoProject => None,
            },
            harness,
            model,
            reasoning,
            load: Load::Loading,
            creating: false,
            saved_project: project,
        }
    }
}

impl Catalog {
    pub fn ready(&self) -> Option<(&[Model], &[String])> {
        match self {
            Self::Ready { models, levels } => Some((models, levels)),
            Self::Loading | Self::Failed(_) => None,
        }
    }
}

impl State {
    /// Load a new chat's projects and agents, keeping valid picks.
    pub(super) fn load_draft(&mut self) {
        let Some(page) = self.chat_mut() else { return };
        let nav = page.nav;
        if let Some(draft) = page.draft_mut() {
            draft.load = Load::Loading;
        }
        self.api.call(
            "list_projects",
            json!({}),
            move |s, result: Result<Projects, String>, _| {
                let recent = s
                    .list
                    .chats
                    .as_ref()
                    .and_then(|c| c.first()?.project.as_ref())
                    .map(|p| p.id.clone());
                let Some(draft) = s.page_at(nav).and_then(|p| p.draft_mut()) else {
                    return;
                };
                match result {
                    Ok(mut got) => {
                        got.projects.sort_by(|a, b| a.name.cmp(&b.name));
                        draft.projects = got.projects;
                        if draft.saved_project == ProjectPick::Recent {
                            draft.project = recent;
                        }
                        if !draft
                            .projects
                            .iter()
                            .any(|p| Some(&p.id) == draft.project.as_ref())
                        {
                            draft.project = None;
                        }
                        s.load_info();
                    }
                    Err(e) => {
                        draft.load = Load::Failed;
                        s.fail("Couldn't load the projects", e);
                    }
                }
            },
        );
        self.api.call(
            "list_harnesses",
            json!({}),
            move |s, result: Result<Harnesses, String>, _| {
                let recent = s
                    .list
                    .chats
                    .as_ref()
                    .and_then(|c| c.first())
                    .and_then(|c| c.harness.clone());
                let Some(draft) = s.page_at(nav).and_then(|p| p.draft_mut()) else {
                    return;
                };
                match result {
                    Ok(got) => {
                        draft.harnesses = got
                            .harnesses
                            .into_iter()
                            .filter(|h| h.available)
                            .map(|h| h.id)
                            .collect();
                        if !draft
                            .harness
                            .as_ref()
                            .is_some_and(|h| draft.harnesses.contains(h))
                        {
                            draft.harness = recent
                                .filter(|h| draft.harnesses.contains(h))
                                .or_else(|| draft.harnesses.first().cloned());
                            draft.model = None;
                            draft.reasoning = None;
                        }
                        if draft.load == Load::Loading {
                            draft.load = Load::Ready;
                        }
                        if let Some(h) = draft.harness.clone() {
                            s.catalog(&h);
                        }
                    }
                    Err(e) => {
                        draft.load = Load::Failed;
                        s.fail("Couldn't load the agents", e);
                    }
                }
            },
        );
    }

    /// Create the chat with the first message, then show it in place of the draft.
    pub(super) fn create(&mut self, text: String) {
        let Some(page) = self.chat_mut() else { return };
        let nav = page.nav;
        let branch = page.info.as_ref().and_then(|i| i.branch.clone());
        let Some(draft) = page.draft_mut() else {
            return;
        };
        let Some(harness) = draft.harness.clone() else {
            return;
        };
        draft.creating = true;
        let mut args = json!({ "prompt": text, "harness": harness });
        for (key, value) in [
            ("project", &draft.project),
            ("model", &draft.model),
            ("reasoning", &draft.reasoning),
        ] {
            if let Some(value) = value {
                args[key] = json!(value);
            }
        }
        // The desktop records the project's branch on the chat it creates.
        if let Some(branch) = branch {
            args["branch"] = json!(branch);
        }
        let pick = NewChat {
            project: ProjectPick::from(draft.project.clone()),
            harness: Some(harness),
            model: draft.model.clone(),
            reasoning: draft.reasoning.clone(),
        };
        self.api.call(
            "create_chat",
            args,
            move |s, result: Result<Created, String>, _| {
                let Some(page) = s.page_at(nav) else { return };
                let made = match result {
                    Ok(made) => made,
                    Err(e) => {
                        if let Some(draft) = page.draft_mut() {
                            draft.creating = false;
                        }
                        s.fail("Couldn't start the chat", e);
                        return;
                    }
                };
                // The chat exists, so there's no text to keep for the next new chat.
                let typed = std::mem::take(&mut page.composer.text);
                // Also resets the caret, history, and marks.
                page.composer.clear();
                s.prefs.new_chat = pick;
                s.save_prefs();
                if let Some(why) = made.send_error {
                    // Open it with the message back in its composer.
                    s.drafts.insert(made.chat_id.clone(), typed);
                    s.open_chat(made.chat_id);
                    s.fail("Chat started, but the message wasn't sent", why);
                    return;
                }
                s.open_chat(made.chat_id);
                if let Some(chat) = s.chat_mut().and_then(|p| p.existing_mut()) {
                    chat.pending.push(text);
                }
                s.load_thumbs();
            },
        );
    }

    /// Load `harness`'s models and levels unless they are loaded or loading.
    pub fn catalog(&mut self, harness: &str) {
        if matches!(
            self.catalogs.get(harness),
            Some(Catalog::Loading | Catalog::Ready { .. })
        ) {
            return;
        }
        self.catalogs.insert(harness.to_owned(), Catalog::Loading);
        let key = harness.to_owned();
        let args = json!({ "harness": harness });
        let id = key.clone();
        self.api.run(
            move |tools| async move {
                let (models, harnesses) = tokio::join!(
                    tools.call::<Models>("list_models", args),
                    tools.call::<Harnesses>("list_harnesses", json!({})),
                );
                let levels = harnesses?
                    .harnesses
                    .into_iter()
                    .find(|h| h.id == id)
                    .map(|h| h.reasoning_levels)
                    .unwrap_or_default();
                Ok::<_, String>((models?.models, levels))
            },
            move |s, result, _| {
                let catalog = match result {
                    Ok((models, levels)) => Catalog::Ready { models, levels },
                    Err(e) => {
                        eprintln!("loading {key}'s models: {e}");
                        Catalog::Failed(e)
                    }
                };
                s.catalogs.insert(key, catalog);
                s.fit_draft();
            },
        );
    }

    /// The model, reasoning and harness the composer shows: a new chat's picks, else the chat's.
    pub fn config(&self) -> Config<'_> {
        let Some(page) = self.chat() else {
            return Config::default();
        };
        let chat = match &page.target {
            Target::New(d) => {
                return Config {
                    harness: d.harness.as_deref(),
                    model: d.model.as_deref(),
                    reasoning: d.reasoning.as_deref(),
                };
            }
            Target::Existing(chat) => chat.chat.as_ref(),
        };
        let chat_harness = chat.and_then(|c| c.harness.as_deref());
        match &page.info {
            Some(info) => Config {
                harness: info.harness.as_deref().or(chat_harness),
                model: info.model.as_deref(),
                reasoning: info.reasoning.as_deref(),
            },
            None => Config {
                harness: chat_harness,
                model: chat.and_then(|c| c.model.as_deref()),
                reasoning: chat.and_then(|c| c.reasoning.as_deref()),
            },
        }
    }

    /// Pick a model or reasoning for the next turns (and a new chat's agent).
    pub fn pick(&mut self, pick: Pick) {
        let Some(page) = self.chat_mut() else { return };
        let nav = page.nav;
        let id = match &page.target {
            Target::New(_) => return self.pick_draft(pick),
            Target::Existing(chat) => chat.id.clone(),
        };
        let args = match pick {
            // A created chat keeps its agent.
            Pick::Harness(_) => return,
            Pick::Model(model) => json!({ "chat": id, "model": model, "reasoning": null }),
            Pick::Reasoning(level) => json!({ "chat": id, "model": null, "reasoning": level }),
        };
        self.api.call(
            "set_chat_model",
            args,
            move |s, result: Result<Picked, String>, _| match result {
                Ok(saved) => {
                    if let Some(info) = s.page_at(nav).and_then(|p| p.info.as_mut()) {
                        info.model = saved.model;
                        info.reasoning = saved.reasoning;
                    }
                }
                Err(e) => s.fail("Couldn't change the model", e),
            },
        );
    }

    fn pick_draft(&mut self, pick: Pick) {
        let Some(draft) = self.chat_mut().and_then(|p| p.draft_mut()) else {
            return;
        };
        match pick {
            Pick::Harness(h) => {
                if draft.harness.as_ref() == Some(&h) {
                    return;
                }
                draft.harness = Some(h.clone());
                draft.model = None;
                draft.reasoning = None;
                self.catalog(&h);
                self.reload_skills();
            }
            Pick::Model(model) => draft.model = Some(model),
            Pick::Reasoning(level) => draft.reasoning = Some(level),
        }
        self.fit_draft();
    }

    /// Keep a new chat's model in its agent's catalog and its reasoning on the model's levels.
    fn fit_draft(&mut self) {
        let Page::Chat(page) = &mut self.page else {
            return;
        };
        let Some(draft) = page.draft_mut() else {
            return;
        };
        let Some((models, levels)) = draft
            .harness
            .as_ref()
            .and_then(|h| self.catalogs.get(h))
            .and_then(Catalog::ready)
        else {
            return;
        };
        if draft
            .model
            .as_ref()
            .is_some_and(|m| !models.iter().any(|x| &x.id == m))
        {
            draft.model = None;
        }
        let levels = levels_of(models, levels, draft.model.as_deref().unwrap_or_default());
        if draft
            .reasoning
            .as_ref()
            .is_some_and(|r| !levels.contains(r))
        {
            draft.reasoning = None;
        }
    }

    pub fn pick_project(&mut self, project: Option<String>) {
        let Some(page) = self.chat_mut() else { return };
        let Some(draft) = page.draft_mut() else {
            return;
        };
        if draft.project == project {
            return;
        }
        draft.project = project;
        page.info = None;
        self.load_info();
        self.reload_skills();
    }
}
