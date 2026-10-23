//! Handles one incoming message: download, transcribe, reply.

use std::sync::Arc;

use crate::groq::Groq;
use crate::telegram::{Message, Telegram};
use anyhow::{Result, bail};

/// Bot API limit for files bots can download.
const MAX_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;
/// Telegram message limit, in UTF-16 code units.
const MAX_MESSAGE_LEN: usize = 4096;

pub struct Config {
    pub allowed_users: Vec<i64>,
}

pub struct Bot {
    pub tg: Telegram,
    pub groq: Groq,
    pub config: Config,
}

struct Media<'a> {
    file_id: &'a str,
    duration: u32,
    file_size: Option<u64>,
    ext: String,
}

impl<'a> Media<'a> {
    fn from_message(msg: &'a Message) -> Option<Self> {
        if let Some(v) = &msg.voice {
            return Some(Self {
                file_id: &v.file_id,
                duration: v.duration,
                file_size: v.file_size,
                ext: "ogg".into(),
            });
        }
        if let Some(a) = &msg.audio {
            let ext = a
                .file_name
                .as_deref()
                .and_then(|n| n.rsplit_once('.'))
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_else(|| "mp3".into());
            return Some(Self {
                file_id: &a.file_id,
                duration: a.duration,
                file_size: a.file_size,
                ext,
            });
        }
        if let Some(v) = &msg.video_note {
            return Some(Self {
                file_id: &v.file_id,
                duration: v.duration,
                file_size: v.file_size,
                ext: "mp4".into(),
            });
        }
        None
    }
}

impl Bot {
    pub async fn handle(self: Arc<Self>, msg: Message) {
        let chat_id = msg.chat.id;
        let thread_id = msg.message_thread_id;
        if let Err(err) = self.process(&msg).await {
            tracing::error!(chat_id, "failed: {err:#}");
            let text = format!("⚠️ Transcription failed: {err}");
            if let Err(e) = self
                .tg
                .send_message(chat_id, thread_id, &text, Some(msg.message_id))
                .await
            {
                tracing::error!(chat_id, "could not report error: {e:#}");
            }
        }
    }

    async fn process(&self, msg: &Message) -> Result<()> {
        let chat_id = msg.chat.id;
        let thread_id = msg.message_thread_id;
        let user_id = msg.from.as_ref().map(|u| u.id);
        let allowed = self.config.allowed_users.is_empty()
            || user_id.is_some_and(|id| self.config.allowed_users.contains(&id));

        let Some(media) = Media::from_message(msg) else {
            if msg.chat.kind == "private"
                && msg.text.as_deref().is_some_and(|t| t.starts_with("/start"))
            {
                let text = if allowed {
                    "Send me a voice message and I'll transcribe it.".to_owned()
                } else {
                    format!(
                        "This bot is private. Your user ID is {}.",
                        user_id.unwrap_or_default()
                    )
                };
                self.tg
                    .send_message(chat_id, thread_id, &text, None)
                    .await?;
            }
            return Ok(());
        };
        if !allowed {
            tracing::info!(
                chat_id,
                ?user_id,
                "ignoring message from user not in ALLOWED_USER_IDS"
            );
            return Ok(());
        }
        if media.file_size.is_some_and(|s| s > MAX_DOWNLOAD_BYTES) {
            bail!("the file is larger than the 20 MB bots are allowed to download");
        }

        // Show "typing…" while the file downloads and Groq transcribes.
        let typing = {
            let tg = self.tg.clone();
            tokio::spawn(async move {
                if let Err(e) = tg.send_chat_action(chat_id, thread_id, "typing").await {
                    tracing::warn!(chat_id, "sendChatAction failed: {e:#}");
                }
            })
        };

        let started = std::time::Instant::now();
        let file = self.tg.get_file(media.file_id).await?;
        let Some(path) = file.file_path else {
            bail!("Telegram did not return a download path");
        };
        let data = self.tg.download(&path).await?;
        let downloaded = started.elapsed();

        let mut text = self
            .groq
            .transcribe(data, &format!("audio.{}", media.ext))
            .await?;
        // A late chat action would show "typing…" again after the reply.
        let _ = typing.await;

        if text.is_empty() {
            text = "🤷 No speech detected.".into();
        }
        for (i, part) in split_message(&text, MAX_MESSAGE_LEN)
            .into_iter()
            .enumerate()
        {
            let reply_to = (i == 0).then_some(msg.message_id);
            self.tg
                .send_message(chat_id, thread_id, part, reply_to)
                .await?;
        }
        tracing::info!(
            chat_id,
            duration = media.duration,
            download_ms = downloaded.as_millis() as u64,
            total_ms = started.elapsed().as_millis() as u64,
            "transcribed"
        );
        Ok(())
    }
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Splits `text` into messages of at most `max` UTF-16 units, preferring to
/// break at whitespace.
fn split_message(text: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while utf16_len(rest) > max {
        let mut len = 0;
        let mut hard = 0;
        let mut soft = None;
        for (i, c) in rest.char_indices() {
            // Whitespace is dropped at the cut, so it may sit just past the limit.
            if c.is_whitespace() {
                soft = Some(i);
            }
            len += c.len_utf16();
            if len > max {
                break;
            }
            hard = i + c.len_utf8();
        }
        let cut = soft.filter(|&i| i > 0).unwrap_or(hard);
        out.push(rest[..cut].trim_end());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_whitespace() {
        assert_eq!(split_message("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(split_message("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(split_message("short", 4096), vec!["short"]);
        let long = "word ".repeat(2000);
        assert!(
            split_message(&long, 4096)
                .iter()
                .all(|p| utf16_len(p) <= 4096)
        );
    }
}
