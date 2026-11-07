//! The bridge from the window to Zeron: tool calls run on a tokio runtime and
//! hand their results back to the UI thread through the app's handle.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use rune::{AppHandle, Tx};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::watch;
use zeron_relay_proto::Engine;

use crate::connect::{call_tool, watch_engine};
use crate::zeron::Zeron;

use super::State;

/// How often the UI checks what's due: polls, the working clock.
const TICK: Duration = Duration::from_secs(1);
/// How long saving one image may take: a hung Zeron must not leave it
/// spinning in the composer.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Api {
    rt: Handle,
    tools: Tools,
    app: AppHandle<State>,
}

/// Zeron's phone tools, callable from tasks on the runtime.
#[derive(Clone)]
pub struct Tools {
    zeron: Arc<Zeron>,
    engine: watch::Receiver<Engine>,
}

impl Tools {
    /// Run the phone's tool `tool` with `args` and parse its result, or say why it failed.
    pub async fn call<T: DeserializeOwned>(
        &self,
        tool: &'static str,
        args: Value,
    ) -> Result<T, String> {
        let state = self.engine.borrow().clone();
        let value = call_tool(&self.zeron, &state, tool, args).await?;
        serde_json::from_value(value).map_err(|e| format!("unexpected {tool} answer: {e}"))
    }
}

impl Api {
    /// Watch Zeron's engine and tick the UI, both on `rt`.
    pub fn start(rt: Handle, zeron: Arc<Zeron>, app: AppHandle<State>) -> Self {
        let engine = {
            let _entered = rt.enter();
            watch_engine(zeron.clone())
        };
        let mut changes = engine.clone();
        let to_ui = app.clone();
        rt.spawn(async move {
            loop {
                let now = changes.borrow_and_update().clone();
                to_ui.update(move |s, _| s.engine_changed(now));
                if changes.changed().await.is_err() {
                    break;
                }
            }
        });
        let ticks = app.clone();
        rt.spawn(async move {
            let mut every = tokio::time::interval(TICK);
            loop {
                every.tick().await;
                ticks.update_if(State::tick);
            }
        });
        Self {
            rt,
            tools: Tools { zeron, engine },
            app,
        }
    }

    pub fn app(&self) -> &AppHandle<State> {
        &self.app
    }

    /// Run the phone's tool `tool` with `args`, then `done` with its parsed
    /// result (or why it failed) on the UI thread.
    pub fn call<T: DeserializeOwned + Send + 'static>(
        &self,
        tool: &'static str,
        args: Value,
        done: impl FnOnce(&mut State, Result<T, String>, &mut Tx) + Send + 'static,
    ) {
        self.run(
            move |tools| async move { tools.call(tool, args).await },
            done,
        );
    }

    /// Run `work` with the tools on the runtime, then `done` with its result
    /// on the UI thread: for answers that take more than one call.
    pub fn run<R: Send + 'static, F: Future<Output = R> + Send + 'static>(
        &self,
        work: impl FnOnce(Tools) -> F,
        done: impl FnOnce(&mut State, R, &mut Tx) + Send + 'static,
    ) {
        let app = self.app.clone();
        let work = work(self.tools.clone());
        self.rt.spawn(async move {
            let result = work.await;
            app.update(move |s, tx| done(s, result, tx));
        });
    }

    /// Save an image in Zeron's uploads, then `done` with its path on the UI thread.
    pub fn upload(
        &self,
        image: Payload,
        done: impl FnOnce(&mut State, Result<String, String>, &mut Tx) + Send + 'static,
    ) {
        let (zeron, app) = (self.tools.zeron.clone(), self.app.clone());
        self.rt.spawn(async move {
            let saving = async {
                let (name, bytes) = match image {
                    Payload::Png(name, bytes) => (name, bytes.to_vec()),
                    Payload::File(path) => {
                        let name = path.file_name().map_or_else(
                            || "image".to_owned(),
                            |n| n.to_string_lossy().into_owned(),
                        );
                        let bytes = tokio::fs::read(&path)
                            .await
                            .with_context(|| format!("reading {}", path.display()))?;
                        (name, bytes)
                    }
                };
                if bytes.len() > MAX_IMAGE_BYTES {
                    bail!("{} is too large (24 MB max)", name.as_str());
                }
                zeron.upload(&name, &bytes).await
            };
            let result = match tokio::time::timeout(UPLOAD_TIMEOUT, saving).await {
                Ok(saved) => saved
                    .map(|path| path.to_string_lossy().into_owned())
                    .map_err(|e| format!("{e:#}")),
                Err(_) => Err("Zeron didn't save the image in time".to_owned()),
            };
            app.update(move |s, tx| done(s, result, tx));
        });
    }
}

/// An image to upload: pasted bytes, or a file picked or copied in Finder.
pub enum Payload {
    Png(String, Arc<[u8]>),
    File(PathBuf),
}

/// Zeron's own limit on one image.
const MAX_IMAGE_BYTES: usize = 24 * 1024 * 1024;
