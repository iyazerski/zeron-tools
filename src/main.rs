//! zeron-tools: background services for the running Zeron, set up together
//! by `install`: `connect` keeps the sidebar short and links Zeron to the
//! phone, and `companion` shows the chats in a floating window. They talk to
//! Zeron through its engine's loopback IPC, so they work while Zeron runs and
//! don't depend on Zeron's code. `update` installs the latest release.

mod archive;
mod companion;
mod connect;
mod paths;
mod pi;
mod rename;
mod service;
mod text;
mod update;
mod zeron;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use zeron_json_file as json_file;

use crate::connect::{Link, NotifyWhen, Upkeep};
use crate::pi::Pi;
use crate::service::{COMPANION, CONNECT, RunMode};
use crate::zeron::Zeron;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Args)]
struct Global {
    /// Zeron data dir, where Zeron writes its IPC token and zeron-tools keeps
    /// its files [default: $ZERON_DATA_DIR or ~/.zeron]
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Zeron engine's IPC port
    #[arg(long, global = true, env = "ZERON_IPC_PORT", default_value_t = zeron::DEFAULT_IPC_PORT)]
    ipc_port: u16,
}

impl Global {
    /// The data dir to use: `--data-dir`, else Zeron's default.
    fn data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(zeron::data_dir)
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Run zeron-tools in the background from now on: start at login,
    /// restart after a crash. Run it again to change the options
    ///
    /// While Zeron runs, it archives all but the most recently active chats
    /// of each project (pinned chats and chats in sidebar sections stay) and
    /// titles chats that stayed untitled or got Zeron's fallback title, with
    /// a small model through pi.
    ///
    /// With a relay link, it dials out to the relay and stays connected, so
    /// the phone app can list chats, follow them live, send messages, stop
    /// turns, answer agent questions, and start chats, and it reports chats
    /// that finish, need an answer, or fail, for notifications. Zeron may be
    /// closed: the app shows that and can open it. Pass --relay and --token
    /// once (the app shows them when you add a Mac); they're saved.
    ///
    /// A global shortcut shows a floating window with the same chats as the
    /// phone app: pick a chat or start one, follow it, send messages and
    /// images, and answer the agent's questions.
    ///
    /// Logs go to ~/Library/Logs/zeron-tools-connect.log and
    /// ~/Library/Logs/zeron-tools-companion.log.
    Install(InstallArgs),
    /// Stop running in the background
    Uninstall,
    /// Install the latest release if it's newer, and restart the background
    /// services on it
    Update,
    /// Keep the sidebar short and serve the relay link; the launch agent runs it
    #[command(hide = true)]
    Connect(ConnectArgs),
    /// Show the chats window on the shortcut; the launch agent runs it
    #[command(hide = true)]
    Companion(CompanionArgs),
}

#[derive(Args)]
struct InstallArgs {
    /// The relay's address, like https://zeron.example.com
    #[arg(long, requires = "token")]
    relay: Option<String>,
    /// This Mac's token from the app
    #[arg(long, requires = "relay")]
    token: Option<String>,
    #[command(flatten)]
    connect: ConnectOpts,
    #[command(flatten)]
    companion: CompanionOpts,
}

#[derive(Args)]
struct ConnectArgs {
    #[command(flatten)]
    opts: ConnectOpts,
    #[command(flatten)]
    agent: AgentFlag,
}

#[derive(Args)]
struct CompanionArgs {
    #[command(flatten)]
    opts: CompanionOpts,
    /// Show the window at once instead of waiting for the shortcut
    #[arg(long)]
    show: bool,
    #[command(flatten)]
    model: ModelOpts,
    #[command(flatten)]
    agent: AgentFlag,
}

#[derive(Args)]
struct AgentFlag {
    /// Set by the launch agent
    #[arg(long)]
    agent: bool,
}

impl AgentFlag {
    fn mode(&self) -> RunMode {
        if self.agent {
            RunMode::Agent
        } else {
            RunMode::Manual
        }
    }
}

/// What `connect` does besides the relay link.
#[derive(Args)]
struct ConnectOpts {
    /// Notify even while someone uses the Mac (by default, notifications
    /// wait until it has had no keyboard or mouse input for 2 minutes)
    #[arg(long)]
    notify_always: bool,
    /// Keep this many most recently active chats per project in the sidebar
    #[arg(long, default_value_t = 10)]
    keep: usize,
    /// Don't archive old chats
    #[arg(long)]
    no_archive: bool,
    #[command(flatten)]
    model: ModelOpts,
}

impl ConnectOpts {
    fn notify(&self) -> NotifyWhen {
        if self.notify_always {
            NotifyWhen::Always
        } else {
            NotifyWhen::Away
        }
    }

    /// Resolve the upkeep's settings, with its memory in `data_dir`.
    fn upkeep(self, data_dir: &Path) -> Result<Upkeep> {
        Ok(Upkeep {
            keep: (!self.no_archive).then_some(self.keep),
            pi: self.model.resolve()?,
            memory: data_dir.join("zeron-tools-archived.json"),
        })
    }
}

#[derive(Args)]
struct CompanionOpts {
    /// The global shortcut that shows and hides the chats window, like
    /// cmd+shift+k
    #[arg(long, default_value = "ctrl+alt+space")]
    shortcut: String,
}

/// The model that titles chats and cleans up dictation.
#[derive(Args)]
struct ModelOpts {
    /// Don't title chats Zeron couldn't, nor clean up dictation
    #[arg(long)]
    no_rename: bool,
    #[command(flatten)]
    pi: PiOpts,
}

impl ModelOpts {
    /// The model, unless it's off.
    fn resolve(self) -> Result<Option<Pi>> {
        if self.no_rename {
            return Ok(None);
        }
        self.pi.resolve().map(Some)
    }
}

#[derive(Args)]
struct PiOpts {
    /// Title model id from `pi --list-models`; pi finds the provider. Prefix
    /// it as `provider/model` to pick one when several serve the model
    #[arg(long, default_value = "gpt-6-luna")]
    title_model: String,
    /// Thinking level for the title model
    #[arg(long, default_value = "low")]
    title_thinking: String,
    /// Pi binary
    #[arg(long, env = "PI_BIN", default_value = "pi")]
    pi: PathBuf,
}

impl PiOpts {
    /// Resolve the pi binary to an absolute path now: launchd starts the
    /// agent with a bare `PATH`.
    fn resolve(self) -> Result<Pi> {
        let binary = which::which(&self.pi).with_context(|| {
            format!(
                "{} isn't on PATH; pass --pi, or --no-rename to skip titling",
                self.pi.display()
            )
        })?;
        Ok(Pi {
            binary,
            model: self.title_model,
            thinking: self.title_thinking,
        })
    }
}

// Not `#[tokio::main]`: the companion's window needs the main thread, and
// runs its own runtime beside it.
fn main() -> Result<()> {
    let cli = Cli::parse();
    let runtime = || tokio::runtime::Runtime::new().context("starting the runtime");
    match cli.command {
        Cmd::Install(args) => runtime()?.block_on(install(&cli.global, args)),
        Cmd::Uninstall => uninstall(),
        Cmd::Update => update::run(),
        Cmd::Connect(args) => runtime()?.block_on(run_connect(&cli.global, args)),
        Cmd::Companion(args) => run_companion(&cli.global, args),
    }
}

/// Save a new relay link once the relay accepts it, then install both launch
/// agents with this run's options.
async fn install(global: &Global, args: InstallArgs) -> Result<()> {
    let data_dir = global.data_dir();
    let notify = args.connect.notify();
    let upkeep = args.connect.upkeep(&data_dir)?;
    let path = connect::link_path(&data_dir);
    // Verified first, so a mistyped token doesn't replace a working link.
    let linked = match args.relay.zip(args.token) {
        Some((relay, token)) => {
            let link = Link { relay, token };
            connect::verify(&link).await?;
            link.save(&path)?;
            true
        }
        None => Link::load(&path)?.is_some(),
    };
    if !linked && !upkeep.has_work() {
        bail!(
            "nothing to do: add this Mac in the phone app (it shows the command), or drop --no-archive or --no-rename"
        );
    }
    let connect_log = CONNECT.install(&agent_args(global, notify, &upkeep))?;
    let shortcut = args.companion.shortcut;
    let companion_log = COMPANION.install(&companion_args(global, &shortcut, &upkeep))?;
    println!(
        "zeron-tools now runs in the background and starts at login; {shortcut} shows the chats window"
    );
    println!(
        "logs: {} and {}",
        connect_log.display(),
        companion_log.display()
    );
    if !linked {
        println!("to use Zeron from a phone, add this Mac in the app and run the command it shows");
    }
    Ok(())
}

/// Stop and remove both launch agents.
fn uninstall() -> Result<()> {
    let mut stopped = false;
    for service in [CONNECT, COMPANION] {
        stopped |= service.uninstall()?;
    }
    if stopped {
        println!("stopped; zeron-tools no longer runs in the background");
    } else {
        println!("zeron-tools wasn't running in the background");
    }
    Ok(())
}

/// Run `connect` until stopped.
async fn run_connect(global: &Global, args: ConnectArgs) -> Result<()> {
    let data_dir = global.data_dir();
    let notify = args.opts.notify();
    let upkeep = args.opts.upkeep(&data_dir)?;
    let relay = Link::load(&connect::link_path(&data_dir))?;
    if relay.is_none() && !upkeep.has_work() {
        bail!("nothing to do: no phone link, and both archiving and titling are off");
    }
    if relay.is_none() {
        eprintln!("no phone link yet: add this Mac in the app to use it from a phone");
    }
    let mode = args.agent.mode();
    // Two would keep replacing each other on the relay.
    let _only = CONNECT.single_instance(&data_dir, mode).await?;
    connect::run(connect::Opts {
        relay,
        zeron: Zeron::new(global.ipc_port, &data_dir),
        notify,
        upkeep,
        mode,
    })
    .await
}

/// Run `companion`: show the window on the shortcut until it quits.
fn run_companion(global: &Global, args: CompanionArgs) -> Result<()> {
    // Without pi the window still works; dictation stays as heard.
    let pi = args.model.resolve().unwrap_or_else(|e| {
        eprintln!("{e:#}");
        None
    });
    companion::run(companion::Opts {
        pi,
        data_dir: &global.data_dir(),
        ipc_port: global.ipc_port,
        shortcut: &args.opts.shortcut,
        show: args.show,
        mode: args.agent.mode(),
    })
}

/// `companion`'s launch agent arguments.
fn companion_args(global: &Global, shortcut: &str, upkeep: &Upkeep) -> Vec<String> {
    let mut args = global_args(global);
    args.extend(["companion".into(), "--agent".into()]);
    args.extend(["--shortcut".into(), shortcut.to_owned()]);
    args.extend(model_args(upkeep.pi.as_ref()));
    args
}

/// `connect`'s launch agent arguments: this run's options, minus the link
/// (saved).
fn agent_args(global: &Global, notify: NotifyWhen, upkeep: &Upkeep) -> Vec<String> {
    let mut args = global_args(global);
    args.extend(["connect".into(), "--agent".into()]);
    if notify == NotifyWhen::Always {
        args.push("--notify-always".into());
    }
    match upkeep.keep {
        Some(keep) => args.extend(["--keep".into(), keep.to_string()]),
        None => args.push("--no-archive".into()),
    }
    args.extend(model_args(upkeep.pi.as_ref()));
    args
}

/// The model options a launch agent repeats.
fn model_args(pi: Option<&Pi>) -> Vec<String> {
    match pi {
        Some(pi) => vec![
            "--pi".into(),
            pi.binary.to_string_lossy().into_owned(),
            "--title-model".into(),
            pi.model.clone(),
            "--title-thinking".into(),
            pi.thinking.clone(),
        ],
        None => vec!["--no-rename".into()],
    }
}

/// The global options a launch agent repeats.
fn global_args(global: &Global) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(dir) = &global.data_dir {
        args.extend(["--data-dir".into(), dir.to_string_lossy().into_owned()]);
    }
    args.extend(["--ipc-port".into(), global.ipc_port.to_string()]);
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_repeats_the_upkeep_options() {
        let upkeep = Upkeep {
            keep: Some(5),
            pi: Some(Pi {
                binary: "/bin/pi".into(),
                model: "m".into(),
                thinking: "low".into(),
            }),
            memory: PathBuf::new(),
        };
        let global = Global {
            data_dir: None,
            ipc_port: 1,
        };
        assert_eq!(
            agent_args(&global, NotifyWhen::Always, &upkeep).join(" "),
            "--ipc-port 1 connect --agent --notify-always --keep 5 --pi /bin/pi --title-model m --title-thinking low"
        );
        assert_eq!(
            companion_args(&global, "cmd+k", &upkeep).join(" "),
            "--ipc-port 1 companion --agent --shortcut cmd+k --pi /bin/pi --title-model m --title-thinking low"
        );
        let off = Upkeep {
            keep: None,
            pi: None,
            memory: PathBuf::new(),
        };
        let global = Global {
            data_dir: Some("/d".into()),
            ipc_port: 1,
        };
        assert_eq!(
            agent_args(&global, NotifyWhen::Away, &off).join(" "),
            "--data-dir /d --ipc-port 1 connect --agent --no-archive --no-rename"
        );
    }
}
