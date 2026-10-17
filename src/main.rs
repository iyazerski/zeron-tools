//! zeron-tools: offline maintenance for a local Zeron profile.
//!
//! Commands that write require Zeron to be quit (Cmd+Q); they take Zeron's
//! instance lock and back up `docs.sqlite3` first.

mod rename;
mod sources;
mod zeron;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use chrono::{Local, TimeZone};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::rename::{Codex, Excerpt};
use crate::sources::pi;
use crate::zeron::Profile;

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
        #[arg(value_enum, default_value = "pi")]
        harness: Harness,
        /// Also list sessions linked to a chat, and ones Zeron started itself
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
}

pub(crate) fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
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
    let open = |write: bool| {
        Profile::open(&data_dir, &profile_dir, write, cli.global.ignore_version)
    };
    match cli.command {
        Cmd::Sessions { harness: Harness::Pi, all } => {
            let profile = open(false)?;
            let agent_dir = pi::agent_dir();
            for file in pi::session_files(&agent_dir)? {
                let s = match pi::summarize(&file) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("skip {}: {e:#}", file.display());
                        continue;
                    }
                };
                let status = match profile.linked_chat(&s.id)? {
                    Some(chat) => format!("  [chat {}]", short_id(&chat.id)),
                    None if pi::started_by_zeron(&agent_dir, &s.id) => "  [zeron leftover]".into(),
                    None => String::new(),
                };
                if !status.is_empty() && !all {
                    continue;
                }
                println!(
                    "{}  {}  {:>3} prompts  {}  {}{}",
                    short_id(&s.id),
                    date(s.created_ms),
                    s.user_messages,
                    short_path(&s.cwd),
                    one_line(&s.first_user, 70),
                    status,
                );
            }
        }
        Cmd::Import { harness: Harness::Pi, sessions, title, create_project, write } => {
            if title.is_some() && sessions.len() > 1 {
                bail!("--title needs exactly one session; use `rename` for batches");
            }
            let files = resolve_pi_sessions(&sessions)?;
            let mut profile = open(!write.dry_run)?;
            backup(&profile, &data_dir, &write)?;
            let mut failed = 0;
            for file in files {
                let result = pi::parse(&file).and_then(|session| {
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
        Cmd::Rename { chats, untitled, all, model, reasoning, codex, write } => {
            if chats.is_empty() && !untitled && !all {
                bail!("pick chats with --chat, --untitled, or --all");
            }
            let mut profile = open(!write.dry_run)?;
            let spaces = profile.spaces()?;
            let mut picked = HashSet::new();
            let every = profile.chats()?;
            for prefix in &chats {
                let hits: Vec<_> = every.iter().filter(|c| c.id.starts_with(prefix.as_str())).collect();
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
            eprintln!("asking {model} ({reasoning}) for {} titles…", excerpts.len());
            let titles = Codex { binary: codex, model, reasoning }.titles(&excerpts)?;
            backup(&profile, &data_dir, &write)?;
            for t in &titles {
                let old = selected
                    .iter()
                    .find(|c| c.id == t.id)
                    .and_then(|c| c.title.clone())
                    .unwrap_or_default();
                println!("{}  {}  ->  {}", short_id(&t.id), one_line(&old, 40), t.title);
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
fn resolve_pi_sessions(args: &[String]) -> Result<Vec<PathBuf>> {
    let mut files = None;
    let mut out = vec![];
    for arg in args {
        let path = Path::new(arg);
        if path.is_file() {
            out.push(path.to_path_buf());
            continue;
        }
        if files.is_none() {
            files = Some(pi::session_files(&pi::agent_dir())?);
        }
        let hits: Vec<_> = files
            .iter()
            .flatten()
            .filter(|f| {
                f.file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.rsplit('_').next())
                    .is_some_and(|id| id.starts_with(arg.as_str()))
            })
            .collect();
        match hits.as_slice() {
            [one] => out.push((*one).clone()),
            [] => bail!("no Pi session file or id matches {arg}"),
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

