//! Small in-process decoder/output. File decoding runs on a dedicated PipeWire
//! main-loop thread, never in PipeWire's realtime processing thread.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
use std::{
    collections::VecDeque,
    fs::File,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{DecoderOptions, CODEC_TYPE_NULL},
    errors::Error,
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo},
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::Hint,
    units::{Time, TimeBase},
};

#[cfg(target_os = "linux")]
mod output;

pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track: u32,
    time_base: TimeBase,
    pub rate: u32,
    channels: usize,
    pub duration: Option<f64>,
    samples: Option<SampleBuffer<f32>>,
    offset: usize,
    frame: u64,
    discard_until: u64,
}

impl Decoder {
    pub fn new(file: File) -> Result<Self, String> {
        let source = MediaSourceStream::new(Box::new(file), Default::default());
        let format = symphonia::default::get_probe()
            .format(
                &Hint::new(),
                source,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .map_err(|e| format!("Cannot recognize audio: {e}"))?
            .format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or("No audio track")?;
        let params = &track.codec_params;
        let decoder = symphonia::default::get_codecs()
            .make(params, &DecoderOptions::default())
            .map_err(|e| format!("Unsupported audio codec: {e}"))?;
        let rate = params.sample_rate.unwrap_or(0);
        let duration = params
            .n_frames
            .zip(params.sample_rate)
            .map(|(n, r)| n as f64 / r as f64);
        let mut result = Self {
            track: track.id,
            time_base: params.time_base.unwrap_or(TimeBase::new(1, rate.max(1))),
            rate,
            channels: 0,
            duration,
            format,
            decoder,
            samples: None,
            offset: 0,
            frame: 0,
            discard_until: 0,
        };
        if !result.packet()? {
            return Err("Audio has no samples".into());
        }
        Ok(result)
    }

    fn packet(&mut self) -> Result<bool, String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(false)
                }
                Err(e) => return Err(format!("Cannot read audio packet: {e}")),
            };
            if packet.track_id() != self.track {
                continue;
            }
            let audio = self
                .decoder
                .decode(&packet)
                .map_err(|e| format!("Audio decode failed: {e}"))?;
            if audio.frames() == 0 {
                continue;
            }
            let spec = *audio.spec();
            let channels = spec.channels.count();
            if !(8000..=192000).contains(&spec.rate) || !(1..=2).contains(&channels) {
                return Err("Only mono/stereo audio at 8–192 kHz is supported".into());
            }
            if self.channels != 0 && (self.rate != spec.rate || self.channels != channels) {
                return Err("Mid-stream audio format changes are unsupported".into());
            }
            self.rate = spec.rate;
            self.channels = channels;
            if self
                .samples
                .as_ref()
                .is_none_or(|s| s.capacity() < audio.frames() * channels)
            {
                self.samples = Some(SampleBuffer::new(audio.capacity() as u64, spec));
            }
            self.samples.as_mut().unwrap().copy_interleaved_ref(audio);
            self.offset = 0;
            return Ok(true);
        }
    }

    fn next(&mut self) -> Result<Option<[f32; 2]>, String> {
        loop {
            if self
                .samples
                .as_ref()
                .is_none_or(|s| self.offset >= s.samples().len())
                && !self.packet()?
            {
                return Ok(None);
            }
            let samples = self.samples.as_ref().unwrap().samples();
            let left = samples[self.offset];
            let right = samples[self.offset + self.channels - 1];
            self.offset += self.channels;
            self.frame += 1;
            if self.frame <= self.discard_until {
                continue;
            }
            return Ok(Some([left, right]));
        }
    }

    fn seek(&mut self, seconds: f64) -> Result<(), String> {
        let result = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::from(seconds),
                    track_id: Some(self.track),
                },
            )
            .map_err(|e| format!("Cannot seek audio: {e}"))?;
        self.decoder.reset();
        self.samples = None;
        self.offset = 0;
        let actual = self.time_base.calc_time(result.actual_ts);
        self.frame = ((actual.seconds as f64 + actual.frac) * self.rate as f64).round() as u64;
        self.discard_until = (seconds * self.rate as f64).round() as u64;
        Ok(())
    }
}

/// Bounded Wyoming PCM queue with configurable startup/underrun prebuffer.
pub struct Pcm {
    pub rate: u32,
    pub channels: usize,
    inner: Mutex<PcmQueue>,
    capacity: usize,
    prebuffer: usize,
    underruns: AtomicU64,
    cancelled: AtomicBool,
    pub completed: AtomicBool,
    error: Mutex<Option<String>>,
    cancel_notify: tokio::sync::Notify,
}

struct PcmQueue {
    samples: VecDeque<[f32; 2]>,
    closed: bool,
    waiting: bool,
    played: bool,
}
#[derive(Debug, PartialEq)]
pub(super) enum PcmFrame {
    Sample([f32; 2]),
    Pending,
    End,
}

impl Pcm {
    pub fn new(rate: u32, channels: usize, buffer_ms: u32, prebuffer_ms: u32) -> Arc<Self> {
        Arc::new(Self {
            rate,
            channels,
            inner: Mutex::new(PcmQueue {
                samples: VecDeque::new(),
                closed: false,
                waiting: true,
                played: false,
            }),
            capacity: rate as usize * buffer_ms as usize / 1000,
            prebuffer: rate as usize * prebuffer_ms as usize / 1000,
            underruns: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            error: Mutex::new(None),
            cancel_notify: tokio::sync::Notify::new(),
        })
    }
    pub async fn push(&self, bytes: &[u8]) -> Result<(), String> {
        let stride = self.channels * 2;
        if !bytes.len().is_multiple_of(stride) {
            return Err("PCM chunk is not frame aligned".into());
        }
        for part in bytes.chunks((self.rate as usize / 100).max(1) * stride) {
            loop {
                if self.cancelled.load(Ordering::Acquire) || self.completed.load(Ordering::Acquire)
                {
                    return Err("Announcement playback was stopped".into());
                }
                {
                    let mut queue = self.inner.lock().unwrap();
                    if queue.closed {
                        return Err("PCM stream is already closed".into());
                    }
                    if queue.samples.len() + part.len() / stride <= self.capacity {
                        for frame in part.chunks_exact(stride) {
                            let left = i16::from_le_bytes([frame[0], frame[1]]) as f32 / 32768.0;
                            let n = stride - 2;
                            let right =
                                i16::from_le_bytes([frame[n], frame[n + 1]]) as f32 / 32768.0;
                            queue.samples.push_back([left, right]);
                        }
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        Ok(())
    }
    pub(super) fn frame(&self) -> Result<PcmFrame, String> {
        let mut queue = self.inner.lock().unwrap();
        let ended = queue.closed || self.is_cancelled();
        if queue.waiting && !ended && queue.samples.len() < self.prebuffer.max(1) {
            return Ok(PcmFrame::Pending);
        }
        queue.waiting = false;
        if let Some(frame) = queue.samples.pop_front() {
            queue.played = true;
            return Ok(PcmFrame::Sample(frame));
        }
        if ended {
            if let Some(error) = self.error.lock().unwrap().take() {
                return Err(error);
            }
            return Ok(PcmFrame::End);
        }
        if queue.played {
            let count = self.underruns.fetch_add(1, Ordering::AcqRel) + 1;
            eprintln!(
                "PCM underrun {count}; waiting for {} ms of audio",
                self.prebuffer * 1000 / self.rate as usize
            );
        }
        queue.waiting = true;
        Ok(PcmFrame::Pending)
    }
    pub fn metrics(&self) -> (u64, u64) {
        (
            self.inner.lock().unwrap().samples.len() as u64 * 1000 / self.rate as u64,
            self.underruns.load(Ordering::Acquire),
        )
    }
    pub fn close(&self) {
        self.inner.lock().unwrap().closed = true;
    }
    pub fn fail(&self, error: String) {
        *self.error.lock().unwrap() = Some(error);
        self.close();
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.cancel_notify.notify_waiters();
        self.close();
    }
    pub async fn cancelled(&self) {
        loop {
            let notified = self.cancel_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

pub enum Source {
    File(File),
    Pcm(Arc<Pcm>),
}
pub enum Command {
    Stop,
    Pause(bool, tokio::sync::oneshot::Sender<Result<(), String>>),
    Seek(f64, tokio::sync::oneshot::Sender<Result<(), String>>),
}
pub struct Shared {
    pub started: AtomicBool,
    pub finished: AtomicBool,
    pub buffering: AtomicBool,
    pub volume: AtomicU32,
    pub position: AtomicU64,
    pub duration: AtomicU64,
    pub error: Mutex<Option<String>>,
}
pub struct Output {
    pub shared: Arc<Shared>,
    sender: mpsc::Sender<Command>,
    thread: Option<thread::JoinHandle<()>>,
    pcm: Option<Arc<Pcm>>,
}
impl Output {
    pub fn open(source: Source, volume: f64, target: Option<String>) -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel();
        let shared = Arc::new(Shared {
            started: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            buffering: AtomicBool::new(true),
            volume: AtomicU32::new((volume as f32).to_bits()),
            position: AtomicU64::new(0.0_f64.to_bits()),
            duration: AtomicU64::new(f64::NAN.to_bits()),
            error: Mutex::new(None),
        });
        let state = shared.clone();
        let pcm = match &source {
            Source::Pcm(pcm) => Some(pcm.clone()),
            _ => None,
        };
        let completed_pcm = pcm.clone();
        let thread = thread::Builder::new()
            .name("tts-audio".into())
            .spawn(move || {
                #[cfg(target_os = "linux")]
                let result = output::run(source, target, receiver, state.clone());
                #[cfg(not(target_os = "linux"))]
                let result: Result<(), String> = {
                    let _ = (source, target, receiver);
                    Err("Native playback requires Linux/PipeWire".into())
                };
                if let Err(error) = result {
                    *state.error.lock().unwrap() = Some(error);
                }
                state.finished.store(true, Ordering::Release);
                if let Some(pcm) = completed_pcm {
                    pcm.completed.store(true, Ordering::Release);
                }
            })
            .map_err(|e| format!("Cannot start audio thread: {e}"))?;
        Ok(Self {
            shared,
            sender,
            thread: Some(thread),
            pcm,
        })
    }
    pub fn volume(&self, level: f64) {
        self.shared
            .volume
            .store((level as f32).to_bits(), Ordering::Release);
    }
    pub fn position(&self) -> f64 {
        f64::from_bits(self.shared.position.load(Ordering::Acquire))
    }
    pub fn duration(&self) -> Option<f64> {
        let value = f64::from_bits(self.shared.duration.load(Ordering::Acquire));
        value.is_finite().then_some(value)
    }
    pub fn metrics(&self) -> (u64, u64) {
        self.pcm.as_ref().map_or((0, 0), |p| p.metrics())
    }
    pub fn buffering(&self) -> bool {
        self.shared.buffering.load(Ordering::Acquire)
    }
    pub fn started(&self) -> bool {
        self.shared.started.load(Ordering::Acquire)
    }
    pub fn finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire)
    }
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }
    pub async fn pause(&self, value: bool) -> Result<(), String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(Command::Pause(value, tx))
            .map_err(|_| "Audio thread has stopped")?;
        tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .map_err(|_| "Audio control timed out")?
            .map_err(|_| "Audio thread has stopped")?
    }
    pub async fn seek(&self, value: f64) -> Result<(), String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(Command::Seek(value, tx))
            .map_err(|_| "Audio thread has stopped")?;
        tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .map_err(|_| "Audio control timed out")?
            .map_err(|_| "Audio thread has stopped")?
    }
    pub async fn stop(mut self) {
        let _ = self.sender.send(Command::Stop);
        if let Some(pcm) = &self.pcm {
            pcm.cancel();
        }
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Stop);
        if let Some(pcm) = &self.pcm {
            pcm.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    #[test]
    fn wav_decode_preserves_mono_amplitude_and_exact_seek() {
        let samples = [-16384_i16, 8192, 16384, -8192];
        let mut file = tempfile::tempfile().unwrap();
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend(44_u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16_u32.to_le_bytes());
        wav.extend(1_u16.to_le_bytes());
        wav.extend(1_u16.to_le_bytes());
        wav.extend(16000_u32.to_le_bytes());
        wav.extend(32000_u32.to_le_bytes());
        wav.extend(2_u16.to_le_bytes());
        wav.extend(16_u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(8_u32.to_le_bytes());
        for value in samples {
            wav.extend(value.to_le_bytes());
        }
        file.write_all(&wav).unwrap();
        file.rewind().unwrap();
        let mut decoder = Decoder::new(file).unwrap();
        assert_eq!(decoder.rate, 16000);
        assert_eq!(decoder.next().unwrap(), Some([-0.5, -0.5]));
        decoder.seek(2.0 / 16000.0).unwrap();
        assert_eq!(decoder.next().unwrap(), Some([0.5, 0.5]));
        assert_eq!(decoder.next().unwrap(), Some([-0.25, -0.25]));
        assert_eq!(decoder.next().unwrap(), None);
    }

    #[tokio::test]
    async fn pcm_prebuffers_recovers_and_drains_a_short_tail() {
        let pcm = Pcm::new(16000, 1, 1000, 250);
        pcm.push(&vec![0; 1600 * 2]).await.unwrap();
        assert_eq!(pcm.frame().unwrap(), PcmFrame::Pending);
        assert_eq!(pcm.metrics(), (100, 0));
        pcm.push(&vec![0; 2400 * 2]).await.unwrap();
        for _ in 0..4000 {
            assert_eq!(pcm.frame().unwrap(), PcmFrame::Sample([0.0, 0.0]));
        }
        for _ in 0..10 {
            assert_eq!(pcm.frame().unwrap(), PcmFrame::Pending);
        }
        assert_eq!(pcm.metrics(), (0, 1));
        pcm.push(&vec![0; 1600 * 2]).await.unwrap();
        assert_eq!(pcm.frame().unwrap(), PcmFrame::Pending);
        // End of synthesis releases a tail smaller than the fill threshold.
        pcm.close();
        for _ in 0..1600 {
            assert_eq!(pcm.frame().unwrap(), PcmFrame::Sample([0.0, 0.0]));
        }
        assert_eq!(pcm.frame().unwrap(), PcmFrame::End);
        assert_eq!(pcm.metrics(), (0, 1));
        let short = Pcm::new(16000, 1, 1000, 250);
        short.push(&[0, 64]).await.unwrap();
        short.close();
        assert_eq!(short.frame().unwrap(), PcmFrame::Sample([0.5, 0.5]));
        assert_eq!(short.frame().unwrap(), PcmFrame::End);
        assert_eq!(short.metrics().1, 0);
    }

    #[tokio::test]
    async fn phrase_cadence_preserves_pcm_but_can_starve_a_1500ms_prebuffer() {
        // Two-phrase server receive trace: 4.226 s of mono PCM over 14.321 s.
        // Playback at the advertised rate must not drop chunks or treat phrase
        // tails as the end of the utterance. This producer is below real time.
        let arrivals = [
            (1926, 4410),
            (2521, 4410),
            (2521, 4410),
            (2981, 4410),
            (2981, 4410),
            (3442, 4410),
            (3442, 4410),
            (3902, 4410),
            (3902, 4410),
            (4369, 4410),
            (4369, 4410),
            (4762, 2690),
            (6679, 4410),
            (7281, 4410),
            (7281, 4410),
            (7743, 4410),
            (7743, 4410),
            (8210, 4410),
            (8211, 4410),
            (8679, 4410),
            (8679, 4410),
            (9148, 4410),
            (9148, 4410),
            (9618, 4410),
            (9618, 4410),
            (10087, 4410),
            (10554, 4410),
            (10554, 4410),
            (11021, 4410),
            (11021, 4410),
            (11489, 4410),
            (11490, 4410),
            (11956, 4410),
            (11957, 4410),
            (12426, 4410),
            (12426, 4410),
            (12894, 4410),
            (12894, 4410),
            (13368, 4410),
            (13838, 4410),
            (13838, 4410),
            (14320, 4410),
            (14321, 2868),
        ];
        let pcm = Pcm::new(44100, 1, 2000, 1500);
        let mut submitted = 0;
        let mut consumed = 0;
        let mut clock = 0;
        for (arrival, frames) in arrivals {
            let next = arrival * 44100_u64 / 1000;
            for _ in clock..next {
                match pcm.frame().unwrap() {
                    PcmFrame::Sample(_) => consumed += 1,
                    PcmFrame::Pending => {}
                    PcmFrame::End => panic!("A phrase boundary ended the stream"),
                }
            }
            clock = next;
            pcm.push(&vec![0; frames * 2]).await.unwrap();
            submitted += frames;
        }
        pcm.close();
        while let PcmFrame::Sample(_) = pcm.frame().unwrap() {
            consumed += 1;
        }
        assert_eq!(
            consumed, submitted,
            "No PCM samples may be dropped between phrases"
        );
        assert!(
            pcm.metrics().1 > 0,
            "This measured source cannot sustain real-time output"
        );
    }

    #[tokio::test]
    async fn live_pcm_backpressure_is_bounded_and_cancellable() {
        let pcm = Pcm::new(16000, 1, 100, 0);
        pcm.push(&vec![0; 1600 * 2]).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), pcm.push(&[0, 0]))
                .await
                .is_err()
        );
        pcm.cancel();
        assert!(pcm.push(&[0, 0]).await.is_err());
        assert!(pcm.push(&[0]).await.is_err());
    }
}
