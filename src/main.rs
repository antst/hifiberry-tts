mod audio;
mod config;
mod duck;
mod mpd;
mod player;
mod wyoming;

use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use config::{Config, DirectRequest, PlayRequest};
use player::{Control, Status};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{mpsc, watch, RwLock};

#[derive(Clone)]
struct App {
    config: Arc<Config>,
    status: Arc<RwLock<Status>>,
    sender: mpsc::Sender<Control>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Arc::new(Config::from_env().map_err(std::io::Error::other)?);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let mpd_listener = if let Some(bind) = config.mpd_bind {
        Some(tokio::net::TcpListener::bind(bind).await?)
    } else {
        None
    };
    let wyoming_listener = if let Some(bind) = config.wyoming_bind {
        Some(tokio::net::TcpListener::bind(bind).await?)
    } else {
        None
    };
    eprintln!(
        "hifiberry-tts {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        listener.local_addr()?
    );
    if let Some(proto) = &config.force_proto {
        eprintln!("Media URL protocol override enabled: {proto}");
    }
    let (sender, receiver) = mpsc::channel(8);
    let status = Arc::new(RwLock::new(Status::default()));
    {
        let mut s = status.write().await;
        s.volume_level = config.defaults.volume_level;
        s.pcm_prebuffer_ms = config.pcm_prebuffer_ms;
        s.pcm_buffer_limit_ms = config.pcm_buffer_ms;
    }
    let worker = tokio::spawn(player::run(config.clone(), status.clone(), receiver));
    let (cancel_mpd, cancelled) = watch::channel(false);
    let mpd_worker = mpd_listener.map(|listener| {
        tokio::spawn(mpd::serve(
            listener,
            config.clone(),
            status.clone(),
            sender.clone(),
            cancelled.clone(),
        ))
    });
    let wyoming_worker = wyoming_listener.map(|listener| {
        tokio::spawn(wyoming::serve(
            listener,
            config.clone(),
            status.clone(),
            sender.clone(),
            cancelled,
        ))
    });
    let app = App {
        config,
        status,
        sender: sender.clone(),
    };
    let result = axum::serve(listener, router(app))
        .with_graceful_shutdown(shutdown())
        .await;
    let _ = sender.send(Control::Shutdown).await;
    let _ = cancel_mpd.send(true);
    if let Some(task) = mpd_worker {
        task.await?;
    }
    if let Some(task) = wyoming_worker {
        task.await?;
    }
    worker.await?;
    result?;
    Ok(())
}

fn router(app: App) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/play_media", post(play_media))
        .route("/v1/speak", post(speak))
        .route("/v1/pause", post(pause))
        .route("/v1/volume", post(volume))
        .route("/v1/seek", post(seek))
        .route("/v1/play", post(play))
        .route("/v1/stop", post(stop))
        .layer(DefaultBodyLimit::max(16384))
        .layer(middleware::from_fn_with_state(app.clone(), authenticate))
        .with_state(app)
}

async fn authenticate(State(app): State<App>, request: Request, next: Next) -> Response {
    let expected = format!("Bearer {}", app.config.token);
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    let equal = supplied.len() == expected.len()
        && supplied
            .iter()
            .zip(expected.bytes())
            .fold(0u8, |a, (b, c)| a | (b ^ c))
            == 0;
    if !equal {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"Unauthorized"})),
        )
            .into_response();
    }
    next.run(request).await
}

async fn status(State(app): State<App>) -> Json<Status> {
    Json(app.status.read().await.clone())
}

async fn play_media(State(app): State<App>, Json(request): Json<PlayRequest>) -> Response {
    match app.config.prepare(request) {
        Ok((url, settings)) => enqueue(&app, Control::Load(url, settings)),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response(),
    }
}

async fn speak(State(app): State<App>, Json(request): Json<DirectRequest>) -> Response {
    match config::prepare_direct(request) {
        Ok(url) => {
            let mut settings = app.config.defaults.clone();
            settings.volume_level = app.status.read().await.volume_level;
            enqueue(&app, Control::Load(url, settings))
        }
        Err(error) => bad_request(error),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PauseRequest {
    paused: bool,
}
async fn pause(State(app): State<App>, Json(request): Json<PauseRequest>) -> Response {
    enqueue(&app, Control::Pause(request.paused))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VolumeRequest {
    volume_level: f64,
}
async fn volume(State(app): State<App>, Json(request): Json<VolumeRequest>) -> Response {
    if !request.volume_level.is_finite() || !(0.0..=1.0).contains(&request.volume_level) {
        return bad_request("Volume must be between 0 and 1".into());
    }
    enqueue(&app, Control::Volume(request.volume_level))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SeekRequest {
    position: f64,
}
async fn seek(State(app): State<App>, Json(request): Json<SeekRequest>) -> Response {
    if !request.position.is_finite() || request.position < 0.0 {
        return bad_request("Seek position must be finite and nonnegative".into());
    }
    enqueue(&app, Control::Seek(request.position))
}
fn bad_request(error: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response()
}

async fn play(State(app): State<App>) -> Response {
    let s = app.status.read().await;
    if !s.has_media && s.state != "buffering" {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"Use play_media first; there is no cached media"})),
        )
            .into_response();
    }
    drop(s);
    enqueue(&app, Control::Play)
}

async fn stop(State(app): State<App>) -> Response {
    enqueue(&app, Control::Stop)
}

fn enqueue(app: &App, command: Control) -> Response {
    match app.sender.try_send(command) {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"accepted":true}))).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"Player is busy or unavailable; retry"})),
        )
            .into_response(),
    }
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use axum::{body::Body, http::Request};
    use std::time::Duration;
    use tower::ServiceExt;

    fn test_app() -> (App, mpsc::Receiver<Control>) {
        let (sender, receiver) = mpsc::channel(8);
        (
            App {
                sender,
                status: Arc::new(RwLock::new(Status::default())),
                config: Arc::new(Config {
                    bind: "127.0.0.1:0".parse().unwrap(),
                    mpd_bind: None,
                    token: "test-token-at-least-24-characters".into(),
                    defaults: Settings {
                        volume_level: 0.7,
                        duck_factor: 0.25,
                        duck_targets: vec!["*".into()],
                    },
                    media_hosts: vec!["ha.local".into()],
                    max_audio_bytes: 1024,
                    max_play_time: Duration::from_secs(10),
                    wyoming_bind: None,
                    tts_server: None,
                    tts_voice: None,
                    pcm_buffer_ms: 1000,
                    pcm_prebuffer_ms: 250,
                    pw_dump: "pw-dump".into(),
                    pw_cli: "pw-cli".into(),
                    audio_device: None,
                    force_proto: None,
                }),
            },
            receiver,
        )
    }

    #[tokio::test]
    async fn direct_speech_and_native_controls_validate_before_enqueueing() {
        let (app, mut receiver) = test_app();
        let router = router(app);
        let make = |path: &str, body: &str| {
            Request::builder()
                .method("POST")
                .uri(path)
                .header("Authorization", "Bearer test-token-at-least-24-characters")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap()
        };
        for (path, body) in [
            ("/v1/speak", r#"{"text":"test","server":"host:10200/path"}"#),
            (
                "/v1/speak",
                r#"{"text":"test","server":"user:pass@host:10200"}"#,
            ),
            ("/v1/volume", r#"{"volume_level":1.1}"#),
            ("/v1/seek", r#"{"position":-1}"#),
        ] {
            assert_eq!(
                router
                    .clone()
                    .oneshot(make(path, body))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST
            );
            assert!(receiver.try_recv().is_err());
        }
        assert_eq!(router.clone().oneshot(make("/v1/speak", r#"{"text":"test","voice":"renata","language":"ru","speaker":"2","server":"[::1]:10200"}"#)).await.unwrap().status(), StatusCode::ACCEPTED);
        match receiver.recv().await.unwrap() {
            Control::Load(url, _) => {
                let pairs: std::collections::HashMap<_, _> =
                    url.query_pairs().into_owned().collect();
                assert_eq!(pairs["server"], "[::1]:10200");
                assert_eq!(pairs["speaker"], "2");
                assert_eq!(pairs["voice"], "renata");
            }
            _ => panic!("Expected speech load"),
        }
        for (path, body) in [
            ("/v1/pause", r#"{"paused":true}"#),
            ("/v1/volume", r#"{"volume_level":0.8}"#),
            ("/v1/seek", r#"{"position":1.5}"#),
        ] {
            assert_eq!(
                router
                    .clone()
                    .oneshot(make(path, body))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::ACCEPTED
            );
        }
        assert!(matches!(receiver.recv().await, Some(Control::Pause(true))));
        assert!(matches!(receiver.recv().await, Some(Control::Volume(v)) if v == 0.8));
        assert!(matches!(receiver.recv().await, Some(Control::Seek(v)) if v == 1.5));
    }

    #[tokio::test]
    async fn api_auth_url_policy_and_play_semantics() {
        let (app, mut receiver) = test_app();
        let router = router(app);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let make = |path: &str, body: &str| {
            Request::builder()
                .method("POST")
                .uri(path)
                .header("Authorization", "Bearer test-token-at-least-24-characters")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap()
        };
        assert_eq!(
            router
                .clone()
                .oneshot(make("/v1/play", ""))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            router
                .clone()
                .oneshot(make("/v1/play_media", r#"{"url":"file:///etc/passwd"}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            router
                .oneshot(make(
                    "/v1/play_media",
                    r#"{"url":"http://ha.local:8123/a.wav","duck_factor":0.2}"#
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        match receiver.recv().await.unwrap() {
            Control::Load(_, settings) => assert_eq!(settings.duck_factor, 0.2),
            _ => panic!("Expected Load"),
        }
    }
}
