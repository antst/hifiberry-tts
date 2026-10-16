//! Wyoming PCM transport: announcement-only satellite plus direct TTS client.
use crate::{
    audio::Pcm,
    config::{Config, Settings},
    player::{Control, Prepared, Status},
};
use serde_json::{json, Value};
use std::{
    io,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{tcp::OwnedWriteHalf, TcpListener, TcpStream},
    sync::{mpsc, watch, Mutex, RwLock, Semaphore},
};
use url::Url;

struct Event {
    kind: String,
    data: Value,
    payload: Vec<u8>,
}
async fn read(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Result<Option<Event>, String> {
    let mut line = Vec::new();
    loop {
        let buf = reader.fill_buf().await.map_err(|e| e.to_string())?;
        if buf.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err("Truncated Wyoming header".into())
            };
        }
        let end = buf.iter().position(|b| *b == b'\n').map(|n| n + 1);
        let count = end.unwrap_or(buf.len());
        if line.len() + count > 16384 {
            return Err("Wyoming header exceeds 16 KiB".into());
        }
        line.extend_from_slice(&buf[..count]);
        reader.consume(count);
        if end.is_some() {
            break;
        }
    }
    let header: Value = serde_json::from_slice(&line).map_err(|_| "Invalid Wyoming JSON header")?;
    let mut data = header.get("data").cloned().unwrap_or_else(|| json!({}));
    let data_length = header
        .get("data_length")
        .map_or(Ok(0), |v| v.as_u64().ok_or("Invalid Wyoming data length"))?;
    let payload_length = header.get("payload_length").map_or(Ok(0), |v| {
        v.as_u64().ok_or("Invalid Wyoming payload length")
    })?;
    if data_length > 65536 || payload_length > 262144 {
        return Err("Wyoming event exceeds size limit".into());
    }
    if data_length > 0 {
        let mut extra = vec![0; data_length as usize];
        reader
            .read_exact(&mut extra)
            .await
            .map_err(|e| e.to_string())?;
        let extra: Value =
            serde_json::from_slice(&extra).map_err(|_| "Invalid Wyoming event data")?;
        data.as_object_mut()
            .ok_or("Wyoming data must be an object")?
            .extend(
                extra
                    .as_object()
                    .ok_or("Wyoming extra data must be an object")?
                    .clone(),
            );
    }
    let mut payload = vec![0; payload_length as usize];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(Event {
        kind: header["type"]
            .as_str()
            .ok_or("Missing Wyoming event type")?
            .into(),
        data,
        payload,
    }))
}
async fn write(writer: &mut OwnedWriteHalf, kind: &str, data: Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&json!({"type":kind,"data":data}))?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await
}
fn format(event: &Event) -> Result<(u32, usize), String> {
    let rate = event.data["rate"]
        .as_u64()
        .ok_or("Missing PCM sample rate")?;
    let channels = event.data["channels"]
        .as_u64()
        .ok_or("Missing PCM channel count")?;
    if !(8000..=192000).contains(&rate) || !(1..=2).contains(&channels) || event.data["width"] != 2
    {
        return Err(
            "Wyoming requires signed little-endian 16-bit mono/stereo PCM at 8–192 kHz".into(),
        );
    }
    Ok((rate as u32, channels as usize))
}
fn validate_chunk(event: &Event, pcm: &Pcm) -> Result<(), String> {
    let (rate, channels) = format(event)?;
    if rate != pcm.rate || channels != pcm.channels {
        return Err("Wyoming PCM format changed mid-stream".into());
    }
    Ok(())
}
fn limited(total: u64, pcm: &Pcm, config: &Config) -> Result<(), String> {
    if total > config.max_audio_bytes
        || total / (pcm.channels as u64 * 2) > pcm.rate as u64 * config.max_play_time.as_secs()
    {
        Err("Wyoming audio exceeds configured byte/duration limit".into())
    } else {
        Ok(())
    }
}
fn info() -> Value {
    json!({"satellite": {"name":"Kitchen Audio", "description":"HiFiBerry announcement speaker (no microphone)",
        "attribution":{"name":"hifiberry-tts","url":"https://github.com/hifiberry/hifiberry-os"},
        "installed":true,"version":env!("CARGO_PKG_VERSION"),"has_vad":false,"supports_trigger":false,
        "active_wake_words":[],"max_active_wake_words":0},
        "snd":[{"name":"hifiberry-tts","description":"PipeWire PCM output","installed":true,
          "attribution":{"name":"hifiberry-tts","url":"https://github.com/hifiberry/hifiberry-os"},
          "snd_format":{"rate":22050,"width":2,"channels":1}}]})
}
pub async fn serve(
    listener: TcpListener,
    config: Arc<Config>,
    status: Arc<RwLock<Status>>,
    sender: mpsc::Sender<Control>,
    mut cancelled: watch::Receiver<bool>,
) {
    eprintln!(
        "Wyoming announcement satellite listening on {}",
        listener.local_addr().unwrap()
    );
    let slots = Arc::new(Semaphore::new(8));
    loop {
        tokio::select! {
            _ = cancelled.changed() => break,
            accepted = listener.accept() => {
                let Ok((socket, peer)) = accepted else { break; };
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                let config = config.clone(); let sender = sender.clone(); let status = status.clone(); let mut cancelled = cancelled.clone();
                eprintln!("Wyoming client connected: {peer}");
                tokio::spawn(async move {
                    let _permit = permit;
                    tokio::select! { _ = cancelled.changed() => {}, result = session(socket, config, status, sender) => {
                        if let Err(error) = result { eprintln!("Wyoming satellite: {error}"); }
                    }}
                });
            }
        }
    }
}
struct CancelOnDrop(Option<Arc<Pcm>>);
struct Heartbeat(tokio::task::JoinHandle<()>);
impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(pcm) = &self.0 {
            pcm.cancel();
        }
    }
}
async fn session(
    socket: TcpStream,
    config: Arc<Config>,
    status: Arc<RwLock<Status>>,
    sender: mpsc::Sender<Control>,
) -> Result<(), String> {
    let (reader, writer) = socket.into_split();
    let mut reader = BufReader::new(reader);
    let writer = Arc::new(Mutex::new(writer));
    let mut heartbeat: Option<Heartbeat> = None;
    let mut active = CancelOnDrop(None);
    let mut started = false;
    let mut bytes = 0;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(90), read(&mut reader))
            .await
            .map_err(|_| "Wyoming connection idle timeout")??;
        let Some(event) = event else {
            return Ok(());
        };
        match event.kind.as_str() {
            "describe" => write(&mut *writer.lock().await, "info", info())
                .await
                .map_err(|e| e.to_string())?,
            "run-satellite" => {
                write(&mut *writer.lock().await, "satellite-connected", json!({}))
                    .await
                    .map_err(|e| e.to_string())?;
                if heartbeat.is_none() {
                    let writer = writer.clone();
                    heartbeat = Some(Heartbeat(tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            // HA may queue many seconds of PCM ahead of its ping.
                            // Keep its read loop alive while the bounded audio
                            // queue backpressures that data on the same socket.
                            if write(
                                &mut *writer.lock().await,
                                "ping",
                                json!({"text":"hifiberry-tts"}),
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                        }
                    })));
                }
            }
            "ping" => write(&mut *writer.lock().await, "pong", event.data)
                .await
                .map_err(|e| e.to_string())?,
            "audio-start" => {
                let (rate, channels) = format(&event)?;
                eprintln!("Wyoming satellite audio-start: {rate} Hz, {channels} channels");
                if let Some(old) = active.0.take() {
                    old.cancel();
                }
                active.0 = Some(Pcm::new(
                    rate,
                    channels,
                    config.pcm_buffer_ms,
                    config.pcm_prebuffer_ms,
                ));
                started = false;
                bytes = 0;
            }
            "audio-chunk" => {
                let pcm = active.0.as_ref().ok_or("Audio chunk without audio-start")?;
                validate_chunk(&event, pcm)?;
                bytes += event.payload.len() as u64;
                limited(bytes, pcm, &config)?;
                if !started && !event.payload.is_empty() {
                    let mut settings = config.defaults.clone();
                    settings.volume_level = status.read().await.volume_level;
                    sender
                        .send(Control::Pcm(pcm.clone(), settings))
                        .await
                        .map_err(|_| "Player stopped")?;
                    started = true;
                }
                pcm.push(&event.payload).await?;
            }
            "audio-stop" => {
                eprintln!("Wyoming satellite audio-stop: {bytes} PCM bytes received");
                if bytes == 0 {
                    eprintln!("No satellite audio received; check HA's TTS pipeline, local URL and FFmpeg logs");
                }
                if let Some(pcm) = active.0.as_ref() {
                    pcm.close();
                    let pcm = pcm.clone();
                    let writer = writer.clone();
                    let timeout = config.max_play_time;
                    let started = started;
                    tokio::spawn(async move {
                        let _ = tokio::time::timeout(timeout, async {
                            while started
                                && !pcm.completed.load(Ordering::Acquire)
                                && !pcm.is_cancelled()
                            {
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                            let _ = write(&mut *writer.lock().await, "played", json!({})).await;
                        })
                        .await;
                    });
                }
            }
            "pause-satellite" => {} // HA pipeline pause does not cancel an announcement.
            "synthesize" | "run-pipeline" | "transcribe" | "detect" => {}
            _ => {}
        }
    }
}

pub async fn direct(
    config: &Config,
    _status: &RwLock<Status>,
    url: Url,
    settings: Settings,
) -> Result<Prepared, String> {
    let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    let server = pairs
        .get("server")
        .or(config.tts_server.as_ref())
        .ok_or("Set WYOMING_TTS_ADDR in the bridge environment to use direct TTS")?;
    let socket = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(server))
        .await
        .map_err(|_| "Wyoming TTS connection timed out")?
        .map_err(|e| format!("Cannot connect to Wyoming TTS: {e}"))?;
    socket.set_nodelay(true).map_err(|e| e.to_string())?;
    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::new(reader);
    let mut data = json!({"text":pairs.get("text").ok_or("Missing TTS text")?});
    let mut voice = serde_json::Map::new();
    if let Some(name) = pairs
        .get("voice")
        .or(config.tts_voice.as_ref())
        .filter(|v| !v.is_empty())
    {
        voice.insert("name".into(), json!(name));
    }
    for name in ["language", "speaker"] {
        if let Some(value) = pairs.get(name) {
            voice.insert(name.into(), json!(value));
        }
    }
    if !voice.is_empty() {
        data["voice"] = Value::Object(voice);
    }
    // Match HA's streaming text request when the selected server supports it.
    // Some engines use a phrase coordinator only on this protocol path; plain
    // synthesize can have very different inference/chunk cadence for long text.
    write(&mut writer, "describe", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    let description = tokio::time::timeout(Duration::from_secs(5), read(&mut reader))
        .await
        .map_err(|_| "Wyoming TTS description timed out")??
        .ok_or("Wyoming TTS disconnected before its description")?;
    if description.kind != "info" {
        return Err("Wyoming TTS did not return info for describe".into());
    }
    let streaming = supports_streaming(&description.data);
    if streaming {
        let mut start = json!({});
        if let Some(voice) = data.get("voice") {
            start["voice"] = voice.clone();
        }
        write(&mut writer, "synthesize-start", start)
            .await
            .map_err(|e| e.to_string())?;
        write(
            &mut writer,
            "synthesize-chunk",
            json!({"text":data["text"]}),
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    // This full-text event is HA's compatibility fallback within a streaming
    // request too. A streaming server consumes it without synthesizing twice.
    write(&mut writer, "synthesize", data)
        .await
        .map_err(|e| e.to_string())?;
    if streaming {
        write(&mut writer, "synthesize-stop", json!({}))
            .await
            .map_err(|e| e.to_string())?;
    }
    eprintln!(
        "Wyoming TTS request: {}",
        if streaming {
            "streaming text"
        } else {
            "plain synthesize"
        }
    );
    let mut pcm: Option<Arc<Pcm>> = None;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(60), read(&mut reader))
            .await
            .map_err(|_| "Wyoming TTS first audio timed out")??
            .ok_or("Wyoming TTS disconnected before audio")?;
        match event.kind.as_str() {
            "error" => return Err("Wyoming TTS server reported a synthesis error".into()),
            "audio-start" => {
                let (rate, channels) = format(&event)?;
                pcm = Some(Pcm::new(
                    rate,
                    channels,
                    config.pcm_buffer_ms,
                    config.pcm_prebuffer_ms,
                ));
            }
            "audio-chunk" if !event.payload.is_empty() => {
                let pcm = pcm.ok_or("Wyoming TTS omitted audio-start")?;
                validate_chunk(&event, &pcm)?;
                limited(event.payload.len() as u64, &pcm, config)?;
                let weak = Arc::downgrade(&pcm);
                let config = config.clone();
                let first_bytes = event.payload.len() as u64;
                tokio::spawn(async move {
                    // Keep the write half alive while reading the same connection.
                    let _writer = writer;
                    let result: Result<(), String> = async {
                        if let Some(pcm) = weak.upgrade() {
                            pcm.push(&event.payload).await?;
                        }
                        let mut total = first_bytes;
                        loop {
                            let Some(pcm) = weak.upgrade() else {
                                return Ok(());
                            };
                            let event = tokio::select! {
                                _ = pcm.cancelled() => return Ok(()),
                                event =
                                tokio::time::timeout(Duration::from_secs(30), read(&mut reader))
                                    => event.map_err(|_| "Wyoming TTS audio stalled")??
                                    .ok_or("Wyoming TTS disconnected without audio-stop")?,
                            };
                            if pcm.is_cancelled() {
                                return Ok(());
                            }
                            match event.kind.as_str() {
                                "audio-chunk" => {
                                    validate_chunk(&event, &pcm)?;
                                    total += event.payload.len() as u64;
                                    limited(total, &pcm, &config)?;
                                    pcm.push(&event.payload).await?;
                                }
                                "audio-stop" => {
                                    pcm.close();
                                    return Ok(());
                                }
                                "error" => {
                                    return Err(
                                        "Wyoming TTS server reported a synthesis error".into()
                                    )
                                }
                                _ => {}
                            }
                        }
                    }
                    .await;
                    if let Some(pcm) = weak.upgrade() {
                        if let Err(error) = result {
                            pcm.fail(error);
                        }
                    }
                });
                return Ok(Prepared::Pcm(pcm, settings));
            }
            "audio-stop" => return Err("Wyoming TTS returned no audio".into()),
            _ => {}
        }
    }
}

fn supports_streaming(info: &Value) -> bool {
    info.get("tts")
        .and_then(Value::as_array)
        .and_then(|programs| programs.iter().find(|p| p["installed"] == true))
        .is_some_and(|program| program["supports_synthesize_streaming"] == true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_negotiate_streaming_advertised_by_the_selected_installed_program() {
        assert!(!supports_streaming(&json!({"tts":[{"installed":true}]})));
        assert!(!supports_streaming(
            &json!({"tts":[{"installed":false,"supports_synthesize_streaming":true}]})
        ));
        assert!(!supports_streaming(
            &json!({"tts":[{"installed":true,"supports_synthesize_streaming":false},{"installed":true,"supports_synthesize_streaming":true}]})
        ));
        assert!(supports_streaming(
            &json!({"tts":[{"installed":true,"supports_synthesize_streaming":true}]})
        ));
    }
}
