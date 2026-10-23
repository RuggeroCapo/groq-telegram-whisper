mod bot;
mod groq;
mod telegram;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

use crate::bot::{Bot, Config};
use crate::groq::Groq;
use crate::telegram::Telegram;

const POLL_TIMEOUT_SECS: u64 = 50;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let token = env("TELEGRAM_BOT_TOKEN").context("TELEGRAM_BOT_TOKEN is not set")?;
    let groq_key = env("GROQ_API_KEY").context("GROQ_API_KEY is not set")?;
    let model = env("GROQ_MODEL").unwrap_or_else(|| "whisper-large-v3-turbo".into());
    let language = env("WHISPER_LANGUAGE");
    let allowed_users = env("ALLOWED_USER_IDS")
        .map(|v| {
            v.split(',')
                .map(|id| id.trim().parse::<i64>())
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
        .context("ALLOWED_USER_IDS must be a comma-separated list of numeric user IDs")?
        .unwrap_or_default();

    // One pooled client for everything: connections to Telegram and Groq stay
    // open between requests, so only the first one pays for TCP + TLS.
    let http = reqwest::Client::builder()
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(10))
        .build()?;

    let tg = Telegram::new(http.clone(), token);
    let groq = Groq::new(http, groq_key, model.clone(), language);
    groq.warm_up().await?;
    tg.delete_webhook().await?;

    let bot = Arc::new(Bot {
        tg: tg.clone(),
        groq,
        config: Config { allowed_users },
    });
    tracing::info!(%model, "bot started");

    // Docker stops containers with SIGTERM, which PID 1 ignores unless handled.
    let mut sigterm = signal(SignalKind::terminate())?;
    tokio::select! {
        res = poll(tg, bot) => res,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            Ok(())
        }
        _ = sigterm.recv() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}

async fn poll(tg: Telegram, bot: Arc<Bot>) -> Result<()> {
    let mut offset = 0;
    loop {
        let updates = match tg.get_updates(offset, POLL_TIMEOUT_SECS).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("getUpdates failed: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        for update in updates {
            offset = offset.max(update.update_id + 1);
            if let Some(msg) = update.message {
                tokio::spawn(bot.clone().handle(msg));
            }
        }
    }
}
