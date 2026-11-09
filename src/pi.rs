//! A small model through Pi, one-shot (`pi -p`) with nothing loaded but the
//! model: it titles chats ([`crate::rename`]) and cleans up the phone's
//! dictation ([`crate::connect`]).

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// The lines of pi's stderr reported when it fails.
const STDERR_TAIL: usize = 15;

/// How to call Pi: no session file, and no tools, extensions, skills, context
/// files, or MCP, so a call costs a few hundred tokens instead of a full
/// agent prompt.
#[derive(Debug, Clone)]
pub struct Pi {
    pub binary: PathBuf,
    /// A model id (pi picks the provider) or `provider/model`, as
    /// `pi --list-models` shows.
    pub model: String,
    pub thinking: String,
}

impl Pi {
    /// Send `input` to the model under the system prompt `instructions`, at
    /// thinking level `thinking`, and return its answer.
    pub async fn ask(&self, thinking: &str, instructions: &str, input: &str) -> Result<String> {
        // Run outside any project so Pi can't pick up project-local settings.
        let mut child = Command::new(&self.binary)
            .args(["--model", &self.model, "--thinking", thinking])
            .args([
                "--no-session",
                "--no-tools",
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-themes",
                "--no-context-files",
                "--no-mcp",
                "--no-approve",
                "--offline",
                "--system-prompt",
                instructions,
                "-p",
            ])
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A caller that gives up stops the model too.
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("running {}", self.binary.display()))?;
        let mut stdin = child.stdin.take().context("pi stdin")?;
        stdin.write_all(input.as_bytes()).await?;
        drop(stdin);
        let run = child.wait_with_output().await?;
        if !run.status.success() {
            let stderr = String::from_utf8_lossy(&run.stderr);
            let lines: Vec<&str> = stderr.lines().collect();
            let tail = lines[lines.len().saturating_sub(STDERR_TAIL)..].join("\n");
            bail!("pi exited with {}:\n{tail}", run.status);
        }
        Ok(String::from_utf8_lossy(&run.stdout).into_owned())
    }
}
