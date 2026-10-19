//! zeron-tools: offline maintenance for a local Zeron profile.
//!
//! Commands that write require Zeron to be quit (Cmd+Q); they take Zeron's
//! instance lock and back up `docs.sqlite3` first. Each command's logic lives
//! in its module's `run`; `sync` chains them under one lock and one backup.

mod archive;
mod delete;
mod export;
mod import;
mod refresh;
mod rename;
mod sources;
mod zeron;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, TimeZone, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use zeron_proto::HarnessId;

use crate::export::{ExportedChat, Format};
use crate::import::Projects;
use crate::rename::Pi;
use crate::sources::Source;
use crate::zeron::Profile;

#[derive(Parser)]
#[command(
    version = concat!(
        env!("CARGO_PKG_VERSION"),
        " (Zeron ",
        zeron::zeron_version!(),
        ")"
    ),
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
    /// Run even if Zeron is at a version this build doesn't target
    #[arg(long, global = true)]
    ignore_version: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Keep Zeron in sync: delete orphaned sessions, import new pi and Codex
    /// sessions, add turns made elsewhere, title untitled chats, and archive
    /// old ones
    ///
    /// Imports skip sessions whose folder is gone or excluded, and sessions
    /// active in the last hour (they may still be running). A project is
    /// created for a new folder only if it is a git repository root.
    Sync {
        /// Keep this many most recently active chats per project in the sidebar
        #[arg(long, default_value_t = 10)]
        keep: usize,
        /// Don't import sessions in these folders (Codex's scratch chats and
        /// temp folders by default)
        #[arg(
            long,
            value_name = "DIR",
            default_values = ["~/Documents/Codex", "/tmp", "/private/tmp", "/private/var/folders"]
        )]
        exclude: Vec<String>,
        /// Leave untitled chats as they are
        #[arg(long)]
        no_rename: bool,
        #[command(flatten)]
        pi: PiOpts,
        /// Codex binary (for `codex delete`)
        #[arg(long, env = "CODEX_BIN", default_value = "codex")]
        codex: PathBuf,
        #[command(flatten)]
        write: WriteOpts,
    },
    /// Add turns made outside Zeron (the pi CLI, Codex's apps) to
    /// the chats that resume those sessions
    Refresh {
        #[command(flatten)]
        write: WriteOpts,
    },
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
        /// Re-import sessions already linked to a chat: delete that chat
        /// (including anything added in Zeron since) and import again
        #[arg(long)]
        replace: bool,
        #[command(flatten)]
        write: WriteOpts,
    },
    /// Title chats with a small model through pi
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
        #[command(flatten)]
        pi: PiOpts,
        #[command(flatten)]
        write: WriteOpts,
    },
    /// Delete chats and the harness sessions they resume
    ///
    /// Pi session files go to the Trash; Codex threads are deleted with
    /// `codex delete`. A session another chat still resumes is kept.
    Delete {
        /// Chat ids or id prefixes
        #[arg(long = "chat", value_name = "ID")]
        chats: Vec<String>,
        /// Every chat whose agent never finished a turn with an answer (you
        /// interrupted it, it failed, or it never replied)
        #[arg(long)]
        unfinished: bool,
        /// Pi and Codex sessions Zeron started that no chat resumes anymore
        #[arg(long)]
        orphaned: bool,
        /// Never delete these chats or sessions (ids or id prefixes)
        #[arg(long, value_name = "ID")]
        keep: Vec<String>,
        /// Delete chats from Zeron only; keep their harness sessions
        #[arg(long)]
        zeron_only: bool,
        /// Codex binary (for `codex delete`)
        #[arg(long, env = "CODEX_BIN", default_value = "codex")]
        codex: PathBuf,
        #[command(flatten)]
        write: WriteOpts,
    },
    /// Archive older chats so the sidebar lists only recent ones
    ///
    /// Zeron's sidebar draws every unarchived chat; archived ones move to its
    /// Archived shelf (hidden unless enabled) and stay searchable and
    /// resumable. Pinned chats and chats in sidebar sections are left alone.
    Archive {
        /// Keep this many most recently active chats per project
        #[arg(long, default_value_t = 10)]
        keep: usize,
        /// Only archive chats inactive since before this local date
        /// (YYYY-MM-DD) or for this long (30d, 4w)
        #[arg(long, value_parser = export::parse_since)]
        older_than: Option<DateTime<Utc>>,
        /// Only this project (name or folder)
        #[arg(long)]
        project: Option<String>,
        #[command(flatten)]
        write: WriteOpts,
    },
    /// Export chats as plain text (works while Zeron runs)
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

#[derive(Args)]
struct WriteOpts {
    /// Show what would change without writing (works while Zeron runs)
    #[arg(long)]
    dry_run: bool,
    /// Skip the docs.sqlite3 backup
    #[arg(long)]
    no_backup: bool,
}

/// The model that writes titles.
#[derive(Args)]
struct PiOpts {
    /// Title model, as `provider/model` from `pi --list-models`
    #[arg(long, default_value = "openai-codex/gpt-6-luna")]
    model: String,
    /// Thinking level for the title model
    #[arg(long, default_value = "low")]
    thinking: String,
    /// Pi binary
    #[arg(long, env = "PI_BIN", default_value = "pi")]
    pi: PathBuf,
}

impl PiOpts {
    fn pi(self) -> Pi {
        Pi {
            binary: self.pi,
            model: self.model,
            thinking: self.thinking,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Harness {
    Pi,
    Codex,
}

impl Harness {
    fn source(self) -> Result<Source> {
        Ok(match self {
            Self::Pi => Source::pi(),
            Self::Codex => Source::codex()?,
        })
    }
}

/// What a step did: items done, and failures it reported and kept going past.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tally {
    pub done: usize,
    pub failed: usize,
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
    let failed = match cli.command {
        Cmd::Sync {
            keep,
            exclude,
            no_rename,
            pi,
            codex,
            write,
        } => {
            let mut profile = open(!write.dry_run)?;
            backup(&profile, &data_dir, &write)?;
            let exclude: Vec<PathBuf> = exclude.iter().map(|e| expand_tilde(e)).collect();
            let mut failed = 0;
            // A failing step is reported; the others still run.
            let mut step = |name: &str, result: Result<Tally>| match result {
                Ok(t) => {
                    eprintln!("{name}: {} done{}", t.done, failures(t.failed));
                    failed += t.failed;
                }
                Err(e) => {
                    eprintln!("{name} FAILED: {e:#}");
                    failed += 1;
                }
            };
            eprintln!("== Deleting orphaned sessions");
            let delete = delete::Opts {
                chats: &[],
                unfinished: false,
                orphaned: true,
                keep: &[],
                zeron_only: false,
                codex: &codex,
                dry_run: write.dry_run,
            };
            step("orphans", delete::run(&mut profile, &delete));
            for harness in [Harness::Pi, Harness::Codex] {
                let name = match harness {
                    Harness::Pi => "pi",
                    Harness::Codex => "codex",
                };
                eprintln!("== Importing new {name} sessions");
                let opts = import::Opts {
                    title: None,
                    projects: Projects::GitRoots,
                    replace: false,
                    dry_run: write.dry_run,
                };
                let result = harness.source().and_then(|source| {
                    let files = import::pending(&profile, &source, &exclude)?;
                    Ok(import::run(&mut profile, &source, &files, &opts))
                });
                step(&format!("{name} import"), result);
            }
            eprintln!("== Adding turns made outside Zeron");
            step("turns", refresh::run(&mut profile, write.dry_run));
            if !no_rename {
                eprintln!("== Titling untitled chats");
                let selection = rename::Selection {
                    chats: &[],
                    untitled: true,
                    all: false,
                };
                step(
                    "titles",
                    rename::run(&mut profile, selection, &pi.pi(), write.dry_run),
                );
            }
            eprintln!("== Archiving old chats");
            step(
                "archive",
                archive::run(&mut profile, keep, None, None, write.dry_run),
            );
            failed
        }
        Cmd::Refresh { write } => {
            let mut profile = open(!write.dry_run)?;
            backup(&profile, &data_dir, &write)?;
            refresh::run(&mut profile, write.dry_run)?.failed
        }
        Cmd::Sessions { harness, all } => {
            let profile = open(false)?;
            let source = harness.source()?;
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
            0
        }
        Cmd::Import {
            harness,
            sessions,
            title,
            create_project,
            replace,
            write,
        } => {
            if title.is_some() && sessions.len() > 1 {
                bail!("--title needs exactly one session; use `rename` for batches");
            }
            let source = harness.source()?;
            let files = resolve_sessions(&source, &sessions)?;
            let mut profile = open(!write.dry_run)?;
            backup(&profile, &data_dir, &write)?;
            let opts = import::Opts {
                title: title.as_deref(),
                projects: if create_project {
                    Projects::Always
                } else {
                    Projects::Never
                },
                replace,
                dry_run: write.dry_run,
            };
            import::run(&mut profile, &source, &files, &opts).failed
        }
        Cmd::Rename {
            chats,
            untitled,
            all,
            pi,
            write,
        } => {
            let mut profile = open(!write.dry_run)?;
            backup(&profile, &data_dir, &write)?;
            let selection = rename::Selection {
                chats: &chats,
                untitled,
                all,
            };
            rename::run(&mut profile, selection, &pi.pi(), write.dry_run)?.failed
        }
        Cmd::Delete {
            chats,
            unfinished,
            orphaned,
            keep,
            zeron_only,
            codex,
            write,
        } => {
            // Even --orphaned needs Zeron quit: a running Zeron may not have
            // saved the row of a chat it just created, so its session would
            // look orphaned.
            let mut profile = open(!write.dry_run)?;
            backup(&profile, &data_dir, &write)?;
            let opts = delete::Opts {
                chats: &chats,
                unfinished,
                orphaned,
                keep: &keep,
                zeron_only,
                codex: &codex,
                dry_run: write.dry_run,
            };
            delete::run(&mut profile, &opts)?.failed
        }
        Cmd::Archive {
            keep,
            older_than,
            project,
            write,
        } => {
            let mut profile = open(!write.dry_run)?;
            backup(&profile, &data_dir, &write)?;
            archive::run(
                &mut profile,
                keep,
                older_than,
                project.as_deref(),
                write.dry_run,
            )?
            .failed
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
            let profile = open(false)?;
            let spaces = profile.spaces()?;
            let every = profile.chats()?;
            let space = project
                .as_deref()
                .map(|p| find_space(&spaces, p))
                .transpose()?;
            let picked = chats
                .iter()
                .map(|prefix| find_chat(&every, prefix).map(|c| c.id.as_str()))
                .collect::<Result<HashSet<_>>>()?;
            let mut selected: Vec<_> = every
                .iter()
                .filter(|c| picked.is_empty() || picked.contains(c.id.as_str()))
                .filter(|c| space.is_none_or(|s| c.space_id.as_deref() == Some(s.id.as_str())))
                .filter(|c| export::active_between(c, since, until))
                .filter(|c| !(skip_archived && c.archived))
                .collect();
            selected.sort_by_key(|c| std::cmp::Reverse(export::last_activity(c)));
            selected.truncate(limit.unwrap_or(usize::MAX));
            selected.sort_by_key(|c| c.created_at);
            if let Some(dir) = &output {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            let options = zeron_mcp::RenderOptions {
                include_reasoning: reasoning,
                include_tools: !no_tools,
            };
            // A fork's copied history is left out when its source is exported too.
            let ids: HashSet<&str> = selected.iter().map(|c| c.id.as_str()).collect();
            let (mut written, mut messages) = (0, 0);
            let mut stdout = std::io::stdout().lock();
            for chat in selected {
                let space = spaces
                    .iter()
                    .find(|s| Some(&s.id) == chat.space_id.as_ref());
                let exported =
                    ExportedChat::new(chat, space, &profile.transcript(&chat.id)?, options, &ids);
                if exported.messages.is_empty() {
                    continue;
                }
                let text = exported.render(format)?;
                match &output {
                    Some(dir) => std::fs::write(dir.join(exported.file_name(format)), text)?,
                    None => {
                        use std::io::Write;
                        // Separate markdown documents with a rule.
                        if written > 0 && format == Format::Markdown {
                            stdout.write_all(b"\n---\n\n")?;
                        }
                        stdout.write_all(text.as_bytes())?;
                    }
                }
                written += 1;
                messages += exported.messages.len();
            }
            eprintln!(
                "exported {written} chat(s), {messages} message(s){}",
                output
                    .map(|d| format!(" to {}", d.display()))
                    .unwrap_or_default()
            );
            0
        }
    };
    if failed > 0 {
        bail!("{failed} failure(s); see above");
    }
    Ok(())
}

fn failures(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        format!(", {n} failed")
    }
}

/// Expand a leading `~/` to the home folder.
fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(path),
    }
}

/// Back up docs.sqlite3 before a write, unless dry-run or opted out.
fn backup(profile: &Profile, data_dir: &Path, opts: &WriteOpts) -> Result<()> {
    if opts.dry_run || opts.no_backup {
        return Ok(());
    }
    let dest = backup_dir(&data_dir.join("zeron-tools-backups"), Local::now())?;
    profile.backup(&dest)?;
    eprintln!("backup: {}", dest.display());
    Ok(())
}

/// Create a new backup folder under `root`, named by `now` to the second.
/// Runs started within the same second get a `-2`, `-3`, … suffix.
fn backup_dir(root: &Path, now: DateTime<Local>) -> Result<PathBuf> {
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let stamp = now.format("%Y%m%d-%H%M%S").to_string();
    let mut n = 1;
    loop {
        let dest = match n {
            1 => root.join(&stamp),
            _ => root.join(format!("{stamp}-{n}")),
        };
        // `create_dir` fails on an existing folder, so concurrent runs can't share one.
        match std::fs::create_dir(&dest) {
            Ok(()) => return Ok(dest),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e).with_context(|| format!("creating {}", dest.display())),
        }
    }
}

/// A harness's id as Zeron spells it (`codex`, `pi`, `claude-code`).
fn harness_name(h: HarnessId) -> String {
    serde_json::to_value(h)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{h:?}"))
}

/// The one chat whose id starts with `prefix`.
fn find_chat<'a>(chats: &'a [zeron_proto::Chat], prefix: &str) -> Result<&'a zeron_proto::Chat> {
    let hits: Vec<_> = chats.iter().filter(|c| c.id.starts_with(prefix)).collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => bail!("no chat matches {prefix}"),
        _ => bail!("{prefix} matches {} chats; use a longer prefix", hits.len()),
    }
}

/// The project named `arg` (case-insensitive) or at folder `arg`.
fn find_space<'a>(spaces: &'a [zeron_proto::Space], arg: &str) -> Result<&'a zeron_proto::Space> {
    let folder = expand_tilde(arg);
    let folder = folder.canonicalize().unwrap_or(folder);
    spaces
        .iter()
        .find(|s| s.display_name().eq_ignore_ascii_case(arg))
        .or_else(|| spaces.iter().find(|s| Path::new(&s.path) == folder))
        .with_context(|| {
            let mut names: Vec<_> = spaces.iter().map(|s| s.display_name()).collect();
            names.sort_unstable();
            format!("no project {arg:?}; projects: {}", names.join(", "))
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backups_in_the_same_second_get_their_own_folder() {
        let root = tempfile::tempdir().unwrap();
        let now = Local.with_ymd_and_hms(2026, 10, 19, 10, 49, 7).unwrap();
        let names: Vec<_> = (0..3)
            .map(|_| backup_dir(root.path(), now).unwrap())
            .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            ["20261019-104907", "20261019-104907-2", "20261019-104907-3"]
        );
    }
}
