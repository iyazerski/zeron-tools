//! The commands that run in the background (`connect`, `companion`): each
//! one's launch agent, which starts it at login, restarts it after a crash,
//! and logs to a file, and its lock, which keeps one running per Mac.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

pub const CONNECT: Service = Service("connect");
pub const COMPANION: Service = Service("companion");

/// A zeron-tools command that can run in the background, by its name.
#[derive(Debug, Clone, Copy)]
pub struct Service(&'static str);

/// How a service was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// From a terminal: another running one is an error.
    Manual,
    /// By the launch agent: it waits for another running one to stop, and
    /// ends with a clean exit when going on would only fail again.
    Agent,
}

impl Service {
    pub fn name(self) -> &'static str {
        self.0
    }

    fn label(self) -> String {
        format!("dev.iyazerski.zeron-tools.{}", self.0)
    }

    fn plist_path(self) -> PathBuf {
        crate::paths::home()
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", self.label()))
    }

    fn log_path(self) -> PathBuf {
        crate::paths::home().join(format!("Library/Logs/zeron-tools-{}.log", self.0))
    }

    /// Install the agent to run this binary with `args` (ending in `<name>
    /// --agent …`), replacing an earlier one, and start it. Returns the log path.
    pub fn install(self, args: &[String]) -> Result<PathBuf> {
        let exe = std::env::current_exe().context("finding the zeron-tools binary")?;
        let (plist, log) = (self.plist_path(), self.log_path());
        for dir in [plist.parent(), log.parent()].into_iter().flatten() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let agent = LaunchAgent {
            label: self.label(),
            program_arguments: std::iter::once(exe.to_string_lossy().into_owned())
                .chain(args.iter().cloned())
                .collect(),
            run_at_load: true,
            keep_alive: KeepAlive {
                successful_exit: false,
            },
            throttle_interval: 30,
            standard_out_path: log.clone(),
            standard_error_path: log.clone(),
        };
        self.unload()?;
        plist::to_file_xml(&plist, &agent)
            .with_context(|| format!("writing {}", plist.display()))?;
        launchctl(&["bootstrap", &domain(), &plist.to_string_lossy()])?;
        Ok(log)
    }

    /// Restart the agent on the current binary, if it's installed. Returns
    /// whether it restarted.
    pub fn restart(self) -> Result<bool> {
        let plist = self.plist_path();
        if !plist.exists() {
            return Ok(false);
        }
        // Load the job again rather than `kickstart` it: macOS has refused to
        // start a kickstarted agent after an update (a launch constraint
        // violation) until it was loaded again. This also brings back a job
        // launchd dropped after it kept failing.
        self.unload()?;
        launchctl(&["bootstrap", &domain(), &plist.to_string_lossy()])?;
        Ok(true)
    }

    /// Stop the agent's job and wait until launchd lets it go: `bootout`
    /// returns before that, and `bootstrap` fails while the label is loaded.
    fn unload(self) -> Result<()> {
        let target = self.target();
        // Not loaded (it exited cleanly, or never started) is fine.
        let _ = launchctl(&["bootout", &target]);
        for _ in 0..100 {
            if launchctl(&["print", &target]).is_err() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        bail!("launchd didn't stop {target} in 10 s")
    }

    /// Stop and remove the agent. Returns whether one was installed.
    pub fn uninstall(self) -> Result<bool> {
        let plist = self.plist_path();
        if !plist.exists() {
            return Ok(false);
        }
        // Not loaded (it exited cleanly, or never started) is fine.
        let _ = launchctl(&["bootout", &self.target()]);
        std::fs::remove_file(&plist).with_context(|| format!("removing {}", plist.display()))?;
        Ok(true)
    }

    /// Make sure this is the only one running for `data_dir`. The launch
    /// agent waits its turn (a manual run may be holding it); a manual run
    /// fails at once. Hold the returned file while running.
    pub async fn single_instance(self, data_dir: &Path, mode: RunMode) -> Result<std::fs::File> {
        let name = self.0;
        let path = data_dir.join(format!("zeron-tools-{name}.lock"));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match (file.try_lock(), mode) {
            (Ok(()), _) => return Ok(file),
            (Err(std::fs::TryLockError::Error(e)), _) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
            (Err(std::fs::TryLockError::WouldBlock), RunMode::Manual) => bail!(
                "{name} already runs on this Mac (maybe in the background; stop it with zeron-tools uninstall)"
            ),
            (Err(std::fs::TryLockError::WouldBlock), RunMode::Agent) => {}
        }
        eprintln!("another {name} runs on this Mac; waiting for it to stop");
        tokio::task::spawn_blocking(move || file.lock().map(|()| file))
            .await?
            .with_context(|| format!("locking {}", path.display()))
    }

    /// The agent's launchd job.
    fn target(self) -> String {
        format!("{}/{}", domain(), self.label())
    }
}

/// The launch agent's job description, as launchd reads it from the plist.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LaunchAgent {
    label: String,
    program_arguments: Vec<String>,
    run_at_load: bool,
    keep_alive: KeepAlive,
    throttle_interval: u32,
    standard_out_path: PathBuf,
    standard_error_path: PathBuf,
}

/// Restart after a crash, but not after a clean exit (a rejected token,
/// quitting the companion), which the user asked for.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct KeepAlive {
    successful_exit: bool,
}

/// The launchd domain of the logged-in user's GUI session.
fn domain() -> String {
    // SAFETY: getuid has no preconditions and can't fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> Result<()> {
    let out = Command::new("/bin/launchctl")
        .args(args)
        .output()
        .context("running launchctl")?;
    if !out.status.success() {
        bail!(
            "launchctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}
