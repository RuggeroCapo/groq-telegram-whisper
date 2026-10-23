# groq-telegram-whisper

A Telegram bot that transcribes voice messages with Groq's
`whisper-large-v3-turbo`. In private chats, the text appears live through
[`sendMessageDraft`](https://core.telegram.org/bots/api#sendmessagedraft)
while transcription is still running.

It handles voice messages, audio files and video notes.

## How it stays fast

Nearly all the latency is network time: downloading the file from Telegram
and the Groq round trip. The bot reduces it like this:

1. **Instant feedback.** As soon as a voice message arrives, the bot sends an
   empty draft, which shows Telegram's "Thinking…" placeholder. It does this
   while the file downloads.
2. **No transcoding.** Telegram voice notes are Ogg/Opus, which Groq accepts
   as-is, so they're uploaded unchanged.
3. **Parallel chunks for long recordings.** Groq's transcription API doesn't
   stream. For recordings longer than 1.5 × `CHUNK_SECONDS`, the bot finds
   pauses with `ffmpeg silencedetect` and cuts at them, so words aren't split.
   It copies the Opus packets without re-encoding and transcribes every chunk
   at the same time. Each time the next chunk in order finishes, the draft
   gets longer. The final message is sent when all chunks are done.
4. **Warm connections.** One pooled HTTP/2 client is shared, and the Groq
   connection is opened at startup, so later requests skip the TCP and TLS
   handshakes.

Drafts only work in private chats. In groups the bot shows "typing…" and then
replies with the transcription.

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

You need Rust 1.88 or newer. `ffmpeg` is optional; without it, long
recordings are sent as a single request.

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
| `CHUNK_SECONDS` | `20` | Target chunk length for long recordings. `0` disables splitting |
| `MAX_PARALLEL_REQUESTS` | `4` | Limit on concurrent Groq requests across all chats |
| `RUST_LOG` | `info` | Log level |

Groq bills each request for at least 10 seconds of audio, and the free tier
has a low requests-per-minute limit. Smaller `CHUNK_SECONDS` values make
drafts update more often, but they also mean more requests. Groq's 429
responses are retried and the `retry-after` header is respected.

## Limits

- Bots can only download files up to 20 MB, so longer files are rejected
  with an error message.
- Transcriptions longer than 4096 characters are split into several messages.
