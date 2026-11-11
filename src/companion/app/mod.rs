//! The companion's state and what changes it: navigation and polling here, a
//! chat's actions in [`actions`], a new chat's picks in [`draft`], and the
//! composer's images in [`attach`].

mod actions;
mod attach;
mod draft;
mod page;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rune::{ImageCache, TextEdit, Tx};
use serde_json::json;
use zeron_relay_proto::Engine;

use super::api::Api;
use super::data::{Chat, Chats};
use super::prefs::Prefs;

pub use self::attach::Upload;
pub use self::draft::{Catalog, Config, Draft, Load, Pick};
pub use self::page::{
    ChatPage, Existing, LEVELS_KEPT, Overlay, QuestionForm, Sheet, SkillList, Target, Voice,
};

/// How often the list refreshes while shown, as on the phone.
const LIST_EVERY: Duration = Duration::from_secs(4);
/// How often an open chat refreshes: quickly while a turn runs.
const CHAT_WORKING_EVERY: Duration = Duration::from_millis(900);
const CHAT_IDLE_EVERY: Duration = Duration::from_millis(3500);
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
    pub filter: ListFilter,
    pub error: Option<String>,
    poll: Poll,
}

/// Which chats the list shows, by status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListFilter {
    #[default]
    All,
    Working,
    /// Finished since they were last opened: the ones marked Done.
    Done,
    /// Active in the last `RECENT`.
    Recent,
}

impl ListFilter {
    /// The filters in the order the list offers them.
    pub const ALL: [Self; 4] = [Self::All, Self::Working, Self::Done, Self::Recent];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Working => "Working",
            Self::Done => "Done",
            Self::Recent => "Recent",
        }
    }
}

pub struct Toast {
    pub title: String,
    pub body: String,
    pub kind: ToastKind,
    until: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Note,
    Failure,
}

/// When some data last loaded, and whether a load runs now.
#[derive(Default)]
struct Poll {
    loading: bool,
    last: Option<Instant>,
}

impl Poll {
    /// Whether to load again: nothing runs, and the last load is `every` old.
    fn due(&self, every: Duration) -> bool {
        !self.loading && self.last.is_none_or(|t| t.elapsed() >= every)
    }

    fn start(&mut self) {
        self.loading = true;
    }

    fn done(&mut self) {
        self.loading = false;
        self.last = Some(Instant::now());
    }

    /// Load again at the next tick.
    fn expire(&mut self) {
        self.last = None;
    }
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
            Page::Chat(page) => match &mut page.target {
                Target::Existing(Existing {
                    question: Some(form),
                    ..
                }) if index < form.fields.len() => &mut form.fields[index],
                _ => &mut page.scratch,
            },
            Page::List => &mut self.list.query,
        }
    }

    /// The page `nav` named, if it's still the one shown.
    pub fn page_at(&mut self, nav: u64) -> Option<&mut ChatPage> {
        self.chat_mut().filter(|p| p.nav == nav)
    }

    /// The existing chat `nav` named, if it's still the one shown.
    fn existing_at(&mut self, nav: u64) -> Option<&mut Existing> {
        self.page_at(nav)?.existing_mut()
    }

    pub fn save_prefs(&self) {
        self.api.save_prefs(&self.prefs);
    }

    /// Show a short note.
    pub fn notify(&mut self, title: &str) {
        self.show_toast(ToastKind::Note, title, String::new());
    }

    /// Show what failed, and why.
    pub fn fail(&mut self, title: &str, why: impl Into<String>) {
        self.show_toast(ToastKind::Failure, title, why.into());
    }

    fn show_toast(&mut self, kind: ToastKind, title: &str, body: String) {
        self.toast = Some(Toast {
            title: title.to_owned(),
            body,
            kind,
            until: Instant::now() + TOAST_FOR,
        });
    }

    /// Show or hide the window; it opens where it was left.
    pub fn toggle_window(&mut self) {
        if self.api.app().is_visible() {
            return self.hide();
        }
        self.api.app().open();
        self.list.poll.expire();
        if let Some(page) = self.chat_mut() {
            page.composer.focused = true;
            if let Some(chat) = page.existing_mut() {
                chat.poll.expire();
            }
        }
        self.load_due();
    }

    /// Hide the window, and stop listening for dictation.
    pub fn hide(&mut self) {
        self.stop_dictating();
        self.api.app().hide();
    }

    pub fn engine_changed(&mut self, engine: Engine) {
        let opened = matches!(engine, Engine::Running { .. })
            && !matches!(self.engine, Engine::Running { .. });
        self.engine = engine;
        if !opened {
            return;
        }
        self.list.poll.expire();
        if let Some(page) = self.chat_mut() {
            match &mut page.target {
                Target::Existing(chat) => chat.poll.expire(),
                Target::New(draft) if draft.load == Load::Failed => self.load_draft(),
                Target::New(_) => {}
            }
        }
        if self.api.app().is_visible() {
            self.load_due();
        }
    }

    /// Start what's due and expire the toast and copy checks; true when the view must redraw.
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
        if let Some(chat) = self.chat_mut().and_then(|p| p.existing_mut()) {
            changed |= chat.expire_copied();
        }
        if !self.api.app().is_visible() || !matches!(self.engine, Engine::Running { .. }) {
            return changed;
        }
        // The working clock counts seconds; nothing else moves.
        changed |= self
            .chat()
            .and_then(ChatPage::existing)
            .is_some_and(Existing::shows_clock);
        self.load_due();
        changed
    }

    /// Start the shown page's load if it's due. Navigation calls this at once,
    /// so a page it opens or expires doesn't wait for the next tick.
    fn load_due(&mut self) {
        if !matches!(self.engine, Engine::Running { .. }) {
            return;
        }
        match &self.page {
            Page::List => {
                if self.list.poll.due(LIST_EVERY) {
                    self.load_chats();
                }
            }
            Page::Chat(page) => {
                let Some(chat) = page.existing() else { return };
                let every = if page.is_working() {
                    CHAT_WORKING_EVERY
                } else {
                    CHAT_IDLE_EVERY
                };
                if chat.poll.due(every) {
                    self.refresh_chat();
                }
            }
        }
    }

    /// Load the transcript of the chat `nav` named whole, now.
    fn reload_at(&mut self, nav: u64) {
        if let Some(chat) = self.existing_at(nav) {
            chat.reload();
            self.load_due();
        }
    }

    fn load_chats(&mut self) {
        self.list.poll.start();
        self.api.call(
            "list_chats",
            json!({ "limit": 200 }),
            |s, result: Result<Chats, String>, _| {
                s.list.poll.done();
                match result {
                    Ok(mut got) => {
                        got.chats.sort_by_key(|c| std::cmp::Reverse(c.active_at()));
                        if s.prefs.prime(&got.chats) {
                            s.save_prefs();
                        }
                        s.list.chats = Some(got.chats);
                        s.list.error = None;
                    }
                    Err(e) => s.list.error = Some(e),
                }
            },
        );
    }

    pub fn open_chat(&mut self, id: String) {
        let chat = self
            .list
            .chats
            .iter()
            .flatten()
            .find(|c| c.id == id)
            .cloned();
        self.show(Target::Existing(Existing::new(id, chat)));
        self.load_due();
    }

    pub fn new_chat(&mut self) {
        self.show(Target::New(Draft::new(self.prefs.new_chat.clone())));
        self.load_draft();
    }

    pub fn back(&mut self) {
        self.leave_page();
        self.list.poll.expire();
        self.load_due();
    }

    /// Show `target`'s page, with the text left unsent there before.
    fn show(&mut self, target: Target) {
        self.leave_page();
        self.nav += 1;
        let mut composer = TextEdit::new(self.drafts.remove(target.key()).unwrap_or_default());
        composer.set_caret(composer.text.len());
        composer.focused = true;
        self.page = Page::Chat(Box::new(ChatPage::new(self.nav, target, composer)));
    }

    /// Close the open page for the list, keeping its unsent text for when it
    /// opens again.
    fn leave_page(&mut self) {
        let Page::Chat(mut page) = std::mem::replace(&mut self.page, Page::List) else {
            return;
        };
        let key = page.target.key();
        if page.composer.text.trim().is_empty() {
            self.drafts.remove(key);
        } else {
            let text = std::mem::take(&mut page.composer.text);
            self.drafts.insert(key.to_owned(), text);
        }
    }
}
