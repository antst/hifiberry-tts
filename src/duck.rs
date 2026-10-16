//! Per-stream software gain only. Never writes sink, device, or ALSA master gain.
use crate::config::{Config, Settings};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::{process::Command, time::timeout};

#[derive(Clone, Debug)]
struct Stream {
    id: u64,
    serial: String,
    key: &'static str,
    volumes: Vec<f64>,
}

#[derive(Default)]
pub struct Duck {
    saved: Vec<(Stream, Vec<f64>)>,
    pub warnings: Vec<String>,
}

impl Duck {
    pub fn count(&self) -> usize {
        self.saved.len()
    }

    pub async fn apply(config: &Config, settings: &Settings) -> Self {
        let mut duck = Self::default();
        if settings.duck_factor == 1.0 {
            return duck;
        }
        let objects = match dump(config).await {
            Ok(value) => value,
            Err(error) => {
                duck.warnings.push(error);
                return duck;
            }
        };
        let mut unsupported = 0;
        for object in &objects {
            if !selected(object, &settings.duck_targets, &objects) {
                continue;
            }
            let Some(stream) = parse_stream(object) else {
                unsupported += 1;
                continue;
            };
            let lowered = stream
                .volumes
                .iter()
                .map(|v| v * settings.duck_factor)
                .collect::<Vec<_>>();
            // Save before the write, including writes whose acknowledgement fails.
            duck.saved.push((stream.clone(), lowered.clone()));
            if let Err(error) = set(config, &stream, &lowered).await {
                duck.warnings.push(error);
            }
        }
        if unsupported > 0 {
            duck.warnings.push(format!(
                "{unsupported} playing stream(s) have no readable software volume/serial"
            ));
        }
        if !duck.saved.is_empty() {
            match dump(config).await {
                Ok(current) => {
                    for (stream, applied) in &duck.saved {
                        if !current
                            .iter()
                            .filter_map(parse_stream)
                            .any(|s| same_stream(stream, &s) && close(&s.volumes, applied))
                        {
                            duck.warnings.push(format!(
                                "Stream {} did not confirm ducked volume",
                                stream.id
                            ));
                        }
                    }
                }
                Err(error) => duck.warnings.push(error),
            }
        }
        duck
    }

    pub async fn restore(mut self, config: &Config) -> Vec<String> {
        if self.saved.is_empty() {
            return self.warnings;
        }
        match dump(config).await {
            Ok(current) => {
                for (saved, applied) in self.saved {
                    let current = current
                        .iter()
                        .filter_map(parse_stream)
                        .find(|s| same_stream(&saved, s));
                    // Object IDs may be reused. Also respect a user/player volume change.
                    if let Some(current) = current {
                        if close(&current.volumes, &applied) {
                            if let Err(error) = set(config, &saved, &saved.volumes).await {
                                self.warnings.push(error);
                            }
                        }
                    }
                }
            }
            Err(error) => self
                .warnings
                .push(format!("Cannot restore stream volume: {error}")),
        }
        self.warnings
    }
}

fn same_stream(a: &Stream, b: &Stream) -> bool {
    a.id == b.id && a.serial == b.serial && a.key == b.key
}

fn close(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| (a - b).abs() < 0.0001)
}

fn selected(object: &Value, targets: &[String], objects: &[Value]) -> bool {
    let props = &object["info"]["props"];
    // NG's RIAA and Speaker EQ outputs also use Stream/Output/Audio. Their
    // linked Audio/Sink is a shared processing stage, not a music source.
    // Ducking it would attenuate announcements and could multiply attenuation.
    let shared_stage = props["node.link-group"].as_str().is_some_and(|group| {
        objects.iter().any(|other| {
            other["info"]["props"]["media.class"] == "Audio/Sink"
                && other["info"]["props"]["node.link-group"].as_str() == Some(group)
        })
    });
    let client = objects.iter().find(|other| {
        other["type"] == "PipeWire:Interface:Client" && other["id"] == props["client.id"]
    });
    // ALSA-over-PipeWire players (including NG's librespot) put the binary
    // name on their Client, rather than on the playback Node itself.
    let property = |name: &str| {
        props.get(name).and_then(Value::as_str).or_else(|| {
            client
                .and_then(|c| c["info"]["props"].get(name))
                .and_then(Value::as_str)
        })
    };
    object["type"] == "PipeWire:Interface:Node"
        && props["media.class"] == "Stream/Output/Audio"
        && object["info"]["state"] == "running"
        && !shared_stage
        && property("application.name") != Some("hifiberry-tts")
        && (targets.iter().any(|s| s == "*")
            || targets
                .iter()
                .any(|s| property("application.process.binary") == Some(s)))
}

fn parse_stream(object: &Value) -> Option<Stream> {
    if object["type"] != "PipeWire:Interface:Node"
        || object["info"]["props"]["media.class"] != "Stream/Output/Audio"
    {
        return None;
    }
    let id = object["id"].as_u64()?;
    let serial = match &object["info"]["props"]["object.serial"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    for props in object["info"]["params"]["Props"].as_array()? {
        if let Some(volumes) = props["channelVolumes"].as_array() {
            let volumes: Option<Vec<f64>> = volumes.iter().map(Value::as_f64).collect();
            if let Some(volumes) =
                volumes.filter(|v| !v.is_empty() && v.iter().all(|v| v.is_finite() && *v >= 0.0))
            {
                return Some(Stream {
                    id,
                    serial,
                    key: "channelVolumes",
                    volumes,
                });
            }
        }
    }
    None
}

async fn dump(config: &Config) -> Result<Vec<Value>, String> {
    let bytes = run(&config.pw_dump, &[])
        .await
        .map_err(|_| "pw-dump failed (check the PipeWire session/user)".to_owned())?;
    serde_json::from_slice(&bytes).map_err(|_| "pw-dump did not return a JSON array".into())
}

async fn set(config: &Config, stream: &Stream, volumes: &[f64]) -> Result<(), String> {
    let payload = json!({stream.key: volumes}).to_string();
    run(
        &config.pw_cli,
        &["set-param", &stream.id.to_string(), "Props", &payload],
    )
    .await
    .map(|_| ())
    .map_err(|_| format!("Cannot set software volume for stream {}", stream.id))
}

async fn run(binary: &str, args: &[&str]) -> Result<Vec<u8>, ()> {
    let output = timeout(
        Duration::from_secs(3),
        Command::new(binary)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(class: &str, binary: &str, state: &str) -> Value {
        json!({"id": 42, "type":"PipeWire:Interface:Node", "info":{
            "state": state, "props":{"object.serial":100, "media.class":class,
            "application.name":binary, "application.process.binary":binary},
            "params":{"Props":[{"channelVolumes":[0.8,0.4], "mute":false}]}
        }})
    }

    #[test]
    fn all_selects_playing_sources_but_never_master_capture_or_own_stream() {
        let targets = vec!["*".into()];
        assert!(selected(
            &node("Stream/Output/Audio", "anything", "running"),
            &targets,
            &[]
        ));
        for (class, binary, state) in [
            ("Audio/Sink", "alsa", "running"),
            ("Stream/Input/Audio", "capture", "running"),
            ("Stream/Output/Audio", "hifiberry-tts", "running"),
            ("Stream/Output/Audio", "mpd", "idle"),
        ] {
            assert!(!selected(&node(class, binary, state), &targets, &[]));
        }
    }

    #[test]
    fn named_targets_and_identity_guard() {
        let original = node("Stream/Output/Audio", "mpd", "running");
        assert!(selected(&original, &["mpd".into()], &[]));
        assert!(!selected(&original, &["librespot".into()], &[]));
        let saved = parse_stream(&original).unwrap();
        assert_eq!(saved.volumes, vec![0.8, 0.4]);
        let mut recycled = original.clone();
        recycled["info"]["props"]["object.serial"] = json!(101);
        assert!(!same_stream(&saved, &parse_stream(&recycled).unwrap()));
        assert!(!close(&saved.volumes, &[0.2, 0.1]));
        assert!(close(&[0.20000001, 0.1], &[0.2, 0.1]));
    }

    #[test]
    fn ng_shared_dsp_stages_are_excluded_and_alsa_client_binary_is_resolved() {
        let sink = json!({"id":39,"type":"PipeWire:Interface:Node","info":{"props":{
            "media.class":"Audio/Sink","node.link-group":"filter-chain-29"}}});
        let mut eq_output = node("Stream/Output/Audio", "pipewire", "running");
        eq_output["info"]["props"]["node.link-group"] = json!("filter-chain-29");
        let mut alsa = node(
            "Stream/Output/Audio",
            "PipeWire ALSA [librespot]",
            "running",
        );
        alsa["info"]["props"]
            .as_object_mut()
            .unwrap()
            .remove("application.process.binary");
        alsa["info"]["props"]["client.id"] = json!(103);
        let client = json!({"id":103,"type":"PipeWire:Interface:Client","info":{
            "props":{"application.process.binary":"librespot"}}});
        let objects = vec![sink, eq_output.clone(), alsa.clone(), client];
        assert!(!selected(&eq_output, &["*".into()], &objects));
        assert!(selected(&alsa, &["*".into()], &objects));
        assert!(selected(&alsa, &["librespot".into()], &objects));
        assert!(!selected(&alsa, &["mpd".into()], &objects));
    }
}
