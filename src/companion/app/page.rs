//! A chat's page: an existing chat, or a new one until its first message
//! creates it, under the composer they share.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use rune::{Image, TextEdit};
use rune_kit::diff::DiffState;

use super::Poll;
use super::attach::Attached;
use super::draft::Draft;
use crate::companion::data::{Chat, ComposerInfo, Part, PendingInput, Read, Role, Skill};
use crate::companion::dictation::Listening;

/// Messages per page, and how many more "Load earlier messages" adds.
const PAGE: usize = 40;
/// The composer's branch and context change slowly; re-read them at most this often.
const INFO_EVERY: Duration = Duration::from_secs(8);
/// How long a reply's copy button shows a check after a copy.
const COPIED_FOR: Duration = Duration::from_secs(2);

pub struct ChatPage {
    pub nav: u64,
    pub target: Target,
    pub composer: TextEdit,
    pub info: Option<ComposerInfo>,
    pub overlay: Overlay,
    pub skills: SkillMenu,
    /// Images in the composer, saved as they're attached.
    pub attached: Vec<Attached>,
    pub far_from_bottom: bool,
    pub voice: Voice,
    /// Edits aimed at a field that's gone (an answered question) land here.
    pub(super) scratch: TextEdit,
    /// The next attached image's id; ids are never reused, so a late upload
    /// can't land on a newer image.
    next_attached: u64,
    /// The next dictation's id, so a session that ended can't write.
    pub(super) next_dictation: u64,
}

/// How many recent loudness readings the waveform shows.
pub const LEVELS_KEPT: usize = 60;

/// Where dictation into the composer is.
pub enum Voice {
    Idle,
    /// Asking for the microphone and speech recognition.
    Authorizing,
    Listening(Dictating),
    /// What dictation heard is being cleaned up.
    Polishing,
}

impl Voice {
    pub fn listening(&self) -> Option<&Dictating> {
        match self {
            Self::Listening(d) => Some(d),
            Self::Idle | Self::Authorizing | Self::Polishing => None,
        }
    }
}

pub struct Dictating {
    pub id: u64,
    pub session: Listening,
    pub started: Instant,
    /// What's heard so far, shown only once cleaned up.
    pub heard: String,
    /// Recent loudness, 0 to 1, oldest first.
    pub levels: VecDeque<f32>,
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
    pub thumbs: HashMap<String, Option<Image>>,
    pub file_links: FileLinks,
    pub(super) limit: usize,
    /// The transcript version shown; `None` to load it whole.
    pub(super) version: Option<String>,
    pub(super) poll: Poll,
    /// Counts chat refreshes, so an older one that answers late is dropped.
    pub(super) refresh: u64,
    info_for: Option<String>,
    pub(super) info_at: Option<Instant>,
    /// The message whose answer was just copied, and when.
    pub(super) copied: Option<(String, Instant)>,
    /// The latest turn's diff, shown in place of the transcript while open.
    pub changes: Option<Changes>,
    /// The turn the changes page shows: the user message that started it.
    pub changes_of: String,
}

/// The file each inline code names, by its text, looked up once per transcript
/// version. The view fills it, as Rune's Markdown asks for links while it draws.
pub type FileLinks = Rc<RefCell<HashMap<String, Option<String>>>>;

pub enum Changes {
    Loading,
    Ready {
        diff: Box<DiffState>,
        /// Zeron capped the patch, so files or lines are missing.
        truncated: bool,
    },
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Closed,
    Menu,
    Sheet(Sheet),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sheet {
    Model,
    Project,
}

#[derive(Default)]
pub struct SkillMenu {
    pub list: SkillList,
    pub active: usize,
    /// Escape closed the menu for the `$` word at this byte.
    pub dismissed: Option<usize>,
    /// The scope the list was loaded for.
    pub(super) key: String,
}

/// The skills of the page's agent, for the `$` menu.
#[derive(Default)]
pub enum SkillList {
    #[default]
    Loading,
    Ready(Vec<Skill>),
    Failed(String),
}

impl SkillList {
    pub fn ready(&self) -> &[Skill] {
        match self {
            Self::Ready(skills) => skills,
            Self::Loading | Self::Failed(_) => &[],
        }
    }
}

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
            overlay: Overlay::Closed,
            skills: SkillMenu::default(),
            attached: Vec::new(),
            far_from_bottom: false,
            voice: Voice::Idle,
            scratch: TextEdit::default(),
            next_attached: 0,
            next_dictation: 0,
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

    pub fn is_working(&self) -> bool {
        self.existing()
            .and_then(|c| c.chat.as_ref())
            .is_some_and(Chat::is_working)
    }

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
    /// Whether message `id`'s answer was copied a moment ago.
    pub fn just_copied(&self, id: &str) -> bool {
        self.copied
            .as_ref()
            .is_some_and(|(m, at)| m == id && at.elapsed() < COPIED_FOR)
    }

    /// Forget a copy shown long enough; true when one was.
    pub(super) fn expire_copied(&mut self) -> bool {
        let expired = self
            .copied
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= COPIED_FOR);
        if expired {
            self.copied = None;
        }
        expired
    }

    /// Whether the agent works but shows nothing yet (no prose and no
    /// running tool), so the transcript counts the seconds instead.
    pub fn shows_clock(&self) -> bool {
        let working = self.chat.as_ref().is_some_and(Chat::is_working);
        working
            && self.read.as_ref().is_some_and(|r| {
                r.messages.last().is_none_or(|m| {
                    m.role == Role::User
                        || !m.parts.iter().any(|p| match p {
                            Part::Text { .. } => true,
                            Part::Tools { tools } => tools.iter().any(|t| t.ends_with("(running)")),
                            Part::Error { .. } => false,
                        })
                })
            })
    }

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
            file_links: FileLinks::default(),
            limit: PAGE,
            version: None,
            poll: Poll::default(),
            refresh: 0,
            info_for: None,
            info_at: None,
            copied: None,
            changes: None,
            changes_of: String::new(),
        }
    }

    /// Load the transcript whole at the next tick.
    pub(super) fn reload(&mut self) {
        self.version = None;
        self.poll.expire();
    }

    pub(super) fn load_earlier(&mut self) {
        self.limit += PAGE;
        self.reload();
    }

    /// Whether the composer's info may have changed with the transcript at
    /// `version`, and is due; notes `version` if so.
    pub(super) fn info_due(&mut self, version: &str) -> bool {
        let due = self.info_for.as_deref() != Some(version)
            && self.info_at.is_none_or(|t| t.elapsed() >= INFO_EVERY);
        if due {
            self.info_for = Some(version.to_owned());
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
