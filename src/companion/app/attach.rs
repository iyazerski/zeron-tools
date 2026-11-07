//! Images in the composer, pasted or picked. Each one is saved in Zeron's
//! uploads as soon as it's attached, as Zeron's composer does, so sending
//! only has to cite the saved paths.

use std::path::{Path, PathBuf};

use image::imageops::FilterType;
use rune::{ClipboardContent, FileDialogOptions, Image, Tx};

use super::page::Existing;
use super::{ChatPage, State};
use crate::companion::api::Payload;
use crate::companion::data::Role;
use crate::companion::message::split_images;

/// Thumbnails shrink to this many pixels square: the largest they draw
/// (72 points) on a Retina screen. Smaller draws use the mips.
const THUMB_PX: u32 = 144;
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
                self.attach(Payload::Png {
                    name: format!("Image_{n}.png"),
                    bytes: image.png().into(),
                });
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
            Err(e) => s.fail("Couldn't open the image", e.to_string()),
        });
    }

    fn attach(&mut self, image: Payload) {
        let Some(page) = self.chat_mut() else { return };
        let (id, nav) = (page.attached_id(), page.nav);
        page.attached.push(Attached {
            id,
            thumb: None,
            upload: Upload::Sending,
        });
        self.attach_thumb(nav, id, image.clone());
        self.api.upload(image, move |s, saved, _| {
            let Some(attached) = s.attached_at(nav, id) else {
                return;
            };
            match saved {
                Ok(path) => attached.upload = Upload::Done(path),
                Err(e) => {
                    attached.upload = Upload::Failed(e.clone());
                    s.fail("Couldn't attach the image", e);
                }
            }
        });
    }

    /// Put back the images of a message that didn't send.
    pub fn reattach(&mut self, nav: u64, paths: Vec<String>) {
        for path in paths {
            let Some(page) = self.page_at(nav) else {
                return;
            };
            let id = page.attached_id();
            page.attached.push(Attached {
                id,
                thumb: None,
                upload: Upload::Done(path.clone()),
            });
            self.attach_thumb(nav, id, Payload::File(path.into()));
        }
    }

    fn attach_thumb(&self, nav: u64, id: u64, image: Payload) {
        self.decode_thumb(image, move |s, thumb| {
            if let Some(attached) = s.attached_at(nav, id) {
                attached.thumb = Some(thumb);
            }
        });
    }

    fn attached_at(&mut self, nav: u64, id: u64) -> Option<&mut Attached> {
        self.page_at(nav)?.attached.iter_mut().find(|a| a.id == id)
    }

    pub fn detach(&mut self, id: u64) {
        if let Some(page) = self.chat_mut() {
            page.attached.retain(|a| a.id != id);
        }
    }

    /// Decode the transcript's images the first time they show.
    pub fn load_thumbs(&mut self) {
        let Some(page) = self.chat_mut() else { return };
        let nav = page.nav;
        let Some(Existing {
            read,
            pending,
            thumbs,
            ..
        }) = page.existing_mut()
        else {
            return;
        };
        let mut new = Vec::new();
        let mut note = |text: &str| {
            for path in split_images(text).1 {
                if !thumbs.contains_key(path) {
                    thumbs.insert(path.to_owned(), None);
                    new.push(path.to_owned());
                }
            }
        };
        // Only the user's messages carry images.
        let sent = read.iter().flat_map(|r| &r.messages);
        for m in sent.filter(|m| m.role == Role::User) {
            note(&m.text());
        }
        pending.iter().for_each(|text| note(text));
        for path in new {
            self.decode_thumb(Payload::File(PathBuf::from(&path)), move |s, thumb| {
                if let Some(chat) = s.existing_at(nav) {
                    chat.thumbs.insert(path, Some(thumb));
                }
            });
        }
    }

    /// Shrink an image to a thumbnail off the UI thread, then hand it to
    /// `done`. Rune keeps an image at its full size, so a few screenshots
    /// would hold hundreds of megabytes; one that fails keeps its placeholder.
    fn decode_thumb(&self, image: Payload, done: impl FnOnce(&mut State, Image) + Send + 'static) {
        if self.images.is_none() {
            return;
        }
        self.api.run(
            move |_| async move { tokio::task::spawn_blocking(move || thumbnail(&image)).await },
            move |s, decoded, _| {
                let (Some(cache), Ok(Ok(rgba))) = (&s.images, decoded) else {
                    return;
                };
                let (width, height) = rgba.dimensions();
                let thumb = cache.rgba(width, height, rgba.into_raw());
                done(s, thumb);
            },
        );
    }
}

/// Decode an image and crop it to a centered square, as the tiles show it.
fn thumbnail(image: &Payload) -> image::ImageResult<image::RgbaImage> {
    let decoded = match image {
        Payload::Png { bytes, .. } => image::load_from_memory(bytes)?,
        Payload::File(path) => image::ImageReader::open(path)?
            .with_guessed_format()?
            .decode()?,
    };
    Ok(decoded
        .resize_to_fill(THUMB_PX, THUMB_PX, FilterType::Triangle)
        .into_rgba8())
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

impl ChatPage {
    /// Whether every attached image is saved, so the message can go.
    pub fn uploads_done(&self) -> bool {
        self.attached
            .iter()
            .all(|a| matches!(a.upload, Upload::Done(_)))
    }
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
