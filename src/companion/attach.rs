//! Images in the composer, pasted or picked. Each one is saved in Zeron's
//! uploads as soon as it's attached, as Zeron's composer does, so sending
//! only has to cite the saved paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rune::{ClipboardContent, FileDialogOptions, Image, ImageCache, Tx};

use super::api::Payload;
use super::app::{ChatPage, State};
use super::data::split_images;

/// Thumbnails decode at this many points square: the largest they draw.
const THUMB: f32 = 72.0;
/// Thumbnails decode for Retina screens; smaller draws use the mips.
const SCALE: f32 = 2.0;
const EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

pub struct Attached {
    pub id: u64,
    pub thumb: Option<Image>,
    pub upload: Upload,
}

pub enum Upload {
    Sending,
    Done(String),
    Failed(String),
}

impl State {
    /// Attach the images a paste carries; any other paste stays text.
    pub fn paste(&mut self, content: &ClipboardContent, _: &mut Tx) -> bool {
        let files = content.files();
        if !files.is_empty() {
            // A copy in Finder also carries the file's icon as its image.
            let images: Vec<PathBuf> = files.iter().filter(|p| is_image(p)).cloned().collect();
            let any = !images.is_empty();
            for path in images {
                self.attach(Payload::File(path));
            }
            return any;
        }
        // Office apps put a picture of the copied text beside it.
        match content.image() {
            Some(image) if content.text().trim().is_empty() => {
                let n = self.chat().map_or(0, |p| p.attached.len()) + 1;
                self.attach(Payload::Png(format!("Image_{n}.png"), image.png().into()));
                true
            }
            _ => false,
        }
    }

    /// Pick an image file to attach.
    pub fn pick_image(_: &mut State, tx: &mut Tx) {
        let options = FileDialogOptions {
            title: "Attach an image".into(),
            ..Default::default()
        }
        .filter("Images", &EXTENSIONS);
        tx.open_file(options, |s: &mut State, picked, _| match picked {
            Ok(Some(path)) => s.attach(Payload::File(path)),
            Ok(None) => {}
            Err(e) => s.toast("Couldn't open the image", e.to_string(), true),
        });
    }

    fn attach(&mut self, image: Payload) {
        let thumb = self.images.as_ref().map(|cache| match &image {
            Payload::Png(_, bytes) => cache.load(Arc::clone(bytes)),
            Payload::File(path) => cache.file(path, THUMB, SCALE),
        });
        let Some(page) = self.chat_mut() else { return };
        let id = page.attached_id();
        page.attached.push(Attached {
            id,
            thumb,
            upload: Upload::Sending,
        });
        let nav = page.nav;
        self.api.upload(image, move |s, saved, _| {
            let Some(page) = s.page_at(nav) else { return };
            let Some(attached) = page.attached.iter_mut().find(|a| a.id == id) else {
                return;
            };
            match saved {
                Ok(path) => attached.upload = Upload::Done(path),
                Err(e) => {
                    attached.upload = Upload::Failed(e.clone());
                    s.toast("Couldn't attach the image", e, true);
                }
            }
        });
    }

    /// Put back the images of a message that didn't send.
    pub fn reattach(&mut self, nav: u64, paths: Vec<String>) {
        let thumbs: Vec<Option<Image>> = paths
            .iter()
            .map(|p| self.images.as_ref().map(|c| c.file(p, THUMB, SCALE)))
            .collect();
        let Some(page) = self.page_at(nav) else {
            return;
        };
        for (path, thumb) in paths.into_iter().zip(thumbs) {
            let id = page.attached_id();
            page.attached.push(Attached {
                id,
                thumb,
                upload: Upload::Done(path),
            });
        }
    }

    pub fn detach(&mut self, id: u64) {
        if let Some(page) = self.chat_mut() {
            page.attached.retain(|a| a.id != id);
        }
    }

    /// Decode the transcript's images the first time they show.
    pub fn load_thumbs(&mut self) {
        let (Some(cache), super::app::Page::Chat(page)) = (&self.images, &mut self.page) else {
            return;
        };
        let sent = page.read.iter().flat_map(|r| &r.messages).map(|m| m.text());
        let texts: Vec<String> = sent.chain(page.pending.iter().cloned()).collect();
        for text in &texts {
            for path in split_images(text).1 {
                remember(cache, &mut page.thumbs, path);
            }
        }
    }
}

fn remember(cache: &ImageCache, thumbs: &mut HashMap<String, Image>, path: &str) {
    if !thumbs.contains_key(path) {
        thumbs.insert(path.to_owned(), cache.file(path, THUMB, SCALE));
    }
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// The attached images' saved paths, or why the message can't go yet.
pub fn ready(page: &ChatPage) -> Result<Vec<String>, &'static str> {
    page.attached
        .iter()
        .map(|a| match &a.upload {
            Upload::Done(path) => Ok(path.clone()),
            Upload::Sending => Err("The images are still uploading."),
            Upload::Failed(_) => Err("An image didn't attach. Remove it to send."),
        })
        .collect()
}
