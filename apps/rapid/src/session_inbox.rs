//! The session inbox: `rapid send` hands a message to a live session.
//!
//! A shell script, a cron job, a monitor — anything outside the TUI — can
//! drop a message into `<project>/.rapidlm/session-inbox/<session>.jsonl`
//! and the running session adopts it within a second, through the same
//! durable queue typed messages use (`queue_message`: journaled
//! `message.queued`, listed by `/queue`, run after the current turn). A
//! session that is not running adopts what waits for it when it next
//! starts. The file is the whole protocol: one JSON object per line, the
//! session's own file, cleared when the session adopts its contents.

use std::path::{Path, PathBuf};

/// Messages one session may have waiting; a fuller box refuses the send.
pub const MAX_INBOX_MESSAGES: usize = 16;
/// Longest one message may be — the same bound a queued message has.
pub const MAX_INBOX_MESSAGE_BYTES: usize = 16 * 1024;
/// Bound on one session's whole inbox file.
pub const MAX_INBOX_FILE_BYTES: usize = 256 * 1024;

/// The session's inbox file under the project's marker directory.
pub fn inbox_path(marker: &Path, session: &str) -> PathBuf {
    marker
        .join("session-inbox")
        .join(format!("{session}.jsonl"))
}

/// Whether `session` can name a file: session ids are the ledger's own
/// (`s`-prefixed hex today), and whatever shape they have, a path
/// separator or a dot-prefixed name never appears in one.
fn valid_session_id(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 64
        && session
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// Append `text` to the session's inbox. Returns how many messages now
/// wait. Refused typed: an invalid session id, an oversized message, or a
/// full box — a sender is told, never silently dropped.
pub fn append(marker: &Path, session: &str, text: &str) -> Result<usize, String> {
    if !valid_session_id(session) {
        return Err("not a session id".to_owned());
    }
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_INBOX_MESSAGE_BYTES {
        return Err("the message is empty or over the 16 KiB bound".to_owned());
    }
    let path = inbox_path(marker, session);
    let pending = pending(marker, session)?;
    if pending.len() >= MAX_INBOX_MESSAGES {
        return Err(format!(
            "the session's inbox is full ({MAX_INBOX_MESSAGES} waiting); it adopts them the next time it runs"
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("the inbox could not be created: {err}"))?;
    }
    let at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let line = serde_json::json!({
        "id": format!("in-{}", pending.len() + 1),
        "at": at,
        "text": text,
    });
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| format!("the inbox could not be written: {err}"))?;
    writeln!(file, "{line}").map_err(|err| format!("the inbox could not be written: {err}"))?;
    Ok(pending.len() + 1)
}

/// The messages waiting for `session`, oldest first — a read without the
/// clear, so a sender can be told what it lands on.
pub fn pending(marker: &Path, session: &str) -> Result<Vec<String>, String> {
    if !valid_session_id(session) {
        return Err("not a session id".to_owned());
    }
    let path = inbox_path(marker, session);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    if text.len() > MAX_INBOX_FILE_BYTES {
        // A box past its bound is drained whole rather than trusted: the
        // session adopts what parses, and the file starts over.
        return Ok(Vec::new());
    }
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| {
            value
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect())
}

/// Adopt everything waiting for `session` and clear the file. What the
/// session takes it owns: the messages move into its durable queue, so
/// clearing here can never lose one a running session was about to run.
pub fn take(marker: &Path, session: &str) -> Vec<String> {
    let messages = pending(marker, session).unwrap_or_default();
    if messages.is_empty() {
        let _ = std::fs::remove_file(inbox_path(marker, session));
        return messages;
    }
    // Clear by truncation in place: a sender appending between the read
    // and the clear is honored on the next poll rather than lost.
    let path = inbox_path(marker, session);
    if let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
    {
        drop(file);
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rapidlm-inbox-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn send_appends_and_the_session_adopts_in_order() {
        let marker = marker("adopt");
        assert_eq!(
            append(&marker, "s-abc", "first message").expect("append"),
            1
        );
        assert_eq!(
            append(&marker, "s-abc", "second message").expect("append"),
            2
        );
        assert_eq!(
            pending(&marker, "s-abc").expect("pending"),
            vec!["first message".to_owned(), "second message".to_owned()],
            "oldest first"
        );

        let taken = take(&marker, "s-abc");
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0], "first message");
        // Adopted means cleared: the next take is empty and the file is
        // gone, not a zero-byte husk.
        assert!(take(&marker, "s-abc").is_empty());
        assert!(!inbox_path(&marker, "s-abc").exists());
        // Another session's box is untouched.
        assert!(pending(&marker, "s-other").expect("pending").is_empty());

        // Bad sends are refused typed, never silently dropped: a path
        // cannot name a session, and an empty message is not one.
        assert!(append(&marker, "../escape", "x").is_err());
        assert!(append(&marker, "s-abc", "   ").is_err());
        std::fs::remove_dir_all(&marker).ok();
    }

    #[test]
    fn a_full_box_refuses_the_next_send() {
        let marker = marker("full");
        for index in 0..MAX_INBOX_MESSAGES {
            append(&marker, "s-full", &format!("message {index}")).expect("fills");
        }
        let err = append(&marker, "s-full", "one too many").expect_err("full");
        assert!(err.contains("inbox is full"), "{err}");
        // The bound counts messages, not bytes; the byte bound is the
        // message's own.
        assert!(append(&marker, "s-full2", &"x".repeat(MAX_INBOX_MESSAGE_BYTES + 1)).is_err());
        std::fs::remove_dir_all(&marker).ok();
    }
}
