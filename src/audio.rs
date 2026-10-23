//! Splits long recordings into chunks at pauses so they can be transcribed in
//! parallel and streamed back to the user piece by piece.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// One piece of audio ready to upload, with a file name whose extension tells
/// Groq the container format.
pub struct Chunk {
    pub data: Vec<u8>,
    pub file_name: String,
}

pub async fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

/// Splits `data` into chunks of roughly `chunk_secs`, cutting inside pauses
/// when there is one near the target so words are not cut in half.
pub async fn split(data: &[u8], ext: &str, duration: f64, chunk_secs: f64) -> Result<Vec<Chunk>> {
    let dir = tempfile::tempdir()?;
    let input = dir.path().join(format!("in.{ext}"));
    tokio::fs::write(&input, data).await?;

    let silences = detect_silences(&input).await?;
    let cuts = pick_cuts(&silences, duration, chunk_secs);
    if cuts.is_empty() {
        return Ok(vec![Chunk {
            data: data.to_vec(),
            file_name: format!("audio.{ext}"),
        }]);
    }

    // Telegram voice notes are Ogg/Opus, which Groq accepts as-is, so copy the
    // packets without re-encoding. Anything else becomes 16 kHz mono FLAC,
    // which is what Whisper resamples to anyway.
    let (out_ext, codec): (&str, &[&str]) = if matches!(ext, "ogg" | "oga" | "opus") {
        ("ogg", &["-c:a", "copy"])
    } else {
        ("flac", &["-c:a", "flac", "-ar", "16000", "-ac", "1"])
    };
    let times = cuts
        .iter()
        .map(|t| format!("{t:.3}"))
        .collect::<Vec<_>>()
        .join(",");
    let pattern = dir.path().join(format!("chunk%03d.{out_ext}"));
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&input)
        .args(["-map", "0:a:0", "-vn"])
        .args(codec)
        .args([
            "-f",
            "segment",
            "-segment_times",
            &times,
            "-reset_timestamps",
            "1",
        ])
        .arg(&pattern)
        .stdin(Stdio::null())
        .output()
        .await
        .context("failed to run ffmpeg")?;
    if !output.status.success() {
        bail!(
            "ffmpeg segment failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let mut chunks = Vec::with_capacity(cuts.len() + 1);
    for i in 0.. {
        let path = dir.path().join(format!("chunk{i:03}.{out_ext}"));
        match tokio::fs::read(&path).await {
            Ok(data) => chunks.push(Chunk {
                data,
                file_name: format!("chunk{i}.{out_ext}"),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    if chunks.is_empty() {
        bail!("ffmpeg produced no chunks");
    }
    Ok(chunks)
}

/// Returns `(start, end)` of every pause in the recording.
async fn detect_silences(input: &Path) -> Result<Vec<(f64, f64)>> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(input)
        .args(["-af", "silencedetect=noise=-35dB:d=0.3", "-f", "null", "-"])
        .stdin(Stdio::null())
        .output()
        .await
        .context("failed to run ffmpeg")?;
    if !output.status.success() {
        bail!(
            "ffmpeg silencedetect failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(parse_silences(&String::from_utf8_lossy(&output.stderr)))
}

fn parse_silences(log: &str) -> Vec<(f64, f64)> {
    let value = |line: &str, key: &str| -> Option<f64> {
        let rest = &line[line.find(key)? + key.len()..];
        rest.split_whitespace().next()?.parse().ok()
    };
    let mut silences = Vec::new();
    let mut start = None;
    for line in log.lines() {
        if let Some(s) = value(line, "silence_start: ") {
            start = Some(s.max(0.0));
        } else if let Some(e) = value(line, "silence_end: ")
            && let Some(s) = start.take()
        {
            silences.push((s, e));
        }
    }
    silences
}

/// Chooses cut points about `chunk_secs` apart, preferring the middle of a
/// pause close to each target. The final chunk is never shorter than half a
/// chunk, so tiny tail requests are avoided.
fn pick_cuts(silences: &[(f64, f64)], duration: f64, chunk_secs: f64) -> Vec<f64> {
    let mut cuts = Vec::new();
    let mut last = 0.0;
    loop {
        let target = last + chunk_secs;
        if target + chunk_secs * 0.5 >= duration {
            break;
        }
        let window = (target - chunk_secs * 0.4)..=(target + chunk_secs * 0.25);
        let cut = silences
            .iter()
            .map(|(s, e)| (s + e) / 2.0)
            .filter(|mid| window.contains(mid))
            .min_by(|a, b| (a - target).abs().total_cmp(&(b - target).abs()))
            .unwrap_or(target);
        cuts.push(cut);
        last = cut;
    }
    cuts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffmpeg_silence_log() {
        let log = "\
[silencedetect @ 0x1] silence_start: 1.5
[silencedetect @ 0x1] silence_end: 2.25 | silence_duration: 0.75
[silencedetect @ 0x1] silence_start: -0.01
[silencedetect @ 0x1] silence_end: 9 | silence_duration: 9";
        assert_eq!(parse_silences(log), vec![(1.5, 2.25), (0.0, 9.0)]);
    }

    #[test]
    fn short_audio_is_not_cut() {
        assert!(pick_cuts(&[], 25.0, 20.0).is_empty());
        assert!(pick_cuts(&[], 29.9, 20.0).is_empty());
    }

    #[test]
    fn cuts_at_nearest_pause() {
        let silences = [(5.0, 5.5), (17.0, 18.0), (19.5, 20.1), (39.0, 41.0)];
        assert_eq!(pick_cuts(&silences, 60.0, 20.0), vec![19.8, 40.0]);
    }

    #[test]
    fn falls_back_to_hard_cut_without_pause() {
        assert_eq!(pick_cuts(&[(2.0, 3.0)], 65.0, 20.0), vec![20.0, 40.0]);
    }
}

#[cfg(test)]
mod ffmpeg_tests {
    use super::*;

    /// 65 s of Ogg/Opus: 9.4 s of tone then 0.6 s of silence, repeated.
    async fn sample() -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.ogg");
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("sine=frequency=440:duration=65,volume='if(lt(mod(t,10),9.4),1,0)':eval=frame")
            .args(["-ac", "1", "-c:a", "libopus", "-b:a", "24k"])
            .arg(&path)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        tokio::fs::read(&path).await.unwrap()
    }

    #[tokio::test]
    async fn splits_ogg_at_pauses() {
        if !ffmpeg_available().await {
            eprintln!("ffmpeg not installed, skipping");
            return;
        }
        let data = sample().await;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.ogg");
        tokio::fs::write(&input, &data).await.unwrap();
        let cuts = pick_cuts(&detect_silences(&input).await.unwrap(), 65.0, 20.0);
        assert_eq!(cuts.len(), 2);
        assert!(
            (cuts[0] - 19.7).abs() < 0.1 && (cuts[1] - 39.7).abs() < 0.1,
            "{cuts:?}"
        );

        let chunks = split(&data, "ogg", 65.0, 20.0).await.unwrap();
        assert_eq!(chunks.len(), 3);
        for c in &chunks {
            assert!(c.file_name.ends_with(".ogg"));
            assert_eq!(&c.data[..4], b"OggS");
        }
    }
}
