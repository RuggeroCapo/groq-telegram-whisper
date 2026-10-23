# groq-telegram-whisper

A Telegram bot that transcribes voice messages with Groq's
`whisper-large-v3-turbo` and replies with the text.

It handles voice messages, audio files and video notes.

## How it stays fast

Nearly all the latency is network time: downloading the file from Telegram
and the Groq round trip. The bot keeps both short:

- **No transcoding.** Telegram voice notes are Ogg/Opus, which Groq accepts
  as-is, so the file is uploaded unchanged in a single request.
- **Warm connections.** One pooled HTTP/2 client is shared, and the Groq
  connection is opened at startup, so later requests skip the TCP and TLS
  handshakes.

While it works, the bot shows "typing…" in the chat.

## Setup

1. Create a bot with [@BotFather](https://t.me/BotFather) and get the token.
2. Get an API key at <https://console.groq.com/keys>.
3. Copy `.env.example` to `.env` and fill it in.

### Docker

```sh
docker build -t groq-telegram-whisper .
docker run --rm --env-file .env groq-telegram-whisper
```

### From source

You need Rust 1.88 or newer.

```sh
set -a; . ./.env; set +a
cargo run --release
```

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | required | Bot token from BotFather |
| `GROQ_API_KEY` | required | Groq API key |
| `GROQ_MODEL` | `whisper-large-v3-turbo` | Any Groq speech-to-text model |
| `WHISPER_LANGUAGE` | auto-detect | ISO-639-1 code, e.g. `it`. Setting it is slightly faster and more accurate |
| `ALLOWED_USER_IDS` | anyone | Comma-separated Telegram user IDs. Send `/start` to the bot to see yours |
| `RUST_LOG` | `info` | Log level |

Groq's 429 and 5xx responses are retried, and the `retry-after` header is
respected.

## Limits

- Bots can only download files up to 20 MB, so longer files are rejected
  with an error message.
- Transcriptions longer than 4096 characters are split into several messages.
