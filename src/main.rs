//! zeron-tools: offline maintenance for a local Zeron profile.
//!
//! Commands that write require Zeron to be quit (Cmd+Q); they take Zeron's
//! instance lock and back up `docs.sqlite3` first.

mod rename;
mod sources;
mod zeron;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use chrono::{Local, TimeZone};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::rename::{Codex, Excerpt};
use crate::sources::Source;
use crate::zeron::Profile;

#[derive(Parser)]
#[command(
    version = concat!(env!("CARGO_PKG_VERSION"), " (Zeron ", zeron::zeron_version!(), ")"),
    about,
    long_about = None
)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Args)]
struct Global {
    /// Zeron data dir [default: $ZERON_DATA_DIR or ~/.zeron]
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Profile to operate on (Zeron's offline profile is `local`)
    #[arg(long, global = true, default_value = "local")]
    profile: String,
    /// Run even if Zeron last booted a version this build doesn't target
    #[arg(long, global = true)]
    ignore_version: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// List harness sessions not yet in Zeron (started by other tools)
    Sessions {
        #[arg(value_enum)]
        harness: Harness,
        /// Also list sessions linked to a chat, and hidden ones (empty,
        /// subagents, scripted runs, Zeron's own leftovers)
        #[arg(long)]
        all: bool,
    },
    /// Import harness sessions as Zeron chats that resume the original session
    Import {
        #[arg(value_enum)]
        harness: Harness,
        /// Session files, or session id prefixes as shown by `sessions`
        #[arg(required = true)]
        sessions: Vec<String>,
        /// Chat title (only with a single session; default: the session's own name)
        #[arg(long)]
        title: Option<String>,
        /// Create a Zeron project for a cwd that has none (else: "No project")
        #[arg(long)]
        create_project: bool,
        #[command(flatten)]
        write: WriteOpts,
    },
    /// Retitle chats with a Codex model
    Rename {
        /// Chat ids or id prefixes
        #[arg(long = "chat", value_name = "ID")]
        chats: Vec<String>,
        /// Every chat without a title
        #[arg(long)]
        untitled: bool,
        /// Every chat (archived ones too)
        #[arg(long, conflicts_with_all = ["chats", "untitled"])]
        all: bool,
        #[arg(long, default_value = "gpt-6-luna")]
        model: String,
        #[arg(long, default_value = "low")]
        reasoning: String,
        /// Codex binary
        #[arg(long, env = "CODEX_BIN", default_value = "codex")]
        codex: PathBuf,
        #[command(flatten)]
        write: WriteOpts,
    },
}

#[derive(Args)]
struct WriteOpts {
    /// Show what would change without writing (works while Zeron runs)
    #[arg(long)]
    dry_run: bool,
    /// Skip the docs.sqlite3 backup
    #[arg(long)]
    no_backup: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Harness {
    Pi,
    Codex,
}

impl Harness {
    fn source(self) -> Source {
        match self {
            Self::Pi => Source::pi(),
            Self::Codex => Source::codex(),
        }
    }
}

pub(crate) fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/".into())
}

// zeron-sync's store runs SQLite through tokio's blocking pool.
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Die quietly on a closed pipe (`zeron-tools sessions | head`) like other CLIs.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let data_dir = cli.global.data_dir.clone().unwrap_or_else(zeron::data_dir);
    let profile_dir = data_dir.join("profiles").join(&cli.global.profile);
    let open =
        |write: bool| Profile::open(&data_dir, &profile_dir, write, cli.global.ignore_version);
    match cli.command {
        Cmd::Sessions { harness, all } => {
            let profile = open(false)?;
            let source = harness.source();
            let linked: HashMap<String, String> = profile
                .chats()?
                .into_iter()
                .filter_map(|c| Some((c.harness_session_id?, c.id)))
                .collect();
            for file in source.files()? {
                // Skip linked sessions before reading them: files can be huge.
                let chat = source.file_id(&file).and_then(|id| linked.get(id));
                if chat.is_some() && !all {
                    continue;
                }
                let s = match source.summarize(&file) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("skip {}: {e:#}", file.display());
                        continue;
                    }
                };
                let mut tags = vec![];
                match chat.or_else(|| linked.get(&s.id)) {
                    Some(chat) => tags.push(format!("chat {}", short_id(chat))),
                    None => tags.extend(s.hidden_reason.clone()),
                }
                if !tags.is_empty() && !all {
                    continue;
                }
                if s.archived {
                    tags.push("archived".into());
                }
                if !s.cwd.is_dir() {
                    tags.push("cwd missing".into());
                }
                println!(
                    "{}  {}  {:>3} prompts  {}  {}{}",
                    short_id(&s.id),
                    date(s.created_ms),
                    s.user_messages,
                    short_path(&s.cwd),
                    one_line(s.name.as_deref().unwrap_or(&s.first_user), 70),
                    tags.iter().map(|t| format!("  [{t}]")).collect::<String>(),
                );
            }
        }
        Cmd::Import {
            harness,
            sessions,
            title,
            create_project,
            write,
        } => {
            if title.is_some() && sessions.len() > 1 {
                bail!("--title needs exactly one session; use `rename` for batches");
            }
            let source = harness.source();
            let files = resolve_sessions(&source, &sessions)?;
            let mut profile = open(!write.dry_run)?;
            backup(&profile, &data_dir, &write)?;
            let mut failed = 0;
            for file in files {
                let result = source.parse(&file).and_then(|session| {
                    if write.dry_run {
                        if let Some(chat) = profile.linked_chat(&session.id)? {
                            bail!("already linked to chat {}", chat.id);
                        }
                        println!(
                            "would import {}  {} messages  {}",
                            short_id(&session.id),
                            session.messages.len(),
                            short_path(&session.cwd)
                        );
                        return Ok(());
                    }
                    let done = profile.import(&session, title.as_deref(), create_project)?;
                    // Persist per session so a later failure keeps earlier imports.
                    profile.save()?;
                    println!(
                        "imported {} -> chat {}  {} messages  {}{}",
                        short_id(&session.id),
                        done.chat_id,
                        done.messages,
                        short_path(&session.cwd),
                        match (&done.space_id, done.created_space) {
                            (_, true) => " (new project)",
                            (None, _) => " (no project)",
                            _ => "",
                        }
                    );
                    if done.cwd_missing {
                        println!("  note: {} is missing; the chat can't resume until it exists", short_path(&session.cwd));
                    }
                    if let Some(model) = done.dropped_model {
                        println!("  note: Zeron no longer offers {model}; the chat uses Zeron's default model");
                    }
                    Ok(())
                });
                if let Err(e) = result {
                    failed += 1;
                    eprintln!("FAIL {}: {e:#}", file.display());
                }
            }
            if failed > 0 {
                bail!("{failed} session(s) failed");
            }
        }
        Cmd::Rename {
            chats,
            untitled,
            all,
            model,
            reasoning,
            codex,
            write,
        } => {
            if chats.is_empty() && !untitled && !all {
                bail!("pick chats with --chat, --untitled, or --all");
            }
            let mut profile = open(!write.dry_run)?;
            let spaces = profile.spaces()?;
            let mut picked = HashSet::new();
            let every = profile.chats()?;
            for prefix in &chats {
                let hits: Vec<_> = every
                    .iter()
                    .filter(|c| c.id.starts_with(prefix.as_str()))
                    .collect();
                match hits.as_slice() {
                    [one] => picked.insert(one.id.clone()),
                    [] => bail!("no chat matches {prefix}"),
                    _ => bail!("{prefix} matches {} chats; use a longer prefix", hits.len()),
                };
            }
            let selected: Vec<_> = every
                .iter()
                .filter(|c| all || picked.contains(&c.id) || (untitled && c.title.is_none()))
                .collect();
            let mut excerpts = vec![];
            for chat in &selected {
                let project = spaces
                    .iter()
                    .find(|s| Some(&s.id) == chat.space_id.as_ref())
                    .map(|s| s.display_name().to_owned());
                match Excerpt::from_transcript(&chat.id, project, &profile.transcript(&chat.id)?) {
                    Some(e) => excerpts.push(e),
                    None => eprintln!("skip {}: no user message", short_id(&chat.id)),
                }
            }
            if excerpts.is_empty() {
                println!("nothing to rename");
                return Ok(());
            }
            eprintln!(
                "asking {model} ({reasoning}) for {} titles…",
                excerpts.len()
            );
            let titles = Codex {
                binary: codex,
                model,
                reasoning,
            }
            .titles(&excerpts)?;
            backup(&profile, &data_dir, &write)?;
            for t in &titles {
                let old = selected
                    .iter()
                    .find(|c| c.id == t.id)
                    .and_then(|c| c.title.clone())
                    .unwrap_or_default();
                println!(
                    "{}  {}  ->  {}",
                    short_id(&t.id),
                    one_line(&old, 40),
                    t.title
                );
                if !write.dry_run {
                    profile.rename(&t.id, &t.title)?;
                }
            }
            profile.save()?;
            let missing = excerpts.len() - titles.len();
            if missing > 0 {
                eprintln!("{missing} chat(s) got no title");
            }
        }
    }
    Ok(())
}

/// Back up docs.sqlite3 before a write, unless dry-run or opted out.
fn backup(profile: &Profile, data_dir: &Path, opts: &WriteOpts) -> Result<()> {
    if opts.dry_run || opts.no_backup {
        return Ok(());
    }
    let dest = data_dir
        .join("zeron-tools-backups")
        .join(Local::now().format("%Y%m%d-%H%M%S").to_string());
    eprintln!("backup: {}", profile.backup(&dest)?.display());
    Ok(())
}

/// Accept session files or id prefixes; refuse ambiguous prefixes.
fn resolve_sessions(source: &Source, args: &[String]) -> Result<Vec<PathBuf>> {
    let mut files = None;
    let mut out = vec![];
    for arg in args {
        let path = Path::new(arg);
        if path.is_file() {
            out.push(path.to_path_buf());
            continue;
        }
        if files.is_none() {
            files = Some(source.files()?);
        }
        let hits: Vec<_> = files
            .iter()
            .flatten()
            .filter(|f| {
                source
                    .file_id(f)
                    .is_some_and(|id| id.starts_with(arg.as_str()))
            })
            .collect();
        match hits.as_slice() {
            [one] => out.push((*one).clone()),
            [] => bail!("no session file or id matches {arg}"),
            _ => bail!("{arg} matches {} sessions; use a longer prefix", hits.len()),
        }
    }
    Ok(out)
}

/// Pi and chat ids are UUIDv7: the first 8 hex digits are a coarse timestamp,
/// so keep the next group too for a usable, mostly unique prefix.
fn short_id(id: &str) -> &str {
    id.get(..13).unwrap_or(id)
}

fn date(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
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
