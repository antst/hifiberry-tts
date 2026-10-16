use crate::{
    audio::{Output, Pcm, Source},
    config::{Config, Settings},
    duck::Duck,
};
use reqwest::Client;
use serde::Serialize;
use std::{
    error::Error as _,
    sync::{atomic::Ordering, Arc, Weak},
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, RwLock},
    task::JoinHandle,
};
use url::Url;

#[derive(Clone, Serialize)]
pub struct Status {
    pub product: &'static str,
    pub version: &'static str,
    pub api_version: u8,
    pub playback_backend: &'static str,
    pub state: &'static str,
    pub has_media: bool,
    pub ducked_streams: usize,
    pub last_error: Option<String>,
    pub duck_warning: Option<String>,
    pub volume_level: f64,
    pub position: f64,
    pub duration: Option<f64>,
    pub pcm_buffered_ms: u64,
    pub pcm_underruns: u64,
    pub pcm_prebuffer_ms: u32,
    pub pcm_buffer_limit_ms: u32,
    pub pcm_buffering_mode: &'static str,
    pub pcm_source_rate: Option<f64>,
    pub pcm_target_ms: u64,
    pub pcm_waiting_for_complete: bool,
    pub pcm_receive_limit_bytes: u64,
    pub supports_cancel: bool,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            product: "hifiberry-tts",
            version: env!("CARGO_PKG_VERSION"),
            api_version: 1,
            playback_backend: "native-pipewire",
            state: "idle",
            has_media: false,
            ducked_streams: 0,
            last_error: None,
            duck_warning: None,
            volume_level: 1.0,
            position: 0.0,
            duration: None,
            pcm_buffered_ms: 0,
            pcm_underruns: 0,
            pcm_prebuffer_ms: 250,
            pcm_buffer_limit_ms: 1000,
            pcm_buffering_mode: "fixed",
            pcm_source_rate: None,
            pcm_target_ms: 250,
            pcm_waiting_for_complete: false,
            pcm_receive_limit_bytes: 20 * 1024 * 1024,
            supports_cancel: true,
        }
    }
}

pub enum Control {
    Load(Url, Settings),
    RegisterPcm(Weak<Pcm>),
    Pcm(Arc<Pcm>, Settings),
    Play,
    Stop,
    Cancel,
    Pause(bool),
    Volume(f64),
    Seek(f64),
    Shutdown,
}

pub enum Prepared {
    File(Cached),
    Pcm(Arc<Pcm>, Settings),
}

pub struct Cached {
    file: NamedTempFile,
    settings: Settings,
}

struct Running {
    output: Output,
    duck: Duck,
    started: Instant,
    paused: bool,
    settings: Settings,
}

pub async fn run(
    config: Arc<Config>,
    status: Arc<RwLock<Status>>,
    mut receiver: mpsc::Receiver<Control>,
) {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(config.download_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("TLS client initialization");
    let mut pending: Option<JoinHandle<Result<Prepared, String>>> = None;
    let mut cached: Option<Cached> = None;
    let mut last_direct: Option<(Url, Settings)> = None;
    let mut running: Option<Running> = None;
    let mut incoming: Vec<Weak<Pcm>> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(20));

    loop {
        tokio::select! {
            command = receiver.recv() => {
                match command {
                    Some(Control::RegisterPcm(pcm)) => {
                        incoming.retain(|p| p.upgrade().is_some_and(|p| !p.completed.load(Ordering::Acquire) && !p.is_cancelled()));
                        incoming.push(pcm);
                    }
                    Some(Control::Load(url, settings)) => {
                        cancel_download(&mut pending).await;
                        finish(&config, &status, &mut running).await;
                        cached = None;
                        last_direct = (url.scheme() == "wyoming").then(|| (url.clone(), settings.clone()));
                        {
                            let mut s = status.write().await;
                            s.state = "buffering";
                            s.has_media = false;
                            s.last_error = None;
                            s.duck_warning = None;
                            s.volume_level = settings.volume_level;
                            s.position = 0.0;
                            s.duration = None;
                        }
                        let config = config.clone();
                        let client = client.clone();
                        if url.scheme() == "wyoming" {
                            let status = status.clone();
                            pending = Some(tokio::spawn(async move {
                                crate::wyoming::direct(&config, &status, url, settings).await
                            }));
                        } else {
                            pending = Some(tokio::spawn(async move {
                                download(&config, &client, url, settings).await
                            }));
                        }
                    }
                    Some(Control::Pcm(pcm, mut settings)) => {
                        // Cancellation may overtake an already queued first
                        // chunk. That old stream must never restart output.
                        if pcm.is_cancelled() { continue; }
                        cancel_download(&mut pending).await;
                        finish(&config, &status, &mut running).await;
                        cached = None;
                        last_direct = None;
                        settings.volume_level = status.read().await.volume_level;
                        if pcm.is_adaptive() {
                            status.write().await.state = "buffering";
                            let status = status.clone();
                            let timeout = config.max_play_time;
                            pending = Some(tokio::spawn(async move { prepare_pcm(pcm, settings, &status, timeout).await }));
                        } else {
                            start_source(&config, &status, Source::Pcm(pcm), settings, &mut running).await;
                        }
                    }
                    Some(Control::Play) => {
                        if let Some(player) = running.as_mut() {
                            if player.paused {
                                let settings = player.settings.clone();
                                resume(&config, &status, player, &settings).await;
                            }
                        } else if pending.is_none() {
                            if let Some(media) = &cached {
                                start(&config, &status, media, &mut running).await;
                            } else if let Some((url, settings)) = last_direct.clone() {
                                let config = config.clone(); let status = status.clone();
                                status.write().await.state = "buffering";
                                pending = Some(tokio::spawn(async move { crate::wyoming::direct(&config, &status, url, settings).await }));
                            } else {
                                status.write().await.last_error = Some("No cached media; use play_media first".into());
                            }
                        }
                    }
                    Some(command @ (Control::Stop | Control::Cancel)) => {
                        for pcm in incoming.drain(..).filter_map(|p| p.upgrade()) {
                            if !pcm.completed.load(Ordering::Acquire) { pcm.cancel(); }
                        }
                        cancel_download(&mut pending).await;
                        finish(&config, &status, &mut running).await;
                        let purge = matches!(command, Control::Cancel);
                        if purge { cached = None; last_direct = None; }
                        let mut s = status.write().await;
                        s.state = "idle";
                        s.has_media = cached.is_some() || last_direct.is_some();
                        s.pcm_buffered_ms = 0;
                        s.pcm_waiting_for_complete = false;
                        s.pcm_source_rate = None;
                        if purge { s.position = 0.0; s.duration = None; s.last_error = None; }
                    }
                    Some(Control::Pause(paused)) => {
                        if let Some(player) = running.as_mut() {
                            if paused && !player.paused {
                                if player.output.pause(true).await.is_ok() {
                                    let messages = std::mem::take(&mut player.duck).restore(&config).await;
                                    player.paused = true;
                                    let mut s = status.write().await;
                                    s.state = "paused";
                                    s.ducked_streams = 0;
                                    s.duck_warning = warnings(&messages);
                                }
                            } else if !paused && player.paused {
                                let settings = player.settings.clone();
                                resume(&config, &status, player, &settings).await;
                            }
                        }
                    }
                    Some(Control::Volume(level)) => {
                        status.write().await.volume_level = level;
                        if let Some(media) = cached.as_mut() { media.settings.volume_level = level; }
                        if let Some((_, settings)) = last_direct.as_mut() { settings.volume_level = level; }
                        if let Some(player) = running.as_ref() {
                            player.output.volume(level);
                        }
                    }
                    Some(Control::Seek(position)) => {
                        if let Some(player) = running.as_ref() {
                            if let Err(error) = player.output.seek(position).await {
                                status.write().await.last_error = Some(error);
                            }
                        }
                    }
                    Some(Control::Shutdown) | None => {
                        cancel_download(&mut pending).await;
                        finish(&config, &status, &mut running).await;
                        break;
                    }
                }
            }
            _ = tick.tick() => {
                if pending.as_ref().is_some_and(|task| task.is_finished()) {
                    let result = pending.take().unwrap().await;
                    match result {
                        Ok(Ok(Prepared::File(mut media))) => {
                            media.settings.volume_level = status.read().await.volume_level;
                            status.write().await.has_media = true;
                            start(&config, &status, &media, &mut running).await;
                            cached = Some(media);
                        }
                        Ok(Ok(Prepared::Pcm(pcm, mut settings))) => {
                            settings.volume_level = status.read().await.volume_level;
                            start_source(&config, &status, Source::Pcm(pcm), settings, &mut running).await;
                        }
                        Ok(Err(error)) => fail(&status, error).await,
                        Err(_) => fail(&status, "Audio download task failed".into()).await,
                    }
                }
                let completed = if let Some(player) = running.as_mut() {
                    if player.started.elapsed() > config.max_play_time {
                        Some(Err("Playback exceeded MAX_PLAY_SECONDS".into()))
                    } else {
                        if player.output.finished() {
                            Some(player.output.error().map_or(Ok(()), Err))
                        } else if !player.output.started() && !player.paused && player.started.elapsed() > Duration::from_secs(10) {
                            Some(Err("PipeWire output did not start within 10 seconds".into()))
                        } else { None }
                    }
                } else { None };
                if let Some(result) = completed {
                    finish(&config, &status, &mut running).await;
                    status.write().await.has_media = cached.is_some() || last_direct.is_some();
                    match result {
                        Ok(()) => status.write().await.state = "idle",
                        Err(error) => fail(&status, error).await,
                    }
                } else if let Some(player) = running.as_mut() {
                    let mut s = status.write().await;
                    s.position = player.output.position();
                    s.duration = player.output.duration();
                    (s.pcm_buffered_ms, s.pcm_underruns) = player.output.metrics();
                    (s.pcm_source_rate, s.pcm_target_ms, s.pcm_waiting_for_complete) = player.output.adaptive_stats();
                    if player.output.started() && !player.paused { s.state = if player.output.buffering() { "buffering" } else { "playing" }; }
                }
            }
        }
    }
}

/// Keep reception independent of playout while adaptive startup buffers. The
/// guard closes upstream promptly when an in-flight prepare task is cancelled.
pub async fn prepare_pcm(
    pcm: Arc<Pcm>,
    settings: Settings,
    status: &RwLock<Status>,
    timeout: Duration,
) -> Result<Prepared, String> {
    struct Guard(Option<Arc<Pcm>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(pcm) = &self.0 {
                pcm.cancel();
            }
        }
    }
    let mut guard = Guard(Some(pcm));
    let pcm = guard.0.as_ref().unwrap();
    if pcm.is_adaptive() {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut s = status.write().await;
                (s.pcm_buffered_ms, s.pcm_underruns) = pcm.metrics();
                (
                    s.pcm_source_rate,
                    s.pcm_target_ms,
                    s.pcm_waiting_for_complete,
                ) = pcm.adaptive_stats();
            }
            if pcm.is_cancelled() {
                return Err("Announcement playback was stopped".into());
            }
            if pcm.ready() {
                break;
            }
            if Instant::now() >= deadline {
                return Err("Adaptive PCM buffering exceeded MAX_PLAY_SECONDS".into());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    Ok(Prepared::Pcm(guard.0.take().unwrap(), settings))
}

async fn cancel_download(pending: &mut Option<JoinHandle<Result<Prepared, String>>>) {
    if let Some(task) = pending.take() {
        task.abort();
        // An already completed prepare task still owns its result. Abort does
        // nothing in that case; explicitly cancel its PCM before dropping it.
        if let Ok(Ok(Prepared::Pcm(pcm, _))) = task.await {
            pcm.cancel();
        }
    }
}

async fn fail(status: &RwLock<Status>, error: String) {
    eprintln!("{error}");
    let mut s = status.write().await;
    s.state = "idle";
    s.last_error = Some(error);
}

async fn download(
    config: &Config,
    client: &Client,
    url: Url,
    settings: Settings,
) -> Result<Prepared, String> {
    // Fetch ourselves so redirects, protocol, size, and duration stay bounded.
    // The native decoder only sees a private file, never a signed HA URL/token.
    let started = Instant::now();
    let endpoint = url.origin().ascii_serialization();
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| describe_http_error("HTTP request", &endpoint, error))?;
    if !response.status().is_success() {
        return Err(format!(
            "Audio server {endpoint} returned HTTP {} (redirects are disabled)",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|len| len > config.max_audio_bytes)
    {
        return Err("Audio exceeds MAX_AUDIO_BYTES".into());
    }
    let file = NamedTempFile::new().map_err(|_| "Cannot create private audio cache file")?;
    let mut writer =
        tokio::fs::File::from_std(file.reopen().map_err(|_| "Cannot open audio cache file")?);
    let mut bytes = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| describe_http_error("download", &endpoint, error))?
    {
        bytes += chunk.len() as u64;
        if bytes > config.max_audio_bytes {
            return Err("Audio exceeds MAX_AUDIO_BYTES".into());
        }
        writer
            .write_all(&chunk)
            .await
            .map_err(|_| "Cannot write audio cache file")?;
    }
    writer
        .flush()
        .await
        .map_err(|_| "Cannot flush audio cache file")?;
    if bytes == 0 {
        return Err("Audio response was empty".into());
    }
    eprintln!(
        "Audio HTTP download: {bytes} bytes in {} ms",
        started.elapsed().as_millis()
    );
    Ok(Prepared::File(Cached { file, settings }))
}

fn describe_http_error(stage: &str, endpoint: &str, error: reqwest::Error) -> String {
    let category = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection/DNS/TLS"
    } else {
        "HTTP transport"
    };
    // Remove the request URL (which may contain HA signatures), retain useful
    // connector/TLS causes, and redact any URLs included by lower-level errors.
    let error = error.without_url();
    let mut details = vec![redact_urls(&error.to_string())];
    let mut cause = error.source();
    for _ in 0..6 {
        let Some(current) = cause else {
            break;
        };
        let detail = redact_urls(&current.to_string());
        if details.last() != Some(&detail) {
            details.push(detail);
        }
        cause = current.source();
    }
    format!(
        "Audio {stage} failed from {endpoint} ({category}): {}",
        details.join(": ")
    )
}

fn redact_urls(value: &str) -> String {
    let mut result = String::new();
    let mut remaining = value;
    loop {
        let start = match (remaining.find("http://"), remaining.find("https://")) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => {
                result.push_str(remaining);
                break;
            }
        };
        result.push_str(&remaining[..start]);
        let url = &remaining[start..];
        let end = url
            .find(|c: char| c.is_whitespace() || c == '\'' || c == '"')
            .unwrap_or(url.len());
        result.push_str("<audio-url>");
        remaining = &url[end..];
    }
    result
}

async fn start(
    config: &Config,
    status: &RwLock<Status>,
    media: &Cached,
    running: &mut Option<Running>,
) {
    match media.file.reopen() {
        Ok(file) => {
            start_source(
                config,
                status,
                Source::File(file),
                media.settings.clone(),
                running,
            )
            .await
        }
        Err(_) => fail(status, "Cannot open cached audio".into()).await,
    }
}

async fn start_source(
    config: &Config,
    status: &RwLock<Status>,
    source: Source,
    settings: Settings,
    running: &mut Option<Running>,
) {
    let started = Instant::now();
    let duck = Duck::apply(config, &settings).await;
    eprintln!(
        "Background ducking: {} streams in {} ms",
        duck.count(),
        started.elapsed().as_millis()
    );
    match Output::open(source, settings.volume_level, config.audio_device.clone()) {
        Ok(output) => {
            let mut s = status.write().await;
            s.state = "buffering";
            s.last_error = None;
            s.has_media = true;
            s.ducked_streams = duck.count();
            s.duck_warning = warnings(&duck.warnings);
            s.volume_level = settings.volume_level;
            s.position = 0.0;
            s.duration = None;
            s.pcm_buffered_ms = 0;
            s.pcm_underruns = 0;
            *running = Some(Running {
                output,
                duck,
                started: Instant::now(),
                paused: false,
                settings,
            });
        }
        Err(error) => {
            let messages = duck.restore(config).await;
            status.write().await.duck_warning = warnings(&messages);
            fail(status, error).await;
        }
    }
}

async fn resume(
    config: &Config,
    status: &RwLock<Status>,
    player: &mut Running,
    settings: &Settings,
) {
    let duck = Duck::apply(config, settings).await;
    if player.output.pause(false).await.is_ok() {
        player.paused = false;
        let mut s = status.write().await;
        s.state = "playing";
        s.ducked_streams = duck.count();
        s.duck_warning = warnings(&duck.warnings);
        player.duck = duck;
    } else {
        let messages = duck.restore(config).await;
        let mut s = status.write().await;
        s.last_error = Some("Cannot resume announcement".into());
        s.duck_warning = warnings(&messages);
    }
}

async fn finish(config: &Config, status: &RwLock<Status>, running: &mut Option<Running>) {
    if let Some(player) = running.take() {
        player.output.stop().await;
        let messages = player.duck.restore(config).await;
        let mut s = status.write().await;
        s.ducked_streams = 0;
        s.duck_warning = warnings(&messages);
        s.pcm_buffered_ms = 0;
        s.pcm_waiting_for_complete = false;
    }
}

fn warnings(messages: &[String]) -> Option<String> {
    if messages.is_empty() {
        None
    } else {
        Some(messages.join("; "))
    }
}

#[cfg(test)]
mod download_tests {
    use super::*;
    #[tokio::test]
    async fn cancel_discards_an_already_prepared_pcm_before_output_starts() {
        let pcm = Pcm::adaptive(16000, 1, 1000, 250, 64000).unwrap();
        pcm.push(&vec![0; 32000]).await.unwrap();
        let retained = pcm.clone();
        let task = tokio::spawn(async move {
            Ok(Prepared::Pcm(
                pcm,
                Settings {
                    volume_level: 1.0,
                    duck_factor: 1.0,
                    duck_targets: vec![],
                },
            ))
        });
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut pending = Some(task);
        cancel_download(&mut pending).await;
        assert!(pending.is_none());
        assert!(retained.is_cancelled());
        assert_eq!(retained.metrics().0, 0);
    }

    #[tokio::test]
    async fn connection_error_has_endpoint_and_cause_but_no_signed_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let error = Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!(
                "http://{addr}/private-file?authSig=secret-signature"
            ))
            .send()
            .await
            .unwrap_err();
        let message = describe_http_error("HTTP request", &format!("http://{addr}"), error);
        assert!(message.contains(&addr.to_string()));
        assert!(message.contains("connection/DNS/TLS"));
        assert!(!message.contains("secret-signature"));
        assert!(!message.contains("private-file"));
        assert_eq!(
            redact_urls("failed https://ha.local/a?authSig=secret with error"),
            "failed <audio-url> with error"
        );
    }
}
