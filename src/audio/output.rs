use super::{Command, Decoder, LevelMeter, Pcm, PcmFrame, Shared, Source};
use pipewire::{self as pw, properties::properties, spa};
use std::{
    cell::RefCell,
    io::Cursor,
    rc::Rc,
    sync::{atomic::Ordering, mpsc, Arc},
    time::{Duration, Instant},
};

enum Reader {
    File(Box<Decoder>),
    Pcm(Arc<Pcm>),
}
enum Frame {
    Sample([f32; 2]),
    Pending,
    End,
}
struct Session {
    reader: Reader,
    state: Arc<Shared>,
    frame: u64,
    rate: u32,
    draining: bool,
    paused: bool,
}
impl Session {
    fn next(&mut self) -> Result<Frame, String> {
        match &mut self.reader {
            Reader::File(decoder) => Ok(match decoder.next()? {
                Some(frame) => Frame::Sample(frame),
                None => Frame::End,
            }),
            Reader::Pcm(pcm) => Ok(match pcm.frame()? {
                PcmFrame::Sample(frame) => Frame::Sample(frame),
                PcmFrame::Pending => Frame::Pending,
                PcmFrame::End => Frame::End,
            }),
        }
    }
}

pub(super) fn run(
    source: Source,
    target: Option<String>,
    receiver: mpsc::Receiver<Command>,
    state: Arc<Shared>,
) -> Result<(), String> {
    let opened = Instant::now();
    let (reader, rate, duration) = match source {
        Source::File(file) => {
            let d = Decoder::new(file)?;
            let rate = d.rate;
            let duration = d.duration;
            (Reader::File(Box::new(d)), rate, duration)
        }
        Source::Pcm(pcm) => {
            let rate = pcm.rate;
            (Reader::Pcm(pcm), rate, None)
        }
    };
    state
        .duration
        .store(duration.unwrap_or(f64::NAN).to_bits(), Ordering::Release);
    let data = Rc::new(RefCell::new(Session {
        reader,
        state: state.clone(),
        frame: 0,
        rate,
        draining: false,
        paused: false,
    }));
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("Cannot connect to user PipeWire: {e}"))?;
    let mut props = properties! {
        "application.name" => "hifiberry-tts",
        "node.name" => "hifiberry-tts",
        "media.name" => "Announcement",
        "media.type" => "Audio",
        "media.category" => "Playback",
        "media.role" => "Notification",
        "node.latency" => format!("{}/{}", rate / 50, rate),
        "audio.channels" => "2",
    };
    if let Some(target) = target {
        props.insert("target.object", target);
    }
    let stream = pw::stream::StreamRc::new(core.clone(), "hifiberry-tts", props)
        .map_err(|e| e.to_string())?;
    let error_loop = mainloop.clone();
    let error_state = state.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _, _, message| {
            if id == 0 {
                *error_state.error.lock().unwrap() =
                    Some(format!("PipeWire connection: {message}"));
                error_loop.quit();
            }
        })
        .register();
    let state_loop = mainloop.clone();
    let drain_loop = mainloop.clone();
    let _listener = stream
        .add_local_listener_with_user_data(data.clone())
        .state_changed(move |_, data, _, new| {
            if let pw::stream::StreamState::Error(error) = new {
                *data.borrow().state.error.lock().unwrap() =
                    Some(format!("PipeWire output: {error}"));
                state_loop.quit();
            }
        })
        .drained(move |_, _| drain_loop.quit())
        .process(move |stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let datas = buffer.datas_mut();
            let Some(output) = datas.first_mut() else {
                return;
            };
            let mut session = data.borrow_mut();
            let mut count = 0;
            let mut real_frames = 0;
            let mut buffering = false;
            if let Some(bytes) = output.data() {
                let frames = (bytes.len() / 8).min(if requested > 0 {
                    requested
                } else {
                    session.rate as usize / 100
                });
                let volume = f32::from_bits(session.state.volume.load(Ordering::Acquire))
                    * session.state.speech_gain;
                let mut meter = LevelMeter::default();
                for frame in bytes[..frames * 8].as_chunks_mut::<8>().0 {
                    if session.draining || session.paused {
                        break;
                    }
                    let samples = match session.next() {
                        Ok(Frame::Sample(samples)) => {
                            session.frame += 1;
                            real_frames += 1;
                            samples
                        }
                        Ok(Frame::Pending) => {
                            buffering = true;
                            [0.0, 0.0]
                        }
                        Ok(Frame::End) => {
                            session.draining = true;
                            break;
                        }
                        Err(error) => {
                            *session.state.error.lock().unwrap() = Some(error);
                            session.draining = true;
                            break;
                        }
                    };
                    for (value, destination) in
                        samples.into_iter().zip(frame.as_chunks_mut::<4>().0)
                    {
                        let value = meter.sample(value, volume);
                        destination.copy_from_slice(&value.to_le_bytes());
                    }
                    count += 1;
                }
                meter.publish(&session.state);
            }
            *output.chunk_mut().offset_mut() = 0;
            *output.chunk_mut().stride_mut() = 8;
            *output.chunk_mut().size_mut() = (count * 8) as u32;
            session.state.buffering.store(buffering, Ordering::Release);
            session.state.position.store(
                (session.frame as f64 / session.rate as f64).to_bits(),
                Ordering::Release,
            );
            if real_frames > 0 && !session.state.started.swap(true, Ordering::AcqRel) {
                eprintln!(
                    "Native PipeWire first audio buffer: {} ms, {} Hz stereo",
                    opened.elapsed().as_millis(),
                    session.rate
                );
            }
        })
        .register()
        .map_err(|e| e.to_string())?;
    let timer_loop = mainloop.clone();
    let timer_stream = stream.clone();
    let timer_data = data.clone();
    let drain_sent = Rc::new(std::cell::Cell::new(false));
    let timer = mainloop.loop_().add_timer(move |_| {
        loop {
            match receiver.try_recv() {
                Ok(Command::Stop) | Err(mpsc::TryRecvError::Disconnected) => {
                    timer_loop.quit();
                    return;
                }
                Ok(Command::Pause(paused, reply)) => {
                    let result = timer_stream.set_active(!paused).map_err(|e| e.to_string());
                    if result.is_ok() {
                        timer_data.borrow_mut().paused = paused;
                    }
                    let _ = reply.send(result);
                }
                Ok(Command::Seek(seconds, reply)) => {
                    let mut data = timer_data.borrow_mut();
                    let result = match &mut data.reader {
                        Reader::File(decoder) => decoder.seek(seconds),
                        Reader::Pcm(_) => {
                            Err("Seeking a live Wyoming stream is unsupported".into())
                        }
                    };
                    if result.is_ok() {
                        let _ = timer_stream.flush(false);
                        data.draining = false;
                        drain_sent.set(false);
                        data.frame = (seconds * data.rate as f64).round() as u64;
                        data.state
                            .position
                            .store(seconds.to_bits(), Ordering::Release);
                    }
                    let _ = reply.send(result);
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        let data = timer_data.borrow();
        if data.draining && !data.paused && !drain_sent.replace(true) {
            if let Err(error) = timer_stream.flush(true) {
                *data.state.error.lock().unwrap() = Some(error.to_string());
                timer_loop.quit();
            }
        }
    });
    timer
        .update_timer(
            Some(Duration::from_millis(5)),
            Some(Duration::from_millis(5)),
        )
        .into_result()
        .map_err(|e| e.to_string())?;
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(rate);
    info.set_channels(2);
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    positions[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    positions[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(positions);
    let values = spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|e| format!("Cannot describe audio format: {e}"))?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&values).ok_or("Invalid PipeWire format")?];
    // No RT_PROCESS: decoding/allocation occurs on this dedicated main loop,
    // not the server's realtime callback. No additional PCM prebuffer is used.
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| e.to_string())?;
    mainloop.run();
    stream.disconnect().map_err(|e| e.to_string())?;
    Ok(())
}
