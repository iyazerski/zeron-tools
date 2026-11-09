//! What a chat's page does, with the phone app's tool calls: refresh the
//! transcript, send, answer the agent, stop, archive, dictate, and load what
//! the composer shows.

use rune::{TextEdit, Tx};
use serde_json::{Value, json};

use super::page::{SkillList, SkillMenu};
use super::{QuestionForm, State, Target, attach};
use crate::companion::data::{ChatView, ComposerInfo, Role, Skills};
use crate::companion::dictation;
use crate::companion::message::{split_from, with_images, with_skill_refs};

/// A call on the open chat that refreshes it after.
enum ChatAction {
    Stop,
    Archive,
    /// Answer the agent's questions.
    Respond {
        request_id: String,
        answers: Vec<Value>,
    },
}

impl State {
    /// One call per poll: the chat, and its messages only when they may have
    /// changed since the version shown.
    pub fn refresh_chat(&mut self) {
        let Some(page) = self.chat_mut() else { return };
        let nav = page.nav;
        let Some(chat) = page.existing_mut() else {
            return;
        };
        chat.poll.start();
        chat.refresh += 1;
        let seq = chat.refresh;
        let args = json!({ "chat": chat.id, "limit": chat.limit, "known": chat.version });
        self.api.call(
            "chat_view",
            args,
            move |s, result: Result<ChatView, String>, tx| {
                let Some(chat) = s.existing_at(nav).filter(|c| c.refresh == seq) else {
                    return;
                };
                chat.poll.done();
                let view = match result {
                    Ok(view) => view,
                    Err(e) => {
                        chat.error = Some(e);
                        return;
                    }
                };
                chat.error = None;
                let refresh_info = chat.info_due(&view.version);
                if let Some(read) = view.read {
                    chat.pending.retain(|text| {
                        !read
                            .messages
                            .iter()
                            .any(|m| m.role == Role::User && split_from(&m.text()).1.trim() == text)
                    });
                    let asked = read.pending_input.clone();
                    if chat.question.as_ref().map(|q| &q.input.request_id)
                        != asked.as_ref().map(|i| &i.request_id)
                    {
                        chat.question = asked.map(QuestionForm::new);
                        if chat.question.is_some() {
                            tx.scroll_to_bottom("transcript");
                        }
                    }
                    chat.read = Some(read);
                    chat.version = view.version;
                    s.load_thumbs();
                }
                if s.prefs.mark_seen(&view.chat) {
                    s.save_prefs();
                }
                if let Some(chat) = s.existing_at(nav) {
                    chat.chat = Some(view.chat);
                }
                if refresh_info {
                    s.load_info();
                }
            },
        );
    }

    pub fn load_earlier(&mut self) {
        if let Some(chat) = self.chat_mut().and_then(|p| p.existing_mut()) {
            chat.load_earlier();
        }
        self.refresh_chat();
    }

    /// The composer's branch, context and model: the chat's, or a new chat's in its project.
    pub fn load_info(&mut self) {
        let Some(page) = self.chat_mut() else { return };
        let nav = page.nav;
        let (args, project) = match &mut page.target {
            Target::Existing(chat) => {
                chat.info_at = Some(std::time::Instant::now());
                (json!({ "chat": chat.id }), None)
            }
            Target::New(draft) => (
                draft
                    .project
                    .as_ref()
                    .map_or_else(|| json!({}), |p| json!({ "project": p })),
                Some(draft.project.clone()),
            ),
        };
        self.api.call(
            "composer_info",
            args,
            move |s, result: Result<ComposerInfo, String>, _| {
                let Some(page) = s.page_at(nav) else { return };
                // A new chat's project may have changed while this loaded.
                if let Some(project) = &project
                    && page.draft().is_none_or(|d| &d.project != project)
                {
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

    /// Send the composer's text: to the chat, or as a new chat's first message.
    pub fn send(&mut self, tx: &mut Tx) {
        let Some(page) = self.chat_mut() else { return };
        let typed = page.composer.text.trim().to_owned();
        let creating = page.draft().is_some_and(|d| d.creating);
        if typed.is_empty() && page.attached.is_empty() || creating {
            return;
        }
        let paths = match attach::ready(page) {
            Ok(paths) => paths,
            Err(why) => return self.fail(why, ""),
        };
        let text = with_images(&with_skill_refs(&typed, page.skills.list.ready()), &paths);
        let nav = page.nav;
        let Target::Existing(chat) = &mut page.target else {
            self.create(text);
            return;
        };
        let id = chat.id.clone();
        chat.pending.push(text.clone());
        page.composer.clear();
        page.attached.clear();
        self.load_thumbs();
        tx.scroll_to_bottom("transcript");
        let args = json!({ "chat": id, "text": text });
        self.api.call(
            "send_message",
            args,
            move |s, result: Result<Value, String>, _| {
                let Err(e) = result else {
                    s.reload_at(nav);
                    return;
                };
                if let Some(page) = s.page_at(nav) {
                    if let Some(chat) = page.existing_mut() {
                        chat.pending.retain(|p| p != &text);
                    }
                    if page.composer.text.trim().is_empty() && page.attached.is_empty() {
                        page.composer = TextEdit::new(typed);
                        page.composer.set_caret(page.composer.text.len());
                        s.reattach(nav, paths);
                    }
                }
                s.fail("Not sent", e);
            },
        );
    }

    /// Call a tool on the open chat, and refresh it after.
    fn chat_action(&mut self, action: ChatAction) {
        let Some(page) = self.chat_mut() else { return };
        let nav = page.nav;
        let Some(chat) = page.existing() else { return };
        let archive = matches!(action, ChatAction::Archive);
        let (tool, failed) = match &action {
            ChatAction::Stop => ("interrupt_chat", "Couldn't stop"),
            ChatAction::Archive => ("archive_chat", "Couldn't archive"),
            ChatAction::Respond { .. } => ("respond_to_input", "Not sent"),
        };
        let args = match action {
            ChatAction::Stop | ChatAction::Archive => json!({ "chat": chat.id }),
            ChatAction::Respond {
                request_id,
                answers,
            } => json!({ "chat": chat.id, "request_id": request_id, "answers": answers }),
        };
        self.api.call(
            tool,
            args,
            move |s, result: Result<Value, String>, _| match result {
                Ok(_) if archive => {
                    s.notify("Chat archived");
                    if s.page_at(nav).is_some() {
                        s.back();
                    }
                }
                Ok(_) => {
                    if let Some(q) = s.existing_at(nav).and_then(|c| c.question.as_mut()) {
                        q.sending = false;
                    }
                    s.reload_at(nav);
                }
                Err(e) => {
                    if let Some(q) = s.existing_at(nav).and_then(|c| c.question.as_mut()) {
                        q.sending = false;
                    }
                    s.fail(failed, e);
                }
            },
        );
    }

    pub fn stop(&mut self) {
        self.chat_action(ChatAction::Stop);
    }

    pub fn archive(&mut self) {
        self.chat_action(ChatAction::Archive);
    }

    /// Dictate into the composer: macOS listens and types what it hears.
    pub fn dictate(&mut self) {
        let Some(page) = self.chat_mut() else { return };
        page.composer.focused = true;
        if !dictation::start() {
            self.fail("Couldn't start dictation", "macOS didn't take the request");
        }
    }

    /// Answer the agent's questions with the picked options and typed text.
    pub fn respond(&mut self) {
        let Some(form) = self
            .chat_mut()
            .and_then(|p| p.existing_mut())
            .and_then(|c| c.question.as_mut())
        else {
            return;
        };
        let labels: Vec<Vec<&str>> = form
            .picked
            .iter()
            .zip(&form.fields)
            .map(|(picked, field)| {
                let typed = Some(field.text.trim()).filter(|t| !t.is_empty());
                picked.iter().map(String::as_str).chain(typed).collect()
            })
            .collect();
        if labels.iter().any(Vec::is_empty) {
            self.notify("Answer every question first");
            return;
        }
        let answers = form
            .input
            .questions
            .iter()
            .zip(labels)
            .map(|(q, labels)| json!({ "question_id": q.id, "labels": labels }))
            .collect();
        let request_id = form.input.request_id.clone();
        form.sending = true;
        self.chat_action(ChatAction::Respond {
            request_id,
            answers,
        });
    }

    pub fn open_zeron(&mut self) {
        self.api.call(
            "open_zeron",
            json!({}),
            |s, result: Result<Value, String>, _| {
                if let Err(e) = result {
                    s.fail("Couldn't open Zeron", e);
                }
            },
        );
    }

    pub fn restart_zeron(&mut self) {
        self.api.call(
            "restart_zeron",
            json!({}),
            |s, result: Result<Value, String>, _| match result {
                Ok(_) => s.notify("Restarting Zeron"),
                Err(e) => s.fail("Couldn't restart Zeron", e),
            },
        );
    }

    /// Load the skills for the `$` menu: the chat's agent's, or a new chat's in its project.
    pub fn load_skills(&mut self) {
        let Some(page) = self.chat_mut() else { return };
        let args = match &page.target {
            Target::Existing(chat) => json!({ "chat": chat.id }),
            Target::New(d) => match &d.harness {
                Some(h) => json!({ "harness": h, "project": d.project }),
                None => return,
            },
        };
        let key = args.to_string();
        if page.skills.key == key {
            return;
        }
        page.skills.key.clone_from(&key);
        page.skills.list = SkillList::Loading;
        let nav = page.nav;
        self.api.call(
            "list_skills",
            args,
            move |s, result: Result<Skills, String>, _| {
                let Some(skills) = s
                    .page_at(nav)
                    .map(|p| &mut p.skills)
                    .filter(|m| m.key == key)
                else {
                    return;
                };
                skills.list = match result {
                    Ok(got) => SkillList::Ready(got.skills),
                    Err(e) => {
                        // Forget the scope, so the next `$` tries again.
                        skills.key.clear();
                        SkillList::Failed(e)
                    }
                };
            },
        );
    }

    /// Drop the skills of a new chat's old agent or project, and load its new ones.
    pub(super) fn reload_skills(&mut self) {
        if let Some(page) = self.chat_mut() {
            page.skills = SkillMenu::default();
        }
        self.load_skills();
    }
}
