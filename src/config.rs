use serde::Deserialize;
use std::{env, net::SocketAddr, time::Duration};
use url::Url;

#[derive(Clone, Debug)]
pub struct Settings {
    pub volume_level: f64,
    pub duck_factor: f64,
    pub duck_targets: Vec<String>,
}

#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub mpd_bind: Option<SocketAddr>,
    pub wyoming_bind: Option<SocketAddr>,
    pub tts_server: Option<String>,
    pub tts_voice: Option<String>,
    pub pcm_buffer_ms: u32,
    pub pcm_prebuffer_ms: u32,
    pub pcm_adaptive: bool,
    pub token: String,
    pub defaults: Settings,
    pub speech_gain_db: f64,
    pub media_hosts: Vec<String>,
    pub max_audio_bytes: u64,
    pub max_play_time: Duration,
    pub download_timeout: Duration,
    pub pw_dump: String,
    pub pw_cli: String,
    pub audio_device: Option<String>,
    pub force_proto: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayRequest {
    pub url: String,
    pub volume_level: Option<f64>,
    pub duck_factor: Option<f64>,
    pub duck_targets: Option<Vec<String>>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let token = env::var("API_TOKEN").map_err(|_| "API_TOKEN is required")?;
        if token.len() < 24 || !token.bytes().all(|c| c.is_ascii_graphic()) {
            return Err("API_TOKEN must be at least 24 printable ASCII characters".into());
        }
        let defaults = Settings {
            volume_level: read("TTS_VOLUME_LEVEL", "1.0")
                .parse()
                .map_err(|_| "Invalid TTS_VOLUME_LEVEL")?,
            duck_factor: read("DUCK_FACTOR", "0.25")
                .parse()
                .map_err(|_| "Invalid DUCK_FACTOR")?,
            duck_targets: split(&read("DUCK_TARGETS", "*")),
        };
        defaults.validate()?;
        let max_audio_bytes = read("MAX_AUDIO_BYTES", "20971520")
            .parse()
            .map_err(|_| "Invalid MAX_AUDIO_BYTES")?;
        let seconds: u64 = read("MAX_PLAY_SECONDS", "300")
            .parse()
            .map_err(|_| "Invalid MAX_PLAY_SECONDS")?;
        let download_seconds: u64 = read("HTTP_DOWNLOAD_TIMEOUT_SECONDS", "120")
            .parse()
            .map_err(|_| "Invalid HTTP_DOWNLOAD_TIMEOUT_SECONDS")?;
        if max_audio_bytes == 0 || seconds == 0 || download_seconds == 0 {
            return Err("Size and time limits must be positive".into());
        }
        let pcm_buffer_ms: u32 = read("PCM_BUFFER_MS", "1000")
            .parse()
            .map_err(|_| "Invalid PCM_BUFFER_MS")?;
        let pcm_prebuffer_ms: u32 = read("PCM_PREBUFFER_MS", "250")
            .parse()
            .map_err(|_| "Invalid PCM_PREBUFFER_MS")?;
        if !(10..=5000).contains(&pcm_buffer_ms) || pcm_prebuffer_ms > pcm_buffer_ms {
            return Err(
                "PCM_BUFFER_MS must be 10–5000; PCM_PREBUFFER_MS must be 0–PCM_BUFFER_MS".into(),
            );
        }
        Ok(Self {
            pcm_buffer_ms,
            pcm_prebuffer_ms,
            pcm_adaptive: match read("PCM_BUFFERING_MODE", "fixed").as_str() {
                "fixed" => false,
                "adaptive" => true,
                _ => return Err("PCM_BUFFERING_MODE must be fixed or adaptive".into()),
            },
            bind: read("BIND_ADDR", "127.0.0.1:8090")
                .parse()
                .map_err(|_| "Invalid BIND_ADDR")?,
            mpd_bind: match read("MPD_BIND_ADDR", "127.0.0.1:6601").as_str() {
                "" => None,
                value => Some(value.parse().map_err(|_| "Invalid MPD_BIND_ADDR")?),
            },
            token,
            speech_gain_db: parse_speech_gain(&read("SPEECH_GAIN_DB", "0"))?,
            wyoming_bind: match read("WYOMING_BIND_ADDR", "127.0.0.1:10700").as_str() {
                "" => None,
                value => Some(value.parse().map_err(|_| "Invalid WYOMING_BIND_ADDR")?),
            },
            tts_server: env::var("WYOMING_TTS_ADDR").ok().filter(|s| !s.is_empty()),
            tts_voice: env::var("TTS_VOICE").ok().filter(|s| !s.is_empty()),
            defaults,
            media_hosts: split(&read("MEDIA_HOSTS", ""))
                .into_iter()
                .map(|s| s.to_lowercase())
                .collect(),
            max_audio_bytes,
            max_play_time: Duration::from_secs(seconds),
            download_timeout: Duration::from_secs(download_seconds),
            pw_dump: read("PW_DUMP_BIN", "/usr/bin/pw-dump"),
            pw_cli: read("PW_CLI_BIN", "/usr/bin/pw-cli"),
            audio_device: env::var("PIPEWIRE_TARGET").ok().filter(|s| !s.is_empty()),
            force_proto: parse_force_proto(&read("FORCE_PROTO", ""))?,
        })
    }

    pub fn prepare(&self, request: PlayRequest) -> Result<(Url, Settings), String> {
        let url = if request.url.starts_with("wyoming://")
            || request.url.trim_start().starts_with('{')
            || !request.url.contains("://")
        {
            prepare_text(&request.url)?
        } else {
            prepare_url(&request.url, &self.media_hosts, self.force_proto.as_deref())?
        };
        let settings = Settings {
            volume_level: request.volume_level.unwrap_or(self.defaults.volume_level),
            duck_factor: request.duck_factor.unwrap_or(self.defaults.duck_factor),
            duck_targets: request
                .duck_targets
                .unwrap_or_else(|| self.defaults.duck_targets.clone()),
        };
        settings.validate()?;
        Ok((url, settings))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeakRequest {
    pub text: String,
    pub voice: Option<String>,
}

/// Authenticated HTTP input used by the HA adapter. Its selected Wyoming
/// provider supplies the server address; MPD text requests keep using env config.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectRequest {
    pub text: String,
    pub voice: Option<String>,
    pub speaker: Option<String>,
    pub language: Option<String>,
    pub server: Option<String>,
}

pub fn prepare_direct(request: DirectRequest) -> Result<Url, String> {
    let mut url =
        prepare_text(&serde_json::json!({"text":request.text,"voice":request.voice}).to_string())?;
    // An omitted HA voice means the selected engine's default, rather than the
    // bridge's independently configured default for plain MPD text requests.
    if request.voice.is_none() {
        url.query_pairs_mut().append_pair("voice", "");
    }
    for (name, value) in [("speaker", request.speaker), ("language", request.language)] {
        if let Some(value) = value {
            if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                return Err(format!("Invalid {name}"));
            }
            url.query_pairs_mut().append_pair(name, &value);
        }
    }
    if let Some(server) = request.server {
        let parsed = Url::parse(&format!("tcp://{server}"))
            .map_err(|_| "Invalid Wyoming server host:port")?;
        if parsed.host_str().is_none()
            || parsed.port().is_none_or(|p| p == 0)
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || !matches!(parsed.path(), "" | "/")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err("Wyoming server must be a host:port without credentials or a path".into());
        }
        url.query_pairs_mut().append_pair(
            "server",
            &parsed[url::Position::BeforeHost..url::Position::AfterPort],
        );
    }
    Ok(url)
}

pub fn prepare_text(raw: &str) -> Result<Url, String> {
    let request = if raw.starts_with("wyoming://") {
        let url = Url::parse(raw).map_err(|_| "Invalid Wyoming TTS URI")?;
        if url.host_str() != Some("tts")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return Err(
                "Use wyoming://tts?text=...&voice=...; server is configured separately".into(),
            );
        }
        let mut text = None;
        let mut voice = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "text" if text.is_none() => text = Some(value.into_owned()),
                "voice" if voice.is_none() => voice = Some(value.into_owned()),
                _ => return Err("Unknown or repeated Wyoming TTS field".into()),
            }
        }
        SpeakRequest {
            text: text.ok_or("TTS text is required")?,
            voice,
        }
    } else if raw.trim_start().starts_with('{') {
        serde_json::from_str(raw).map_err(|_| "Expected JSON with text and optional voice")?
    } else {
        // Do not turn rejected local-file/other URI requests into spoken text.
        if raw.starts_with('/')
            || raw.starts_with("file:")
            || raw.starts_with("data:")
            || raw.contains("://")
        {
            return Err("Expected text, HTTP(S) URL, or Wyoming TTS URI".into());
        }
        SpeakRequest {
            text: raw.into(),
            voice: None,
        }
    };
    if request.text.trim().is_empty()
        || request.text.len() > 4096
        || request
            .text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err("TTS text must be 1–4096 bytes without control characters".into());
    }
    if request
        .voice
        .as_ref()
        .is_some_and(|v| v.is_empty() || v.len() > 256 || v.chars().any(char::is_control))
    {
        return Err("Invalid TTS voice".into());
    }
    let mut url = Url::parse("wyoming://tts").unwrap();
    url.query_pairs_mut().append_pair("text", &request.text);
    if let Some(voice) = request.voice {
        url.query_pairs_mut().append_pair("voice", &voice);
    }
    Ok(url)
}

pub fn parse_speech_gain(raw: &str) -> Result<f64, String> {
    let db: f64 = raw.parse().map_err(|_| "Invalid SPEECH_GAIN_DB")?;
    if !db.is_finite() || !(-24.0..=24.0).contains(&db) {
        return Err("SPEECH_GAIN_DB must be finite and between -24 and 24 dB".into());
    }
    Ok(db)
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("volume_level", self.volume_level),
            ("duck_factor", self.duck_factor),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(format!("{name} must be between 0 and 1"));
            }
        }
        if self.duck_targets.is_empty()
            || self
                .duck_targets
                .iter()
                .any(|s| s.is_empty() || s.len() > 128 || s.chars().any(char::is_control))
        {
            return Err("duck_targets must contain '*' or one or more process binary names".into());
        }
        Ok(())
    }
}

pub fn validate_url(raw: &str, hosts: &[String]) -> Result<Url, String> {
    if raw.len() > 8192 {
        return Err("URL is too long".into());
    }
    let url = Url::parse(raw).map_err(|_| "Expected an absolute HTTP(S) URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("Only HTTP(S) URLs without credentials or fragments are accepted".into());
    }
    if !hosts.is_empty()
        && !hosts
            .iter()
            .any(|host| Some(host.as_str()) == url.host_str())
    {
        return Err("Media host is not in MEDIA_HOSTS".into());
    }
    Ok(url)
}

fn parse_force_proto(value: &str) -> Result<Option<String>, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" => Ok(None),
        "http" => Ok(Some("http".into())),
        "https" => Ok(Some("https".into())),
        _ => Err("FORCE_PROTO must be empty, http, or https".into()),
    }
}

fn prepare_url(raw: &str, hosts: &[String], force_proto: Option<&str>) -> Result<Url, String> {
    let mut url = validate_url(raw, hosts)?;
    if let Some(proto) = force_proto {
        url.set_scheme(proto)
            .map_err(|_| "Cannot set media URL scheme")?;
    }
    Ok(url)
}

fn read(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.into())
}

fn split(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_voice_and_server_selection_are_explicit() {
        let url = prepare_text(r#"{"text":"Привет: друзья!","voice":"renata-bilingual.s2voice"}"#)
            .unwrap();
        let fields: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(fields["text"], "Привет: друзья!");
        assert_eq!(fields["voice"], "renata-bilingual.s2voice");
        assert!(prepare_text("Hello: kitchen").is_ok());
        assert!(prepare_text(url.as_str()).is_ok());
        for invalid in [
            "",
            "file:///etc/passwd",
            "wyoming://other-server?text=test",
            "wyoming://tts?text=one&text=two",
            "wyoming://tts?text=test&host=other",
        ] {
            assert!(prepare_text(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn url_policy_rejects_local_files_credentials_and_other_hosts() {
        let hosts = vec!["ha.local".into()];
        assert!(validate_url("http://ha.local:8123/api/tts_proxy/a.wav?authSig=x", &hosts).is_ok());
        for raw in [
            "file:///etc/passwd",
            "http://user:pass@ha.local/a",
            "http://other.local/a",
            "http://ha.local/a#fragment",
        ] {
            assert!(validate_url(raw, &hosts).is_err(), "{raw}");
        }
    }

    #[test]
    fn speech_gain_rejects_invalid_or_excessive_values() {
        for db in ["-24", "0", "6", "24"] {
            assert_eq!(parse_speech_gain(db).unwrap(), db.parse::<f64>().unwrap());
        }
        for invalid in ["NaN", "inf", "-inf", "24.1", "-24.1", "bad"] {
            assert!(parse_speech_gain(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn levels_and_targets_are_validated() {
        let mut s = Settings {
            volume_level: 0.7,
            duck_factor: 0.25,
            duck_targets: vec!["*".into()],
        };
        assert!(s.validate().is_ok());
        s.duck_factor = f64::NAN;
        assert!(s.validate().is_err());
        s.duck_factor = 1.1;
        assert!(s.validate().is_err());
        s.duck_factor = 0.0;
        s.duck_targets.clear();
        assert!(s.validate().is_err());
    }

    #[test]
    fn forced_protocol_preserves_audio_path_query_and_nondefault_port() {
        let original = "https://ha.local:8123/api/tts_proxy/test.mp3?authSig=private";
        let rewritten = prepare_url(original, &[], Some("http")).unwrap();
        assert_eq!(
            rewritten.as_str(),
            "http://ha.local:8123/api/tts_proxy/test.mp3?authSig=private"
        );
        assert_eq!(prepare_url(original, &[], None).unwrap().as_str(), original);
        assert_eq!(
            prepare_url("http://ha.local:8123/test.wav", &[], Some("https"))
                .unwrap()
                .as_str(),
            "https://ha.local:8123/test.wav"
        );
        assert!(prepare_url("file:///etc/passwd", &[], Some("http")).is_err());
        assert!(prepare_url(original, &["other.local".into()], Some("http")).is_err());
        assert_eq!(parse_force_proto(" ").unwrap(), None);
        assert_eq!(
            parse_force_proto(" HTTP ").unwrap().as_deref(),
            Some("http")
        );
        assert!(parse_force_proto("ftp").is_err());
    }
}
