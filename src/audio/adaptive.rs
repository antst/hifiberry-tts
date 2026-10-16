//! Rate-aware buffering for finite, progressively generated PCM. This uses the
//! same principles as a download buffer: independent ingestion, rate estimates,
//! and a conservative completion fallback when remaining duration is unknown.
use std::{
    fs::File,
    os::unix::fs::FileExt,
    time::{Duration, Instant},
};

pub(super) struct Buffer {
    file: File,
    pub written: u64,
    pub loaded: u64,
    pub consumed_frames: u64,
    max_bytes: u64,
    first: Option<(Instant, u64)>,
    last: Option<Instant>,
    pub source_rate: Option<f64>,
    pub target_frames: usize,
    pub wait_for_complete: bool,
}

impl Buffer {
    pub fn new(max_bytes: u64, prebuffer_frames: usize) -> Result<Self, String> {
        Ok(Self {
            file: tempfile::tempfile().map_err(|e| format!("Cannot create PCM buffer: {e}"))?,
            written: 0,
            loaded: 0,
            consumed_frames: 0,
            max_bytes,
            first: None,
            last: None,
            source_rate: None,
            target_frames: prebuffer_frames,
            wait_for_complete: false,
        })
    }
    pub fn write(
        &mut self,
        bytes: &[u8],
        rate: u32,
        channels: usize,
        played: bool,
        now: Instant,
    ) -> Result<(), String> {
        let length = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or("PCM buffer size overflow")?;
        if length > self.max_bytes {
            return Err("Adaptive PCM buffer exceeds MAX_AUDIO_BYTES".into());
        }
        self.file
            .write_all_at(bytes, self.written)
            .map_err(|e| format!("Cannot spool PCM: {e}"))?;
        self.written = length;
        let frames = self.written / (channels as u64 * 2);
        let (first_at, first_frames) = *self.first.get_or_insert((now, frames));
        let elapsed = now.saturating_duration_since(first_at);
        if let Some(last) = self.last {
            // Jitter reserve grows within the utterance; never shrink it while
            // waiting and oscillate between short buffering thresholds.
            let gap_frames =
                (now.saturating_duration_since(last).as_secs_f64() * rate as f64 * 1.25) as usize;
            self.target_frames = self.target_frames.max(gap_frames.min(rate as usize * 5));
        }
        self.last = Some(now);
        if elapsed >= Duration::from_millis(250) && frames > first_frames {
            let production_rate =
                (frames - first_frames) as f64 / rate as f64 / elapsed.as_secs_f64();
            self.source_rate = Some(production_rate);
            // No duration is advertised by Wyoming. A source below real time
            // cannot be given a finite safe start threshold for an arbitrary
            // remaining phrase. Conservatively finish reception before start.
            if !played && production_rate < 1.05 && !self.wait_for_complete {
                self.wait_for_complete = true;
                eprintln!(
                    "Adaptive PCM: source {:.2}x real time; buffering to completion",
                    production_rate
                );
            }
        }
        Ok(())
    }
    pub fn buffered_frames(&self, channels: usize) -> usize {
        (self.written / (channels as u64 * 2)).saturating_sub(self.consumed_frames) as usize
    }
    pub fn read(&mut self, frames: usize, channels: usize) -> Result<Vec<u8>, String> {
        let bytes = (frames * channels * 2).min((self.written - self.loaded) as usize);
        let mut result = vec![0; bytes];
        self.file
            .read_exact_at(&mut result, self.loaded)
            .map_err(|e| format!("Cannot read PCM buffer: {e}"))?;
        self.loaded += bytes as u64;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slow_estimate_excludes_initial_inference_and_observes_multiple_chunks() {
        let now = Instant::now() + Duration::from_secs(10);
        let mut buffer = Buffer::new(64000, 4000).unwrap();
        buffer.write(&vec![0; 3200], 16000, 1, false, now).unwrap();
        assert!(buffer.source_rate.is_none());
        buffer
            .write(
                &vec![0; 3200],
                16000,
                1,
                false,
                now + Duration::from_millis(500),
            )
            .unwrap();
        buffer
            .write(
                &vec![0; 3200],
                16000,
                1,
                false,
                now + Duration::from_millis(500),
            )
            .unwrap();
        assert!((buffer.source_rate.unwrap() - 0.4).abs() < 0.001);
        assert!(buffer.wait_for_complete);
        assert!(buffer.target_frames >= 8000);
    }
    #[test]
    fn fast_source_keeps_streaming_and_spool_enforces_a_byte_limit() {
        let now = Instant::now();
        let mut buffer = Buffer::new(40000, 4000).unwrap();
        buffer.write(&vec![0; 3200], 16000, 1, false, now).unwrap();
        buffer
            .write(
                &vec![0; 16000],
                16000,
                1,
                false,
                now + Duration::from_millis(250),
            )
            .unwrap();
        assert!((buffer.source_rate.unwrap() - 2.0).abs() < 0.001);
        assert!(!buffer.wait_for_complete);
        assert!(buffer.write(&vec![0; 40000], 16000, 1, false, now).is_err());
        assert_eq!(buffer.written, 19200);
    }
}
