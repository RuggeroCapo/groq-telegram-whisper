//! Handles one incoming message: download, transcribe, stream the draft, reply.

use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::audio::{self, Chunk};
use crate::groq::Groq;
use crate::telegram::{Message, Telegram};

/// Bot API limit for files bots can download.
const MAX_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;
/// Telegram message limit, in UTF-16 code units.
const MAX_MESSAGE_LEN: usize = 4096;

pub struct Config {
    pub allowed_users: Vec<i64>,
    pub chunk_secs: f64,
    pub ffmpeg: bool,
}

pub struct Bot {
    pub tg: Telegram,
    pub groq: Groq,
    pub config: Config,
    /// Caps concurrent Groq requests across all chats.
    pub groq_slots: Arc<Semaphore>,
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

        // Drafts only work in private chats. Elsewhere, show "typing…".
        let draft = (msg.chat.kind == "private").then(|| Draft {
            tg: self.tg.clone(),
            chat_id,
            thread_id,
            id: msg.message_id,
        });

        // Show feedback immediately, in parallel with the download.
        let placeholder = {
            let tg = self.tg.clone();
            let draft = draft.clone();
            tokio::spawn(async move {
                let res = match &draft {
                    Some(d) => d.show("").await,
                    None => tg
                        .send_chat_action(chat_id, thread_id, "typing")
                        .await
                        .map(|_| ()),
                };
                if let Err(e) = res {
                    tracing::warn!(chat_id, "placeholder failed: {e:#}");
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

        let chunks = self.chunk(data, &media).await;
        let n = chunks.len();

        let mut tasks = JoinSet::new();
        for (i, chunk) in chunks.into_iter().enumerate() {
            let groq = self.groq.clone();
            let slots = self.groq_slots.clone();
            tasks.spawn(async move {
                let _permit = slots.acquire_owned().await?;
                let text = groq.transcribe(chunk.data, &chunk.file_name).await?;
                anyhow::Ok((i, text))
            });
        }

        // Drafts must not land after the final message, so wait for the placeholder.
        let _ = placeholder.await;

        let mut parts: Vec<Option<String>> = vec![None; n];
        let mut shown = 0;
        while let Some(joined) = tasks.join_next().await {
            let (i, text) = joined??;
            parts[i] = Some(text);
            let ready = parts.iter().take_while(|p| p.is_some()).count();
            if let Some(d) = &draft
                && ready > shown
                && ready < n
            {
                shown = ready;
                let text = join(&parts[..ready]);
                if let Err(e) = d.show(&tail(&text, MAX_MESSAGE_LEN - 2)).await {
                    tracing::warn!(chat_id, "draft update failed: {e:#}");
                }
            }
        }

        let mut text = join(&parts);
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
            chunks = n,
            download_ms = downloaded.as_millis() as u64,
            total_ms = started.elapsed().as_millis() as u64,
            "transcribed"
        );
        Ok(())
    }

    async fn chunk(&self, data: Vec<u8>, media: &Media<'_>) -> Vec<Chunk> {
        let duration = f64::from(media.duration);
        let chunk_secs = self.config.chunk_secs;
        if self.config.ffmpeg && chunk_secs > 0.0 && duration > chunk_secs * 1.5 {
            match audio::split(&data, &media.ext, duration, chunk_secs).await {
                Ok(chunks) => return chunks,
                Err(e) => tracing::warn!("splitting failed, sending whole file: {e:#}"),
            }
        }
        vec![Chunk {
            data,
            file_name: format!("audio.{}", media.ext),
        }]
    }
}

#[derive(Clone)]
struct Draft {
    tg: Telegram,
    chat_id: i64,
    thread_id: Option<i64>,
    id: i64,
}

impl Draft {
    async fn show(&self, text: &str) -> Result<()> {
        self.tg
            .send_message_draft(self.chat_id, self.thread_id, self.id, text)
            .await?;
        Ok(())
    }
}

fn join(parts: &[Option<String>]) -> String {
    parts
        .iter()
        .flatten()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Last `max` UTF-16 units of `text`, prefixed with "…" when truncated, so a
/// long draft keeps showing the newest words.
fn tail(text: &str, max: usize) -> String {
    if utf16_len(text) <= max {
        return text.to_owned();
    }
    let mut len = 0;
    let start = text
        .char_indices()
        .rev()
        .find(|(_, c)| {
            len += c.len_utf16();
            len > max - 1
        })
        .map_or(0, |(i, c)| i + c.len_utf8());
    format!("…{}", &text[start..])
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
    fn joins_non_empty_parts() {
        let parts = [
            Some(" hello ".into()),
            Some(String::new()),
            Some("world".into()),
        ];
        assert_eq!(join(&parts), "hello world");
    }

    #[test]
    fn tail_keeps_newest_text() {
        assert_eq!(tail("abc", 3), "abc");
        assert_eq!(tail("abcdef", 4), "…def");
        assert_eq!(utf16_len(&tail("😀😀😀😀", 5)), 5);
    }

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
