//! The companion's state and what changes it: navigation, polling Zeron, and
//! the actions the phone app offers, with the same tool calls.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use rune::{Image, ImageCache, TextEdit, Tx};
use serde_json::{Value, json};
use zeron_relay_proto::Engine;

use super::api::Api;
use super::data::{
    Chat, ChatView, Chats, ComposerInfo, Created, Harnesses, Model, Models, PendingInput, Picked,
    Project, Projects, Read, Skill, Skills, levels_of, with_images, with_skill_refs,
};
use super::prefs::{NewChat, Prefs};

/// How often the list refreshes while shown, as on the phone.
const LIST_EVERY: Duration = Duration::from_secs(4);
/// How often an open chat refreshes: quickly while a turn runs.
const CHAT_WORKING_EVERY: Duration = Duration::from_millis(900);
const CHAT_IDLE_EVERY: Duration = Duration::from_millis(3500);
/// The composer's branch and context change slowly; re-read them at most this often.
const INFO_EVERY: Duration = Duration::from_secs(8);
/// Messages per page, and how many more "Load earlier messages" adds.
pub const PAGE: usize = 40;
const TOAST_FOR: Duration = Duration::from_millis(3800);

pub struct State {
    pub api: Api,
    pub engine: Engine,
    pub prefs: Prefs,
    pub page: Page,
    pub list: ListState,
    pub toast: Option<Toast>,
    /// Each harness's models and reasoning levels, loaded once.
    pub catalogs: HashMap<String, Catalog>,
    /// Decodes images off the UI thread; set once the app starts.
    pub images: Option<ImageCache>,
    /// Unsent text per chat (`new` for a new chat), kept while moving between chats.
    drafts: HashMap<String, String>,
    /// Counts page changes, so a late answer for a page that's gone is dropped.
    nav: u64,
}

pub enum Page {
    List,
    Chat(Box<ChatPage>),
}

#[derive(Default)]
pub struct ListState {
    pub chats: Option<Vec<Chat>>,
    pub query: TextEdit,
    pub error: Option<String>,
    loading: bool,
    last: Option<Instant>,
}

pub struct Toast {
    pub title: String,
    pub body: String,
    pub error: bool,
    until: Instant,
}

#[derive(Default)]
pub struct Catalog {
    pub models: Option<Vec<Model>>,
    pub levels: Option<Vec<String>>,
}

impl Catalog {
    pub fn ready(&self) -> Option<(&[Model], &[String])> {
        Some((self.models.as_deref()?, self.levels.as_deref()?))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sheet {
    Model,
    Project,
}

/// A chat, or with no `id` a new one: the same screen, empty until the first
/// message creates the chat.
pub struct ChatPage {
    pub id: Option<String>,
    pub nav: u64,
    pub chat: Option<Chat>,
    pub read: Option<Read>,
    pub limit: usize,
    pub error: Option<String>,
    /// Sent messages not in the transcript yet.
    pub pending: Vec<String>,
    pub composer: TextEdit,
    pub sending: bool,
    pub info: Option<ComposerInfo>,
    pub open_groups: HashSet<String>,
    pub question: Option<QuestionForm>,
    pub menu: bool,
    pub sheet: Option<Sheet>,
    pub skills: SkillMenu,
    pub draft: Option<Draft>,
    pub far_from_bottom: bool,
    /// Images in the composer, saved as they're attached.
    pub attached: Vec<super::attach::Attached>,
    /// The transcript's images by path, decoded once.
    pub thumbs: HashMap<String, Image>,
    /// Edits aimed at a field that's gone (an answered question) land here.
    scratch: TextEdit,
    version: String,
    loading: bool,
    last: Option<Instant>,
    info_for: String,
    info_at: Option<Instant>,
}

#[derive(Default)]
pub struct SkillMenu {
    pub list: Option<Result<Vec<Skill>, String>>,
    pub active: usize,
    /// Escape closed the menu for the `$` word at this byte.
    pub dismissed: Option<usize>,
    key: String,
}

/// The picks of a chat that isn't created yet.
pub struct Draft {
    pub projects: Vec<Project>,
    pub harnesses: Vec<String>,
    pub project: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub loaded: bool,
    saved_project: Option<Option<String>>,
}

/// The answers being given to the agent's questions.
pub struct QuestionForm {
    pub input: PendingInput,
    pub picked: Vec<BTreeSet<String>>,
    pub fields: Vec<TextEdit>,
    pub sending: bool,
}

impl State {
    pub fn new(api: Api, prefs: Prefs) -> Self {
        Self {
            api,
            engine: Engine::Closed,
            prefs,
            page: Page::List,
            list: ListState::default(),
            toast: None,
            catalogs: HashMap::new(),
            images: None,
            drafts: HashMap::new(),
            nav: 0,
        }
    }

    pub fn chat(&self) -> Option<&ChatPage> {
        match &self.page {
            Page::Chat(page) => Some(page),
            Page::List => None,
        }
    }

    pub fn chat_mut(&mut self) -> Option<&mut ChatPage> {
        match &mut self.page {
            Page::Chat(page) => Some(page),
            Page::List => None,
        }
    }

    /// The composer's text, for its field's edits.
    pub fn composer_field(&mut self) -> &mut TextEdit {
        match &mut self.page {
            Page::Chat(page) => &mut page.composer,
            Page::List => &mut self.list.query,
        }
    }

    /// The typed answer to question `index`, for its field's edits.
    pub fn answer_field(&mut self, index: usize) -> &mut TextEdit {
        match &mut self.page {
            Page::Chat(page) => match &mut page.question {
                Some(form) if index < form.fields.len() => &mut form.fields[index],
                _ => &mut page.scratch,
            },
            Page::List => &mut self.list.query,
        }
    }

    /// The page `nav` named, if it's still the one shown.
    pub fn page_at(&mut self, nav: u64) -> Option<&mut ChatPage> {
        self.chat_mut().filter(|p| p.nav == nav)
    }

    pub fn toast(&mut self, title: &str, body: impl Into<String>, error: bool) {
        self.toast = Some(Toast {
            title: title.to_owned(),
            body: body.into(),
            error,
            until: Instant::now() + TOAST_FOR,
        });
    }

    /// Show or hide the window; it opens where it was left.
    pub fn toggle_window(&mut self) {
        let app = self.api.app();
        if app.is_visible() {
            app.hide();
            return;
        }
        app.open();
        self.list.last = None;
        if let Some(page) = self.chat_mut() {
            page.last = None;
            page.composer.focused = true;
        }
    }

    pub fn engine_changed(&mut self, engine: Engine) {
        let opened = matches!(engine, Engine::Running { .. })
            && !matches!(self.engine, Engine::Running { .. });
        self.engine = engine;
        if opened {
            self.list.last = None;
            if let Some(page) = self.chat_mut() {
                page.last = None;
                if page.draft.as_ref().is_some_and(|d| !d.loaded) {
                    self.load_draft();
                }
            }
        }
    }

    /// Start what's due and expire the toast; true when the view must redraw.
    pub fn tick(&mut self, _: &mut Tx) -> bool {
        let mut changed = false;
        if self
            .toast
            .as_ref()
            .is_some_and(|t| Instant::now() >= t.until)
        {
            self.toast = None;
            changed = true;
        }
        if !self.api.app().is_visible() || !matches!(self.engine, Engine::Running { .. }) {
            return changed;
        }
        let due =
            |last: Option<Instant>, every: Duration| last.is_none_or(|t| t.elapsed() >= every);
        match &self.page {
            Page::List => {
                if !self.list.loading && due(self.list.last, LIST_EVERY) {
                    self.load_chats();
                }
            }
            Page::Chat(page) => {
                let working = page.chat.as_ref().is_some_and(Chat::is_working);
                let every = if working {
                    CHAT_WORKING_EVERY
                } else {
                    CHAT_IDLE_EVERY
                };
                if page.id.is_some() && !page.loading && due(page.last, every) {
                    self.refresh_chat();
                }
                // The working clock counts seconds.
                changed |= working;
            }
        }
        changed
    }

    fn load_chats(&mut self) {
        self.list.loading = true;
        self.api.call(
            "list_chats",
            json!({ "limit": 200 }),
            |s, result: Result<Chats, String>, _| {
                s.list.loading = false;
                s.list.last = Some(Instant::now());
                match result {
                    Ok(mut got) => {
                        got.chats.sort_by_key(|c| std::cmp::Reverse(c.active_at()));
                        s.prefs.prime(&got.chats);
                        s.list.chats = Some(got.chats);
                        s.list.error = None;
                    }
                    Err(e) => s.list.error = Some(e),
                }
            },
        );
    }

    fn page(&mut self, id: Option<String>) -> ChatPage {
        self.nav += 1;
        let key = id.clone().unwrap_or_else(|| "new".to_owned());
        let mut composer = TextEdit::new(self.drafts.remove(&key).unwrap_or_default());
        composer.set_caret(composer.text.len());
        composer.focused = true;
        ChatPage {
            chat: id.as_ref().and_then(|id| {
                self.list
                    .chats
                    .as_ref()?
                    .iter()
                    .find(|c| &c.id == id)
                    .cloned()
            }),
            id,
            nav: self.nav,
            read: None,
            limit: PAGE,
            error: None,
            pending: Vec::new(),
            composer,
            sending: false,
            info: None,
            open_groups: HashSet::new(),
            question: None,
            menu: false,
            sheet: None,
            skills: SkillMenu::default(),
            draft: None,
            attached: Vec::new(),
            thumbs: HashMap::new(),
            far_from_bottom: false,
            scratch: TextEdit::default(),
            version: String::new(),
            loading: false,
            last: None,
            info_for: String::new(),
            info_at: None,
        }
    }

    pub fn open_chat(&mut self, id: String) {
        self.leave_page();
        let page = self.page(Some(id));
        self.page = Page::Chat(Box::new(page));
    }

    pub fn new_chat(&mut self) {
        self.leave_page();
        let mut page = self.page(None);
        let NewChat {
            project,
            harness,
            model,
            reasoning,
        } = self.prefs.new_chat.clone();
        page.draft = Some(Draft {
            projects: Vec::new(),
            harnesses: Vec::new(),
            project: project.clone().flatten(),
            harness,
            model,
            reasoning,
            loaded: false,
            saved_project: project,
        });
        self.page = Page::Chat(Box::new(page));
        self.load_draft();
    }

    pub fn back(&mut self) {
        self.leave_page();
        self.page = Page::List;
        self.list.last = None;
    }

    /// Keep the page's unsent text for when it opens again.
    fn leave_page(&mut self) {
        if let Page::Chat(page) = &self.page {
            let key = page.id.clone().unwrap_or_else(|| "new".to_owned());
            if page.composer.text.trim().is_empty() {
                self.drafts.remove(&key);
            } else {
                self.drafts.insert(key, page.composer.text.clone());
            }
        }
    }

    /// One call per poll: the chat, and its messages only when they may have
    /// changed since the version shown.
    pub fn refresh_chat(&mut self) {
        let Some(page) = self.chat_mut() else { return };
        let Some(id) = page.id.clone() else { return };
        page.loading = true;
        let (nav, limit, known) = (page.nav, page.limit, page.version.clone());
        let args = json!({ "chat": id, "limit": limit, "known": known });
        self.api.call(
            "chat_view",
            args,
            move |s, result: Result<ChatView, String>, tx| {
                let Some(page) = s.page_at(nav) else { return };
                page.loading = false;
                page.last = Some(Instant::now());
                let view = match result {
                    Ok(view) => view,
                    Err(e) => {
                        page.error = Some(e);
                        return;
                    }
                };
                page.error = None;
                let refresh_info = view.version != page.info_for
                    && page.info_at.is_none_or(|t| t.elapsed() >= INFO_EVERY);
                if refresh_info {
                    page.info_for.clone_from(&view.version);
                }
                if let Some(read) = view.read {
                    page.pending.retain(|text| {
                        !read.messages.iter().any(|m| {
                            m.role == super::data::Role::User
                                && super::data::split_from(&m.text()).1.trim() == text
                        })
                    });
                    let asked = read.pending_input.clone();
                    if page.question.as_ref().map(|q| &q.input.request_id)
                        != asked.as_ref().map(|i| &i.request_id)
                    {
                        page.question = asked.map(QuestionForm::new);
                        if page.question.is_some() {
                            tx.scroll_to_bottom("transcript");
                        }
                    }
                    page.read = Some(read);
                    page.version = view.version;
                    s.load_thumbs();
                }
                s.prefs.mark_seen(&view.chat);
                if let Some(page) = s.page_at(nav) {
                    page.chat = Some(view.chat);
                }
                if refresh_info {
                    s.load_info();
                }
            },
        );
    }

    pub fn load_earlier(&mut self) {
        if let Some(page) = self.chat_mut() {
            page.limit += PAGE;
            page.version.clear();
            page.last = None;
        }
        self.refresh_chat();
    }

    /// The composer's branch, context and model: the chat's, or a new chat's in its project.
    pub fn load_info(&mut self) {
        let Some(page) = self.chat_mut() else { return };
        page.info_at = Some(Instant::now());
        let nav = page.nav;
        let (args, project) = match (&page.id, &page.draft) {
            (Some(id), _) => (json!({ "chat": id }), None),
            (None, Some(draft)) => (
                draft
                    .project
                    .as_ref()
                    .map_or_else(|| json!({}), |p| json!({ "project": p })),
                Some(draft.project.clone()),
            ),
            (None, None) => return,
        };
        self.api.call(
            "composer_info",
            args,
            move |s, result: Result<ComposerInfo, String>, _| {
                let Some(page) = s.page_at(nav) else { return };
                // A new chat's project may have changed while this loaded.
                if project.is_some() && project != page.draft.as_ref().map(|d| d.project.clone()) {
                    return;
                }
                match result {
                    Ok(info) => {
                        let harness = info.harness.clone();
                        page.info = Some(info);
                        if let Some(harness) = harness {
                            s.catalog(&harness);
                        }
                    }
                    Err(e) => eprintln!("composer_info: {e}"),
                }
            },
        );
    }

    /// Load `harness`'s models and levels unless they are loaded or loading.
    pub fn catalog(&mut self, harness: &str) {
        if self.catalogs.contains_key(harness) {
            return;
        }
        self.catalogs.insert(harness.to_owned(), Catalog::default());
        let key = harness.to_owned();
        self.api.call(
            "list_models",
            json!({ "harness": harness }),
            move |s, result: Result<Models, String>, _| match result {
                Ok(got) => s.catalogs.entry(key).or_default().models = Some(got.models),
                Err(e) => {
                    s.catalogs.remove(&key);
                    eprintln!("list_models: {e}");
                }
            },
        );
        let key = harness.to_owned();
        self.api.call(
            "list_harnesses",
            json!({}),
            move |s, result: Result<Harnesses, String>, _| {
                let levels = result
                    .ok()
                    .and_then(|h| h.harnesses.into_iter().find(|h| h.id == key));
                s.catalogs.entry(key).or_default().levels =
                    Some(levels.map(|h| h.reasoning_levels).unwrap_or_default());
            },
        );
    }

    /// Load a new chat's projects and agents, keeping valid picks.
    fn load_draft(&mut self) {
        let Some(page) = self.chat_mut() else { return };
        let nav = page.nav;
        self.api.call(
            "list_projects",
            json!({}),
            move |s, result: Result<Projects, String>, _| {
                let recent = s.list.chats.as_ref().and_then(|c| c.first()).cloned();
                let Some(draft) = s.page_at(nav).and_then(|p| p.draft.as_mut()) else {
                    return;
                };
                match result {
                    Ok(mut got) => {
                        got.projects.sort_by(|a, b| a.name.cmp(&b.name));
                        draft.projects = got.projects;
                        if draft.saved_project.is_none() {
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
                let Some(draft) = s.page_at(nav).and_then(|p| p.draft.as_mut()) else {
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

    /// Send the composer's text: to the chat, or as a new chat's first message.
    pub fn send(&mut self, tx: &mut Tx) {
        let Some(page) = self.chat_mut() else { return };
        let typed = page.composer.text.trim().to_owned();
        if typed.is_empty() && page.attached.is_empty() || page.sending {
            return;
        }
        let paths = match super::attach::ready(page) {
            Ok(paths) => paths,
            Err(why) => return self.toast(why, "", true),
        };
        let skills = match &page.skills.list {
            Some(Ok(skills)) => skills.as_slice(),
            _ => &[],
        };
        let text = with_images(&with_skill_refs(&typed, skills), &paths);
        if page.id.is_none() {
            self.create(text);
            return;
        }
        let Some(id) = page.id.clone() else { return };
        let nav = page.nav;
        page.composer.clear();
        page.attached.clear();
        page.pending.push(text.clone());
        self.load_thumbs();
        tx.scroll_to_bottom("transcript");
        let args = json!({ "chat": id, "text": text });
        self.api.call(
            "send_message",
            args,
            move |s, result: Result<Value, String>, _| {
                if let Err(e) = result {
                    if let Some(page) = s.page_at(nav) {
                        page.pending.retain(|p| p != &text);
                        if page.composer.text.trim().is_empty() && page.attached.is_empty() {
                            page.composer = TextEdit::new(typed);
                            page.composer.set_caret(page.composer.text.len());
                            s.reattach(nav, paths);
                        }
                    }
                    s.toast("Not sent", e, true);
                } else if let Some(page) = s.page_at(nav) {
                    page.version.clear();
                    page.last = None;
                }
            },
        );
    }

    /// Create the chat with the first message, then show it in place of the draft.
    fn create(&mut self, text: String) {
        let Some(page) = self.chat_mut() else { return };
        let Some(draft) = &page.draft else { return };
        let Some(harness) = draft.harness.clone() else {
            return;
        };
        page.sending = true;
        let nav = page.nav;
        let mut args = json!({ "prompt": text, "harness": harness });
        let pick = NewChat {
            project: Some(draft.project.clone()),
            harness: Some(harness),
            model: draft.model.clone(),
            reasoning: draft.reasoning.clone(),
        };
        for (key, value) in [
            ("project", &pick.project.clone().flatten()),
            ("model", &pick.model),
            ("reasoning", &pick.reasoning),
        ] {
            if let Some(value) = value {
                args[key] = json!(value);
            }
        }
        // The desktop records the project's branch on the chat it creates.
        if let Some(branch) = page.info.as_ref().and_then(|i| i.branch.clone()) {
            args["branch"] = json!(branch);
        }
        self.api.call(
            "create_chat",
            args,
            move |s, result: Result<Created, String>, _| {
                let Some(page) = s.page_at(nav) else { return };
                page.sending = false;
                match result {
                    Ok(made) => {
                        s.prefs.new_chat = pick;
                        s.prefs.save();
                        s.drafts.remove("new");
                        s.page = Page::List;
                        s.open_chat(made.chat_id);
                        if let Some(page) = s.chat_mut() {
                            page.composer.clear();
                            page.pending.push(text);
                        }
                        s.load_thumbs();
                    }
                    Err(e) => s.toast("Couldn't start the chat", e, true),
                }
            },
        );
    }

    /// Call a tool on the open chat, and refresh it after.
    fn chat_action(&mut self, tool: &'static str, extra: Value, failed: &'static str) {
        let Some(page) = self.chat_mut() else { return };
        let Some(id) = page.id.clone() else { return };
        let nav = page.nav;
        let mut args = json!({ "chat": id });
        if let (Value::Object(args), Value::Object(extra)) = (&mut args, extra) {
            args.extend(extra);
        }
        self.api.call(
            tool,
            args,
            move |s, result: Result<Value, String>, _| match result {
                Ok(_) => {
                    if tool == "archive_chat" {
                        s.toast("Chat archived", "", false);
                        if s.page_at(nav).is_some() {
                            s.back();
                        }
                    } else if let Some(page) = s.page_at(nav) {
                        page.version.clear();
                        page.last = None;
                        if let Some(q) = &mut page.question {
                            q.sending = false;
                        }
                    }
                }
                Err(e) => {
                    if let Some(q) = s.page_at(nav).and_then(|p| p.question.as_mut()) {
                        q.sending = false;
                    }
                    s.toast(failed, e, true);
                }
            },
        );
    }

    pub fn stop(&mut self) {
        self.chat_action("interrupt_chat", json!({}), "Couldn't stop");
    }

    pub fn archive(&mut self) {
        self.chat_action("archive_chat", json!({}), "Couldn't archive");
    }

    /// Answer the agent's questions with the picked options and typed text.
    pub fn respond(&mut self) {
        let Some(form) = self.chat_mut().and_then(|p| p.question.as_mut()) else {
            return;
        };
        let answers: Vec<Value> = form
            .input
            .questions
            .iter()
            .zip(form.picked.iter().zip(&form.fields))
            .map(|(q, (picked, field))| {
                let mut labels: Vec<&str> = picked.iter().map(String::as_str).collect();
                let typed = field.text.trim();
                if !typed.is_empty() {
                    labels.push(typed);
                }
                json!({ "question_id": q.id, "labels": labels })
            })
            .collect();
        if answers
            .iter()
            .any(|a| a["labels"].as_array().is_none_or(Vec::is_empty))
        {
            self.toast("Answer every question first", "", false);
            return;
        }
        form.sending = true;
        let request = form.input.request_id.clone();
        let extra = json!({ "request_id": request, "answers": answers });
        self.chat_action("respond_to_input", extra, "Not sent");
    }

    pub fn open_zeron(&mut self) {
        self.api.call(
            "open_zeron",
            json!({}),
            |s, result: Result<Value, String>, _| {
                if let Err(e) = result {
                    s.toast("Couldn't open Zeron", e, true);
                }
            },
        );
    }

    pub fn restart_zeron(&mut self) {
        self.api.call(
            "restart_zeron",
            json!({}),
            |s, result: Result<Value, String>, _| match result {
                Ok(_) => s.toast("Restarting Zeron", "", false),
                Err(e) => s.toast("Couldn't restart Zeron", e, true),
            },
        );
    }

    /// The model, reasoning and harness the composer shows: a new chat's picks, else the chat's.
    pub fn config(&self) -> (Option<String>, Option<String>, Option<String>) {
        let Some(page) = self.chat() else {
            return (None, None, None);
        };
        if let Some(d) = &page.draft {
            return (d.harness.clone(), d.model.clone(), d.reasoning.clone());
        }
        let chat = page.chat.as_ref();
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
        if let Some(draft) = &mut page.draft {
            if let Some(h) = harness.filter(|h| Some(h) != draft.harness.as_ref()) {
                draft.harness = Some(h.clone());
                draft.model = None;
                draft.reasoning = None;
                self.catalog(&h);
            }
            let Some(draft) = self.chat_mut().and_then(|p| p.draft.as_mut()) else {
                return;
            };
            if model.is_some() {
                draft.model = model;
            }
            if reasoning.is_some() {
                draft.reasoning = reasoning;
            }
            self.fit_draft();
            return;
        }
        let Some(id) = page.id.clone() else { return };
        let nav = page.nav;
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

    /// Keep a new chat's model in its agent's catalog and its reasoning on the model's levels.
    pub fn fit_draft(&mut self) {
        let Some(Page::Chat(page)) = Some(&mut self.page) else {
            return;
        };
        let Some(draft) = &mut page.draft else { return };
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
        let Some(draft) = self.chat_mut().and_then(|p| p.draft.as_mut()) else {
            return;
        };
        if draft.project != project {
            draft.project = project;
            if let Some(page) = self.chat_mut() {
                page.info = None;
            }
            self.load_info();
        }
    }

    /// Load the skills for the `$` menu: the chat's agent's, or a new chat's in its project.
    pub fn load_skills(&mut self) {
        let Some(page) = self.chat_mut() else { return };
        let args = match (&page.id, &page.draft) {
            (Some(id), _) => json!({ "chat": id }),
            (None, Some(d)) => match &d.harness {
                Some(h) => json!({ "harness": h, "project": d.project }),
                None => return,
            },
            (None, None) => return,
        };
        let key = args.to_string();
        if page.skills.key == key {
            return;
        }
        page.skills.key = key;
        page.skills.list = None;
        let nav = page.nav;
        self.api.call(
            "list_skills",
            args,
            move |s, result: Result<Skills, String>, _| {
                if let Some(page) = s.page_at(nav) {
                    page.skills.list = Some(result.map(|r| r.skills));
                }
            },
        );
    }
}

impl QuestionForm {
    fn new(input: PendingInput) -> Self {
        let fields = input
            .questions
            .iter()
            .map(|q| TextEdit::new(q.prefill.clone().unwrap_or_default()))
            .collect();
        Self {
            picked: vec![BTreeSet::new(); input.questions.len()],
            fields,
            input,
            sending: false,
        }
    }
}
