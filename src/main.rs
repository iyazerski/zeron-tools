//! zeron-tools: a background service for the running Zeron (`connect`),
//! plus `export`. Both talk to Zeron through its engine's loopback IPC, so
//! they work while Zeron runs and don't depend on Zeron's code.

mod archive;
mod connect;
mod export;
mod rename;
mod zeron;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};

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
    /// Zeron data dir, where zeron-tools keeps its files [default:
    /// $ZERON_DATA_DIR or ~/.zeron]
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Zeron engine's IPC port
    #[arg(long, global = true, env = "ZERON_IPC_PORT", default_value_t = zeron::DEFAULT_IPC_PORT)]
    ipc_port: u16,
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
    Connect {
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
    },
    /// Export chats as plain text (needs Zeron running)
    ///
    /// Without --chat or --project, every chat is exported. Filters combine.
    /// Chats come out oldest first, with whole transcripts.
    Export {
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
    },
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

pub(crate) fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/".into())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Die quietly on a closed pipe (`zeron-tools export | head`) like other CLIs.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let data_dir = cli.global.data_dir.clone().unwrap_or_else(zeron::data_dir);
    let ipc_port = cli.global.ipc_port;
    match cli.command {
        Cmd::Connect {
            relay,
            token,
            notify_always,
            keep,
            no_archive,
            no_rename,
            pi,
            install,
            uninstall,
            agent,
        } => {
            if uninstall {
                match connect::uninstall()? {
                    true => println!("stopped; connect no longer runs in the background"),
                    false => println!("connect wasn't running in the background"),
                }
                return Ok(());
            }
            let path = connect::link_path(&data_dir);
            // A new link is saved once the relay accepts it, so a mistyped
            // token doesn't replace a working one.
            let (link, save_to) = match relay.zip(token) {
                Some((relay, token)) => (Some(connect::Link { relay, token }), Some(path)),
                None => (connect::Link::load(&path)?, None),
            };
            // launchd starts the agent with a bare PATH, so find pi now.
            let pi = (!no_rename)
                .then(|| -> Result<Pi> {
                    let binary = which(&pi.pi).with_context(|| {
                        format!(
                            "{} isn't on PATH; pass --pi, or --no-rename to skip titling",
                            pi.pi.display()
                        )
                    })?;
                    Ok(Pi {
                        binary,
                        model: pi.title_model,
                        thinking: pi.title_thinking,
                    })
                })
                .transpose()?;
            let upkeep = connect::Upkeep {
                keep: (!no_archive).then_some(keep),
                pi,
                memory: data_dir.join("zeron-tools-archived.json"),
            };
            if link.is_none() && !upkeep.any() {
                bail!(
                    "nothing to do: add this Mac in the phone app (it shows the command), or drop --no-archive or --no-rename"
                );
            }
            if install {
                if let (Some(link), Some(path)) = (&link, save_to) {
                    connect::verify(link).await?;
                    link.save(&path)?;
                }
                let args = agent_args(
                    cli.global.data_dir.as_deref(),
                    ipc_port,
                    notify_always,
                    &upkeep,
                );
                let log = connect::install(&args)?;
                println!(
                    "connect now runs in the background and starts at login; log: {}",
                    log.display()
                );
                if link.is_none() {
                    println!(
                        "to use Zeron from a phone, add this Mac in the app and run the command it shows"
                    );
                }
                return Ok(());
            }
            if link.is_none() {
                eprintln!("no phone link yet: add this Mac in the app to use it from a phone");
            }
            let _only = connect::single_instance(&data_dir, agent).await?;
            connect::run(connect::Opts {
                link,
                save_to,
                ipc_port,
                notify_always,
                upkeep,
                agent,
            })
            .await
        }
        Cmd::Export {
            chats,
            project,
            since,
            until,
            limit,
            skip_archived,
            format,
            reasoning,
            no_tools,
            output,
        } => {
            let opts = export::Opts {
                chats,
                project,
                since,
                until,
                limit,
                skip_archived,
                format,
                render: RenderOptions {
                    include_reasoning: reasoning,
                    include_tools: !no_tools,
                },
                output,
            };
            export::run(&Zeron::new(ipc_port), opts).await
        }
    }
}

/// The launch agent's arguments: this run's options, minus the link (saved).
fn agent_args(
    data_dir: Option<&Path>,
    ipc_port: u16,
    notify_always: bool,
    upkeep: &connect::Upkeep,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(dir) = data_dir {
        args.extend(["--data-dir".into(), dir.to_string_lossy().into_owned()]);
    }
    args.extend(["--ipc-port".into(), ipc_port.to_string()]);
    args.extend(["connect".into(), "--agent".into()]);
    if notify_always {
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

/// `program` as an absolute path: itself if it names a path, else its
/// first match on `PATH`.
fn which(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_owned());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Expand a leading `~/` to the home folder.
fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(path),
    }
}

/// Pi and chat ids are UUIDv7: the first 8 hex digits are a coarse timestamp,
/// so keep the next group too for a usable, mostly unique prefix.
fn short_id(id: &str) -> &str {
    id.get(..13).unwrap_or(id)
}

fn short_path(p: &Path) -> String {
    match p.strip_prefix(home()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_repeats_the_upkeep_options() {
        let upkeep = connect::Upkeep {
            keep: Some(5),
            pi: Some(Pi {
                binary: "/bin/pi".into(),
                model: "m".into(),
                thinking: "low".into(),
            }),
            memory: PathBuf::new(),
        };
        assert_eq!(
            agent_args(None, 1, true, &upkeep).join(" "),
            "--ipc-port 1 connect --agent --notify-always --keep 5 --pi /bin/pi --title-model m --title-thinking low"
        );
        let off = connect::Upkeep {
            keep: None,
            pi: None,
            memory: PathBuf::new(),
        };
        assert_eq!(
            agent_args(Some(Path::new("/d")), 1, false, &off).join(" "),
            "--data-dir /d --ipc-port 1 connect --agent --no-archive --no-rename"
        );
    }

    #[test]
    fn which_finds_programs_on_path() {
        assert!(which(Path::new("sh")).is_some_and(|p| p.is_absolute()));
        assert_eq!(which(Path::new("/bin/sh")), Some(PathBuf::from("/bin/sh")));
        assert_eq!(which(Path::new("surely-not-a-program")), None);
    }
}
