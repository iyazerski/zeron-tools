//! A chat's page: an existing chat, or a new one until its first message
//! creates it, under the composer they share.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use rune::{Image, TextEdit};

use super::Poll;
use super::attach::Attached;
use super::draft::Draft;
use crate::companion::data::{Chat, ComposerInfo, PendingInput, Read, Skill};

/// Messages per page, and how many more "Load earlier messages" adds.
const PAGE: usize = 40;
/// The composer's branch and context change slowly; re-read them at most this often.
const INFO_EVERY: Duration = Duration::from_secs(8);

pub struct ChatPage {
    pub nav: u64,
    pub target: Target,
    pub composer: TextEdit,
    pub info: Option<ComposerInfo>,
    pub menu: bool,
    pub sheet: Option<Sheet>,
    pub skills: SkillMenu,
    /// Images in the composer, saved as they're attached.
    pub attached: Vec<Attached>,
    pub far_from_bottom: bool,
    /// Edits aimed at a field that's gone (an answered question) land here.
    pub(super) scratch: TextEdit,
    /// The next attached image's id; ids are never reused, so a late upload
    /// can't land on a newer image.
    next_attached: u64,
}

/// What the page shows above the composer.
#[expect(
    clippy::large_enum_variant,
    reason = "the page is boxed whole, and is either one for its life"
)]
pub enum Target {
    /// A chat that the first message creates.
    New(Draft),
    Existing(Existing),
}

/// A created chat and its transcript.
pub struct Existing {
    pub id: String,
    pub chat: Option<Chat>,
    pub read: Option<Read>,
    pub error: Option<String>,
    /// Sent messages not in the transcript yet.
    pub pending: Vec<String>,
    pub open_groups: HashSet<String>,
    pub question: Option<QuestionForm>,
    /// The transcript's images by path, decoded once.
    pub thumbs: HashMap<String, Image>,
    pub(super) limit: usize,
    /// The transcript version shown; empty to load it whole.
    pub(super) version: String,
    pub(super) poll: Poll,
    /// Counts chat refreshes, so an older one that answers late is dropped.
    pub(super) refresh: u64,
    info_for: String,
    pub(super) info_at: Option<Instant>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sheet {
    Model,
    Project,
}

#[derive(Default)]
pub struct SkillMenu {
    pub list: Option<Result<Vec<Skill>, String>>,
    pub active: usize,
    /// Escape closed the menu for the `$` word at this byte.
    pub dismissed: Option<usize>,
    /// The scope the list was loaded for.
    pub(super) key: String,
}

/// The answers being given to the agent's questions.
pub struct QuestionForm {
    pub input: PendingInput,
    pub picked: Vec<BTreeSet<String>>,
    pub fields: Vec<TextEdit>,
    pub sending: bool,
}

impl ChatPage {
    pub(super) fn new(nav: u64, target: Target, composer: TextEdit) -> Self {
        Self {
            nav,
            target,
            composer,
            info: None,
            menu: false,
            sheet: None,
            skills: SkillMenu::default(),
            attached: Vec::new(),
            far_from_bottom: false,
            scratch: TextEdit::default(),
            next_attached: 0,
        }
    }

    pub fn existing(&self) -> Option<&Existing> {
        match &self.target {
            Target::Existing(chat) => Some(chat),
            Target::New(_) => None,
        }
    }

    pub fn existing_mut(&mut self) -> Option<&mut Existing> {
        match &mut self.target {
            Target::Existing(chat) => Some(chat),
            Target::New(_) => None,
        }
    }

    pub fn draft(&self) -> Option<&Draft> {
        match &self.target {
            Target::New(draft) => Some(draft),
            Target::Existing(_) => None,
        }
    }

    pub fn draft_mut(&mut self) -> Option<&mut Draft> {
        match &mut self.target {
            Target::New(draft) => Some(draft),
            Target::Existing(_) => None,
        }
    }

    /// Whether the chat's agent is running a turn.
    pub fn is_working(&self) -> bool {
        self.existing()
            .and_then(|c| c.chat.as_ref())
            .is_some_and(Chat::is_working)
    }

    /// A fresh id for an attached image.
    pub(super) fn attached_id(&mut self) -> u64 {
        self.next_attached += 1;
        self.next_attached
    }
}

impl Target {
    /// Where the page's unsent text is kept: the chat's id, or `new`.
    pub(super) fn key(&self) -> &str {
        match self {
            Self::New(_) => "new",
            Self::Existing(chat) => &chat.id,
        }
    }
}

impl Existing {
    pub(super) fn new(id: String, chat: Option<Chat>) -> Self {
        Self {
            id,
            chat,
            read: None,
            error: None,
            pending: Vec::new(),
            open_groups: HashSet::new(),
            question: None,
            thumbs: HashMap::new(),
            limit: PAGE,
            version: String::new(),
            poll: Poll::default(),
            refresh: 0,
            info_for: String::new(),
            info_at: None,
        }
    }

    /// Load the transcript whole at the next tick.
    pub(super) fn reload(&mut self) {
        self.version.clear();
        self.poll.expire();
    }

    /// Show `PAGE` more earlier messages.
    pub(super) fn load_earlier(&mut self) {
        self.limit += PAGE;
        self.reload();
    }

    /// Whether the composer's info may have changed with the transcript at
    /// `version`, and is due; notes `version` if so.
    pub(super) fn info_due(&mut self, version: &str) -> bool {
        let due =
            version != self.info_for && self.info_at.is_none_or(|t| t.elapsed() >= INFO_EVERY);
        if due {
            version.clone_into(&mut self.info_for);
        }
        due
    }
}

impl QuestionForm {
    pub(super) fn new(input: PendingInput) -> Self {
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
