//! zeron-tools: a background service for the running Zeron (`connect`),
//! plus `export`. Both talk to Zeron through its engine's loopback IPC, so
//! they work while Zeron runs and don't depend on Zeron's code.

mod archive;
mod connect;
mod export;
mod paths;
mod rename;
mod text;
mod zeron;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};

use crate::connect::{Link, NotifyWhen, RelayLink, RunMode, Upkeep};
use crate::export::Format;
use crate::rename::Pi;
use crate::zeron::Zeron;
use crate::zeron::transcript::RenderOptions;

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
    /// Run the background service: keep the sidebar short and link this
    /// Mac's Zeron to a relay, so you can use it from a phone
    ///
    /// While Zeron runs, it archives all but the most recently active chats
    /// of each project (pinned chats and chats in sidebar sections stay) and
    /// titles chats that stayed untitled or got Zeron's fallback title, with a small model through pi.
    ///
    /// With a relay link, it dials out to the relay and stays connected, so
    /// the phone app can list chats, follow them live, send messages, stop
    /// turns, answer agent questions, and start chats, and it reports chats
    /// that finish, need an answer, or fail, for notifications. Zeron may be
    /// closed: the app shows that and can open it. Pass --relay and --token
    /// once (the app shows them when you add a Mac); they're saved.
    Connect(ConnectArgs),
    /// Export chats as plain text (needs Zeron running)
    ///
    /// Without --chat or --project, every chat is exported. Filters combine.
    /// Chats come out oldest first, with whole transcripts.
    Export(ExportArgs),
}

#[derive(Args)]
struct ConnectArgs {
    /// The relay's address, like https://zeron.example.com
    #[arg(long, requires = "token")]
    relay: Option<String>,
    /// This Mac's token from the app
    #[arg(long, requires = "relay")]
    token: Option<String>,
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
    /// Don't title chats Zeron couldn't
    #[arg(long)]
    no_rename: bool,
    #[command(flatten)]
    pi: PiOpts,
    /// Run in the background from now on, with these options: start at
    /// login, restart after a crash, log to
    /// ~/Library/Logs/zeron-tools-connect.log
    #[arg(long, conflicts_with = "uninstall")]
    install: bool,
    /// Stop running in the background
    #[arg(long, conflicts_with_all = ["relay", "notify_always"])]
    uninstall: bool,
    /// Set by the launch agent
    #[arg(long, hide = true)]
    agent: bool,
}

#[derive(Args)]
struct ExportArgs {
    /// Chat ids or id prefixes
    #[arg(long = "chat", value_name = "ID")]
    chats: Vec<String>,
    /// Project name or folder (every chat in it)
    #[arg(long)]
    project: Option<String>,
    /// Only chats active on or after this local date (YYYY-MM-DD) or
    /// within this age (30d, 4w)
    #[arg(long, value_parser = export::parse_since)]
    since: Option<DateTime<Utc>>,
    /// Only chats started on or before this local date (YYYY-MM-DD)
    #[arg(long, value_parser = export::parse_until)]
    until: Option<DateTime<Utc>>,
    /// At most this many chats, the most recently active ones
    #[arg(long)]
    limit: Option<usize>,
    /// Leave out archived chats
    #[arg(long)]
    skip_archived: bool,
    #[arg(long, value_enum, default_value = "markdown")]
    format: Format,
    /// Include the model's reasoning
    #[arg(long)]
    reasoning: bool,
    /// Leave out the one-line-per-call tool log
    #[arg(long)]
    no_tools: bool,
    /// Write one file per chat into this folder (default: all to stdout)
    #[arg(long, short)]
    output: Option<PathBuf>,
}

/// The model that writes titles.
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
        let binary = paths::which(&self.pi).with_context(|| {
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Die quietly on a closed pipe (`zeron-tools export | head`) like other CLIs.
    #[cfg(unix)]
    // SAFETY: this runs before any other thread exists, and SIG_DFL is a
    // valid disposition for SIGPIPE.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    match cli.command {
        Cmd::Connect(args) => run_connect(&cli.global, args).await,
        Cmd::Export(args) => run_export(&cli.global, args).await,
    }
}

/// Run `connect`: install or uninstall the launch agent, or serve until stopped.
async fn run_connect(global: &Global, args: ConnectArgs) -> Result<()> {
    if args.uninstall {
        if connect::uninstall()? {
            println!("stopped; connect no longer runs in the background");
        } else {
            println!("connect wasn't running in the background");
        }
        return Ok(());
    }
    let data_dir = global.data_dir();
    let notify = if args.notify_always {
        NotifyWhen::Always
    } else {
        NotifyWhen::Away
    };
    let mode = if args.agent {
        RunMode::Agent
    } else {
        RunMode::Manual
    };
    let path = connect::link_path(&data_dir);
    // A new link is saved once the relay accepts it, so a mistyped token
    // doesn't replace a working one.
    let relay = match args.relay.zip(args.token) {
        Some((relay, token)) => Some(RelayLink {
            link: Link { relay, token },
            save_to: Some(path),
        }),
        None => Link::load(&path)?.map(|link| RelayLink {
            link,
            save_to: None,
        }),
    };
    let pi = if args.no_rename {
        None
    } else {
        Some(args.pi.resolve()?)
    };
    let upkeep = Upkeep {
        keep: (!args.no_archive).then_some(args.keep),
        pi,
        memory: data_dir.join("zeron-tools-archived.json"),
    };
    if relay.is_none() && !upkeep.has_work() {
        bail!(
            "nothing to do: add this Mac in the phone app (it shows the command), or drop --no-archive or --no-rename"
        );
    }
    if args.install {
        return install(global, notify, &upkeep, relay.as_ref()).await;
    }
    if relay.is_none() {
        eprintln!("no phone link yet: add this Mac in the app to use it from a phone");
    }
    let _only = connect::single_instance(&data_dir, mode).await?;
    connect::run(connect::Opts {
        relay,
        zeron: Zeron::new(global.ipc_port, &data_dir),
        notify,
        upkeep,
        mode,
    })
    .await
}

/// Save a new `relay` link once the relay accepts it, then install the
/// launch agent with this run's options.
async fn install(
    global: &Global,
    notify: NotifyWhen,
    upkeep: &Upkeep,
    relay: Option<&RelayLink>,
) -> Result<()> {
    if let Some(RelayLink {
        link,
        save_to: Some(path),
    }) = relay
    {
        connect::verify(link).await?;
        link.save(path)?;
    }
    let log = connect::install(&agent_args(global, notify, upkeep))?;
    println!(
        "connect now runs in the background and starts at login; log: {}",
        log.display()
    );
    if relay.is_none() {
        println!("to use Zeron from a phone, add this Mac in the app and run the command it shows");
    }
    Ok(())
}

/// Run `export` against the running Zeron.
async fn run_export(global: &Global, args: ExportArgs) -> Result<()> {
    let opts = export::Opts {
        chats: args.chats,
        project: args.project,
        since: args.since,
        until: args.until,
        limit: args.limit,
        skip_archived: args.skip_archived,
        format: args.format,
        render: RenderOptions {
            include_reasoning: args.reasoning,
            include_tools: !args.no_tools,
        },
        output: args.output,
    };
    export::run(&Zeron::new(global.ipc_port, &global.data_dir()), opts).await
}

/// The launch agent's arguments: this run's options, minus the link (saved).
fn agent_args(global: &Global, notify: NotifyWhen, upkeep: &Upkeep) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(dir) = &global.data_dir {
        args.extend(["--data-dir".into(), dir.to_string_lossy().into_owned()]);
    }
    args.extend(["--ipc-port".into(), global.ipc_port.to_string()]);
    args.extend(["connect".into(), "--agent".into()]);
    if notify == NotifyWhen::Always {
        args.push("--notify-always".into());
    }
    match upkeep.keep {
        Some(keep) => args.extend(["--keep".into(), keep.to_string()]),
        None => args.push("--no-archive".into()),
    }
    match &upkeep.pi {
        Some(pi) => args.extend([
            "--pi".into(),
            pi.binary.to_string_lossy().into_owned(),
            "--title-model".into(),
            pi.model.clone(),
            "--title-thinking".into(),
            pi.thinking.clone(),
        ]),
        None => args.push("--no-rename".into()),
    }
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
