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
    pub loaded: bool,
    /// The first message is creating the chat.
    pub creating: bool,
    saved_project: ProjectPick,
}

/// A harness's models and reasoning levels.
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
            loaded: false,
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
        self.api.call(
            "list_projects",
            json!({}),
            move |s, result: Result<Projects, String>, _| {
                let recent = s.list.chats.as_ref().and_then(|c| c.first()).cloned();
                let Some(draft) = s.page_at(nav).and_then(|p| p.draft_mut()) else {
                    return;
                };
                match result {
                    Ok(mut got) => {
                        got.projects.sort_by(|a, b| a.name.cmp(&b.name));
                        draft.projects = got.projects;
                        if draft.saved_project == ProjectPick::Recent {
                            draft.project = recent.and_then(|c| c.project).map(|p| p.id);
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
                    Err(e) => s.toast("Couldn't load the projects", e, true),
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
                        draft.loaded = true;
                        if let Some(h) = draft.harness.clone() {
                            s.catalog(&h);
                        }
                    }
                    Err(e) => s.toast("Couldn't load the agents", e, true),
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
                        s.toast("Couldn't start the chat", e, true);
                        return;
                    }
                };
                // Sent, so there's no text to keep for the next new chat.
                page.composer.clear();
                s.prefs.new_chat = pick;
                s.prefs.save();
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
    pub fn config(&self) -> (Option<String>, Option<String>, Option<String>) {
        let Some(page) = self.chat() else {
            return (None, None, None);
        };
        let chat = match &page.target {
            Target::New(d) => return (d.harness.clone(), d.model.clone(), d.reasoning.clone()),
            Target::Existing(chat) => chat.chat.as_ref(),
        };
        match &page.info {
            Some(info) => (
                info.harness
                    .clone()
                    .or_else(|| chat.and_then(|c| c.harness.clone())),
                info.model.clone(),
                info.reasoning.clone(),
            ),
            None => (
                chat.and_then(|c| c.harness.clone()),
                chat.and_then(|c| c.model.clone()),
                chat.and_then(|c| c.reasoning.clone()),
            ),
        }
    }

    /// Pick a model or reasoning for the next turns (and a new chat's agent).
    pub fn pick(
        &mut self,
        harness: Option<String>,
        model: Option<String>,
        reasoning: Option<String>,
    ) {
        let Some(page) = self.chat_mut() else { return };
        let nav = page.nav;
        let id = match &page.target {
            Target::New(_) => return self.pick_draft(harness, model, reasoning),
            Target::Existing(chat) => chat.id.clone(),
        };
        let args = json!({ "chat": id, "model": model, "reasoning": reasoning });
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
                Err(e) => s.toast("Couldn't change the model", e, true),
            },
        );
    }

    fn pick_draft(
        &mut self,
        harness: Option<String>,
        model: Option<String>,
        reasoning: Option<String>,
    ) {
        let Some(draft) = self.chat_mut().and_then(|p| p.draft_mut()) else {
            return;
        };
        if let Some(h) = harness.filter(|h| Some(h) != draft.harness.as_ref()) {
            draft.harness = Some(h.clone());
            draft.model = None;
            draft.reasoning = None;
            self.catalog(&h);
            self.reload_skills();
        }
        let Some(draft) = self.chat_mut().and_then(|p| p.draft_mut()) else {
            return;
        };
        if model.is_some() {
            draft.model = model;
        }
        if reasoning.is_some() {
            draft.reasoning = reasoning;
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
