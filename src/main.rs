//! zeron-tools: offline maintenance for a local Zeron profile.
//!
//! Commands that write require Zeron to be quit (Cmd+Q); they take Zeron's
//! instance lock and back up `docs.sqlite3` first.

mod delete;
mod export;
mod rename;
mod sources;
mod zeron;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, TimeZone, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::export::{ExportedChat, Format};
use crate::rename::{Codex, Excerpt};
use crate::sources::Source;
use crate::zeron::Profile;
use zeron_proto::HarnessId;

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
        /// Re-import sessions already linked to a chat: delete that chat
        /// (including anything added in Zeron since) and import again
        #[arg(long)]
        replace: bool,
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
    fn source(self) -> Result<Source> {
        Ok(match self {
            Self::Pi => Source::pi(),
            Self::Codex => Source::codex()?,
        })
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
            let mut failed = 0;
            for file in files {
                let result = source.parse(&file).and_then(|session| {
                    let linked = profile.linked_chat(&session.id)?;
                    if let Some(chat) = &linked
                        && !replace
                    {
                        bail!("already linked to chat {} (use --replace to re-import)", chat.id);
                    }
                    if write.dry_run {
                        println!(
                            "would import {}  {} messages  {}{}",
                            short_id(&session.id),
                            session.messages.len(),
                            short_path(&session.cwd),
                            linked
                                .map(|c| format!("  (replacing chat {})", short_id(&c.id)))
                                .unwrap_or_default()
                        );
                        return Ok(());
                    }
                    if let Some(chat) = linked {
                        profile.delete_chat(&chat.id)?;
                        println!("deleted chat {} (replacing it)", chat.id);
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
        Cmd::Delete {
            chats,
            unfinished,
            orphaned,
            keep,
            zeron_only,
            codex,
            write,
        } => {
            if chats.is_empty() && !unfinished && !orphaned {
                bail!("pick chats with --chat, --unfinished, or --orphaned");
            }
            // Even --orphaned needs Zeron quit: a running Zeron may not have
            // saved the row of a chat it just created, so its session would
            // look orphaned.
            let mut profile = open(!write.dry_run)?;
            let every = profile.chats()?;
            let spaces = profile.spaces()?;
            let kept = |id: &str| keep.iter().any(|k| id.starts_with(k.as_str()));
            // Chosen chats with why, in sidebar-independent creation order.
            let mut doomed: Vec<(&zeron_proto::Chat, String)> = vec![];
            for prefix in &chats {
                let chat = find_chat(&every, prefix)?;
                doomed.push((chat, "picked".into()));
            }
            if unfinished {
                for chat in &every {
                    if doomed.iter().any(|(c, _)| c.id == chat.id) {
                        continue;
                    }
                    if let Some(u) = delete::unfinished(&profile.transcript(&chat.id)?) {
                        let prompts = if u.prompts == 1 { "prompt" } else { "prompts" };
                        doomed.push((chat, format!("{} {prompts}, {}", u.prompts, u.outcome)));
                    }
                }
            }
            doomed.retain(|(c, _)| {
                !kept(&c.id) && !c.harness_session_id.as_deref().is_some_and(kept)
            });
            doomed.sort_by_key(|(c, _)| c.created_at);
            let doomed_ids: HashSet<&str> = doomed.iter().map(|(c, _)| c.id.as_str()).collect();
            // Sessions chats outside this run still resume: never delete those.
            let resumed: HashSet<&str> = every
                .iter()
                .filter(|c| !doomed_ids.contains(c.id.as_str()))
                .filter_map(|c| c.harness_session_id.as_deref())
                .collect();
            // Open only the stores this run touches (Codex lists every rollout).
            let mut sources: HashMap<HarnessId, Source> = HashMap::new();
            for (id, harness) in [
                (HarnessId::Pi, Harness::Pi),
                (HarnessId::Codex, Harness::Codex),
            ] {
                let used = doomed
                    .iter()
                    .any(|(c, _)| c.config.as_ref().is_some_and(|cfg| cfg.harness == id));
                if orphaned || (used && !zeron_only) {
                    sources.insert(id, harness.source()?);
                }
            }
            backup(&profile, &data_dir, &write)?;
            let verb = if write.dry_run {
                "would delete"
            } else {
                "deleted"
            };
            let mut failed = 0;
            for (chat, why) in &doomed {
                let project = spaces
                    .iter()
                    .find(|s| Some(&s.id) == chat.space_id.as_ref())
                    .map_or("no project", |s| s.display_name());
                println!(
                    "{verb} chat {}  {why}  {project}  {}",
                    short_id(&chat.id),
                    one_line(chat.title.as_deref().unwrap_or("Untitled"), 50)
                );
                if !write.dry_run
                    && let Err(e) = profile.delete_chat(&chat.id)
                {
                    failed += 1;
                    eprintln!("FAIL chat {}: {e:#}", chat.id);
                    continue;
                }
                let harness = chat.config.as_ref().map(|c| c.harness);
                let (Some(harness), Some(sid)) = (harness, chat.harness_session_id.as_deref())
                else {
                    continue;
                };
                if zeron_only {
                    continue;
                }
                if resumed.contains(sid) {
                    println!("  kept session {} (another chat resumes it)", short_id(sid));
                    continue;
                }
                let Some(src) = sources.get(&harness) else {
                    println!(
                        "  kept {} session {} (not supported)",
                        harness_name(harness),
                        short_id(sid)
                    );
                    continue;
                };
                let session = format!("{} session {}", harness_name(harness), short_id(sid));
                if write.dry_run {
                    println!("  {verb} {session}");
                    continue;
                }
                match src.delete_session(sid, &codex) {
                    Ok(true) => println!("  {verb} {session}"),
                    Ok(false) => println!("  {session} was already gone"),
                    Err(e) => {
                        failed += 1;
                        eprintln!("FAIL session {sid}: {e:#}");
                    }
                }
            }
            if orphaned {
                let linked: HashSet<&str> = every
                    .iter()
                    .filter_map(|c| c.harness_session_id.as_deref())
                    .collect();
                for harness in [HarnessId::Pi, HarnessId::Codex] {
                    let Some(src) = sources.get(&harness) else {
                        continue;
                    };
                    for file in src.files()? {
                        // Skip linked sessions before reading them: files can be huge.
                        if src
                            .file_id(&file)
                            .is_some_and(|id| linked.contains(id) || kept(id))
                        {
                            continue;
                        }
                        let s = match src.summarize(&file) {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("skip {}: {e:#}", file.display());
                                continue;
                            }
                        };
                        if !s.started_by_zeron || linked.contains(s.id.as_str()) || kept(&s.id) {
                            continue;
                        }
                        println!(
                            "{verb} {} session {}  orphaned  {}  {}",
                            harness_name(harness),
                            short_id(&s.id),
                            short_path(&s.cwd),
                            one_line(s.name.as_deref().unwrap_or(&s.first_user), 50)
                        );
                        if !write.dry_run
                            && let Err(e) = src.delete_session(&s.id, &codex)
                        {
                            failed += 1;
                            eprintln!("FAIL session {}: {e:#}", s.id);
                        }
                    }
                }
            }
            if failed > 0 {
                bail!("{failed} deletion(s) failed");
            }
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
            let (mut written, mut messages) = (0, 0);
            let mut stdout = std::io::stdout().lock();
            for chat in selected {
                let space = spaces
                    .iter()
                    .find(|s| Some(&s.id) == chat.space_id.as_ref());
                let exported =
                    ExportedChat::new(chat, space, &profile.transcript(&chat.id)?, options);
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
                picked.insert(find_chat(&every, prefix)?.id.clone());
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
    let folder = match arg.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(arg),
    };
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
