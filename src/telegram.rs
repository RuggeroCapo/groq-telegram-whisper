//! Minimal Telegram Bot API client: only the methods this bot needs.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

const API: &str = "https://api.telegram.org";

#[derive(Clone)]
pub struct Telegram {
    http: Client,
    token: String,
}

#[derive(Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub message_thread_id: Option<i64>,
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
    pub voice: Option<Voice>,
    pub audio: Option<Audio>,
    pub video_note: Option<VideoNote>,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct Voice {
    pub file_id: String,
    pub duration: u32,
    pub file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Audio {
    pub file_id: String,
    pub duration: u32,
    pub file_name: Option<String>,
    pub file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct VideoNote {
    pub file_id: String,
    pub duration: u32,
    pub file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct File {
    pub file_path: Option<String>,
}

#[derive(Serialize)]
struct ReplyParameters {
    message_id: i64,
    allow_sending_without_reply: bool,
}

impl Telegram {
    pub fn new(http: Client, token: String) -> Self {
        Self { http, token }
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> Result<T> {
        let url = format!("{API}/bot{}/{method}", self.token);
        let resp: ApiResponse<T> = self
            .http
            .post(url)
            .json(body)
            .timeout(timeout)
            .send()
            .await
            .with_context(|| format!("{method}: request failed"))?
            .json()
            .await
            .with_context(|| format!("{method}: invalid response"))?;
        match (resp.ok, resp.result) {
            (true, Some(result)) => Ok(result),
            _ => bail!(
                "{method}: {}",
                resp.description.unwrap_or_else(|| "unknown error".into())
            ),
        }
    }

    pub async fn delete_webhook(&self) -> Result<bool> {
        self.call("deleteWebhook", &json!({}), Duration::from_secs(10))
            .await
    }

    pub async fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Update>> {
        self.call(
            "getUpdates",
            &json!({
                "offset": offset,
                "timeout": timeout_secs,
                "allowed_updates": ["message"],
            }),
            Duration::from_secs(timeout_secs + 10),
        )
        .await
    }

    pub async fn send_message(
        &self,
        chat_id: i64,
        thread_id: Option<i64>,
        text: &str,
        reply_to: Option<i64>,
    ) -> Result<serde_json::Value> {
        let mut body = json!({ "chat_id": chat_id, "text": text });
        if let Some(thread_id) = thread_id {
            body["message_thread_id"] = thread_id.into();
        }
        if let Some(message_id) = reply_to {
            body["reply_parameters"] = serde_json::to_value(ReplyParameters {
                message_id,
                allow_sending_without_reply: true,
            })?;
        }
        self.call("sendMessage", &body, Duration::from_secs(15))
            .await
    }

    pub async fn send_chat_action(
        &self,
        chat_id: i64,
        thread_id: Option<i64>,
        action: &str,
    ) -> Result<bool> {
        let mut body = json!({ "chat_id": chat_id, "action": action });
        if let Some(thread_id) = thread_id {
            body["message_thread_id"] = thread_id.into();
        }
        self.call("sendChatAction", &body, Duration::from_secs(10))
            .await
    }

    pub async fn get_file(&self, file_id: &str) -> Result<File> {
        self.call(
            "getFile",
            &json!({ "file_id": file_id }),
            Duration::from_secs(10),
        )
        .await
    }

    pub async fn download(&self, file_path: &str) -> Result<Vec<u8>> {
        let url = format!("{API}/file/bot{}/{file_path}", self.token);
        let resp = self
            .http
            .get(url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .context("file download failed")?
            .error_for_status()
            .context("file download failed")?;
        Ok(resp.bytes().await?.to_vec())
    }
}
