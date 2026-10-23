//! Groq speech-to-text client (OpenAI-compatible transcription endpoint).

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, StatusCode};
use serde::Deserialize;

const BASE: &str = "https://api.groq.com/openai/v1";
const MAX_ATTEMPTS: u32 = 3;

#[derive(Clone)]
pub struct Groq {
    http: Client,
    api_key: String,
    model: String,
    language: Option<String>,
}

#[derive(Deserialize)]
struct Transcription {
    text: String,
}

impl Groq {
    pub fn new(http: Client, api_key: String, model: String, language: Option<String>) -> Self {
        Self {
            http,
            api_key,
            model,
            language,
        }
    }

    /// Validates the API key and opens a pooled TLS connection so the first
    /// transcription doesn't pay for the handshake.
    pub async fn warm_up(&self) -> Result<()> {
        self.http
            .get(format!("{BASE}/models"))
            .bearer_auth(&self.api_key)
            .timeout(Duration::from_secs(10))
            .send()
            .await?
            .error_for_status()
            .context("Groq API key check failed")?;
        Ok(())
    }

    pub async fn transcribe(&self, audio: Vec<u8>, file_name: &str) -> Result<String> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.request(audio.clone(), file_name).await {
                Ok(text) => return Ok(text),
                Err(Retry::After(wait, err)) if attempt < MAX_ATTEMPTS => {
                    tracing::warn!("groq attempt {attempt} failed, retrying in {wait:?}: {err:#}");
                    tokio::time::sleep(wait).await;
                }
                Err(Retry::After(_, err) | Retry::Never(err)) => return Err(err),
            }
        }
    }

    async fn request(&self, audio: Vec<u8>, file_name: &str) -> Result<String, Retry> {
        let mut form = Form::new()
            .part("file", Part::bytes(audio).file_name(file_name.to_owned()))
            .text("model", self.model.clone())
            .text("response_format", "json")
            .text("temperature", "0");
        if let Some(language) = &self.language {
            form = form.text("language", language.clone());
        }

        let resp = self
            .http
            .post(format!("{BASE}/audio/transcriptions"))
            .bearer_auth(&self.api_key)
            .multipart(form)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| Retry::After(Duration::from_millis(300), e.into()))?;

        let status = resp.status();
        if status.is_success() {
            let body: Transcription = resp.json().await.map_err(|e| {
                Retry::Never(anyhow::Error::from(e).context("invalid Groq response"))
            })?;
            return Ok(body.text.trim().to_owned());
        }

        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok())
            .map(|secs| Duration::from_secs_f64(secs.clamp(0.0, 10.0)));
        let body = resp.text().await.unwrap_or_default();
        let err = anyhow::anyhow!("Groq returned {status}: {body}");
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            Err(Retry::After(
                retry_after.unwrap_or(Duration::from_secs(1)),
                err,
            ))
        } else {
            Err(Retry::Never(err))
        }
    }
}

enum Retry {
    After(Duration, anyhow::Error),
    Never(anyhow::Error),
}
