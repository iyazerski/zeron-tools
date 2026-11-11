//! What a chat's latest turn changed, as the Changes pane's "Latest turn"
//! scope shows it: the whole patch for the app's diff page, and its counts
//! for the chat screen.

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use super::tools::cwd_of;
use crate::zeron::types::{Chat, CheckoutDiff, Space};
use crate::zeron::{Zeron, space_of};

/// The `turn_changes` arguments.
#[derive(Debug, Deserialize)]
pub struct TurnArgs {
    /// The chat's id or id prefix.
    chat: String,
}

/// Return `{patch, files, additions, deletions, truncated}` for the chat's
/// latest turn; empty when Zeron recorded no turn of it.
pub async fn turn(zeron: &Zeron, args: TurnArgs) -> Result<Value> {
    let chat = zeron.chat(&args.chat).await?;
    let spaces = zeron.spaces().await?;
    let diff = latest(zeron, &chat, &spaces).await?;
    Ok(json!(diff.unwrap_or_default()))
}

/// The chat's latest turn diff; `None` for a chat without a folder, or one
/// Zeron recorded no turn of.
async fn latest(zeron: &Zeron, chat: &Chat, spaces: &[Space]) -> Result<Option<CheckoutDiff>> {
    match cwd_of(chat, space_of(chat, spaces)) {
        Some(cwd) => zeron.turn_diff(cwd, &chat.id).await,
        None => Ok(None),
    }
}

/// `{files, additions, deletions}` of the chat's latest turn, or `None` when
/// it changed nothing.
pub(super) async fn counts(zeron: &Zeron, chat: &Chat, spaces: &[Space]) -> Result<Option<Value>> {
    let diff = latest(zeron, chat, spaces).await?;
    Ok(diff.filter(|d| !d.files.is_empty()).map(
        |d| json!({ "files": d.files.len(), "additions": d.additions, "deletions": d.deletions }),
    ))
}
