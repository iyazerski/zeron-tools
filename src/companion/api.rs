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

use crate::connect::{EngineWatch, call_tool, watch_engine};
use crate::json_file;
use crate::pi::Pi;
use crate::zeron::Zeron;

use super::State;
use super::prefs::Prefs;

/// How often the UI checks what's due: polls, the working clock.
const TICK: Duration = Duration::from_secs(1);
/// How long saving one image may take: a hung Zeron must not leave it
/// spinning in the composer.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Api {
    rt: Handle,
    tools: Tools,
    app: AppHandle<State>,
    prefs: watch::Sender<Value>,
}

/// Zeron's phone tools, callable from tasks on the runtime.
#[derive(Clone)]
pub struct Tools {
    zeron: Arc<Zeron>,
    pi: Option<Arc<Pi>>,
    engine: EngineWatch,
}

impl Tools {
    /// Run the phone's tool `tool` with `args` and parse its result, or say why it failed.
    pub async fn call<T: DeserializeOwned>(
        &self,
        tool: &'static str,
        args: Value,
    ) -> Result<T, String> {
        let value = call_tool(&self.zeron, self.pi.as_deref(), &self.engine, tool, args).await?;
        serde_json::from_value(value).map_err(|e| format!("unexpected {tool} answer: {e}"))
    }
}

impl Api {
    /// Watch Zeron's engine, tick the UI and write the prefs to
    /// `prefs_path`, all on `rt`; `pi` cleans up dictation.
    pub fn start(
        rt: Handle,
        zeron: Arc<Zeron>,
        pi: Option<Pi>,
        app: AppHandle<State>,
        prefs_path: PathBuf,
    ) -> Self {
        let engine = {
            let _entered = rt.enter();
            watch_engine(zeron.clone())
        };
        let mut changes = engine.changes();
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
        let prefs = write_prefs(&rt, prefs_path);
        Self {
            rt,
            tools: Tools {
                zeron,
                pi: pi.map(Arc::new),
                engine,
            },
            app,
            prefs,
        }
    }

    /// Save the prefs off the UI thread; losing them only resets what the
    /// list shows as new and the last picks.
    pub fn save_prefs(&self, prefs: &Prefs) {
        match serde_json::to_value(prefs) {
            Ok(prefs) => {
                self.prefs.send_replace(prefs);
            }
            Err(e) => eprintln!("{e:#}"),
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
                match image {
                    Payload::Png { name, bytes } => {
                        check_size(&name, bytes.len() as u64)?;
                        zeron.upload(&name, &bytes).await
                    }
                    Payload::File(path) => {
                        let name = path.file_name().map_or_else(
                            || "image".to_owned(),
                            |n| n.to_string_lossy().into_owned(),
                        );
                        let reading = || format!("reading {}", path.display());
                        let size = tokio::fs::metadata(&path).await.with_context(reading)?;
                        check_size(&name, size.len())?;
                        let bytes = tokio::fs::read(&path).await.with_context(reading)?;
                        zeron.upload(&name, &bytes).await
                    }
                }
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

/// Write the newest prefs sent to `path`, one write at a time: saves made
/// during a slow write collapse into one.
fn write_prefs(rt: &Handle, path: PathBuf) -> watch::Sender<Value> {
    let (saves, mut latest) = watch::channel(Value::Null);
    rt.spawn(async move {
        while latest.changed().await.is_ok() {
            let prefs = latest.borrow_and_update().clone();
            let path = path.clone();
            let written = tokio::task::spawn_blocking(move || json_file::save(&path, &prefs));
            if let Ok(Err(e)) = written.await {
                eprintln!("{e:#}");
            }
        }
    });
    saves
}

/// An image to upload: pasted bytes, or a file picked or copied in Finder.
#[derive(Clone)]
pub enum Payload {
    Png { name: String, bytes: Arc<[u8]> },
    File(PathBuf),
}

/// Zeron's own limit on one image.
const MAX_IMAGE_BYTES: u64 = 24 * 1024 * 1024;

fn check_size(name: &str, bytes: u64) -> anyhow::Result<()> {
    if bytes > MAX_IMAGE_BYTES {
        bail!("{name} is too large (24 MB max)");
    }
    Ok(())
}
