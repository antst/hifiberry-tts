//! A deliberately small, single-item MPD protocol adapter for HA's built-in MPD integration.
//! It shares the announcement worker; it does not control NG's MPD daemon.
use crate::{
    config::{Config, PlayRequest},
    player::{Control, Status},
};
use std::{io, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch, Mutex, RwLock, Semaphore},
};
use url::Url;

const COMMANDS: &[&str] = &[
    "password",
    "ping",
    "close",
    "commands",
    "status",
    "currentsong",
    "playlistinfo",
    "playlistid",
    "listplaylists",
    "clear",
    "add",
    "addid",
    "play",
    "playid",
    "stop",
    "pause",
    "setvol",
    "getvol",
    "seekcur",
    "seek",
    "seekid",
    "next",
    "previous",
    "repeat",
    "single",
    "random",
    "consume",
    "outputs",
    "tagtypes",
    "urlhandlers",
];

struct Queue {
    url: Option<Url>,
    version: u64,
    needs_load: bool,
    volume: f64,
}

struct Server {
    config: Arc<Config>,
    status: Arc<RwLock<Status>>,
    sender: mpsc::Sender<Control>,
    queue: Mutex<Queue>,
}

pub async fn serve(
    listener: TcpListener,
    config: Arc<Config>,
    status: Arc<RwLock<Status>>,
    sender: mpsc::Sender<Control>,
    mut cancelled: watch::Receiver<bool>,
) {
    eprintln!(
        "MPD announcement interface listening on {}",
        listener.local_addr().unwrap()
    );
    let volume = config.defaults.volume_level;
    let server = Arc::new(Server {
        config,
        status,
        sender,
        queue: Mutex::new(Queue {
            url: None,
            version: 0,
            needs_load: false,
            volume,
        }),
    });
    let slots = Arc::new(Semaphore::new(16));
    loop {
        tokio::select! {
            _ = cancelled.changed() => break,
            connection = listener.accept() => {
                let Ok((socket, _)) = connection else { break; };
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                let server = server.clone();
                let mut cancelled = cancelled.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    tokio::select! {
                        _ = cancelled.changed() => {},
                        _ = session(socket, server) => {},
                    }
                });
            }
        }
    }
}

async fn session(socket: TcpStream, server: Arc<Server>) -> io::Result<()> {
    let (read, mut write) = socket.into_split();
    let mut reader = BufReader::new(read);
    write.write_all(b"OK MPD 0.23.0\n").await?;
    let mut authenticated = false;
    let mut batch: Option<(bool, Vec<Vec<String>>)> = None;
    loop {
        let line = match tokio::time::timeout(Duration::from_secs(60), read_line(&mut reader)).await
        {
            Ok(Ok(Some(line))) => line,
            _ => return Ok(()),
        };
        let words = match parse(&line) {
            Ok(words) if !words.is_empty() => words,
            _ => {
                write
                    .write_all(b"ACK [2@0] {} Invalid command syntax\n")
                    .await?;
                continue;
            }
        };
        if words[0] == "command_list_begin" || words[0] == "command_list_ok_begin" {
            if batch.is_some() {
                return Ok(());
            }
            batch = Some((words[0] == "command_list_ok_begin", Vec::new()));
            continue;
        }
        if words[0] == "command_list_end" {
            let Some((list_ok, commands)) = batch.take() else {
                write
                    .write_all(b"ACK [2@0] {} Not in command list\n")
                    .await?;
                continue;
            };
            let mut failed = false;
            for (index, command) in commands.iter().enumerate() {
                match execute(&server, command, &mut authenticated).await {
                    Ok(output) => {
                        write.write_all(output.as_bytes()).await?;
                        if list_ok {
                            write.write_all(b"list_OK\n").await?;
                        }
                    }
                    Err((code, message)) => {
                        write
                            .write_all(
                                format!("ACK [{code}@{index}] {{{}}} {message}\n", command[0])
                                    .as_bytes(),
                            )
                            .await?;
                        failed = true;
                        break;
                    }
                }
            }
            if !failed {
                write.write_all(b"OK\n").await?;
            }
            continue;
        }
        if let Some((_, commands)) = batch.as_mut() {
            if commands.len() >= 64 {
                return Ok(());
            }
            commands.push(words);
            continue;
        }
        match execute(&server, &words, &mut authenticated).await {
            Ok(output) => {
                write.write_all(output.as_bytes()).await?;
                write.write_all(b"OK\n").await?;
                if words[0] == "close" {
                    return Ok(());
                }
            }
            Err((code, message)) => {
                write
                    .write_all(format!("ACK [{code}@0] {{{}}} {message}\n", words[0]).as_bytes())
                    .await?
            }
        }
    }
}

type Failure = (u8, &'static str);

async fn execute(
    server: &Server,
    words: &[String],
    authenticated: &mut bool,
) -> Result<String, Failure> {
    let command = words[0].as_str();
    let args = &words[1..];
    if command == "password" {
        let password = args.first().ok_or((2, "Password required"))?;
        let expected = server.config.token.as_bytes();
        let equal = password.len() == expected.len()
            && password
                .bytes()
                .zip(expected)
                .fold(0u8, |v, (a, b)| v | (a ^ b))
                == 0;
        *authenticated = equal;
        return if equal {
            Ok(String::new())
        } else {
            Err((3, "Incorrect password"))
        };
    }
    if command == "ping" || command == "close" {
        return Ok(String::new());
    }
    if !*authenticated {
        return Err((4, "Authentication required"));
    }
    let mut queue = server.queue.lock().await;
    let state = server.status.read().await.clone();
    let send = |control| {
        server
            .sender
            .try_send(control)
            .map_err(|_| (52, "Announcement worker busy; retry"))
    };
    let argument = |index: usize| args.get(index).ok_or((2, "Missing argument"));
    let current = || -> String {
        match &queue.url {
            Some(url) => format!("file: {url}\nTitle: Announcement\nPos: 0\nId: 1\n"),
            None => String::new(),
        }
    };
    match command {
        "commands" => Ok(COMMANDS
            .iter()
            .map(|name| format!("command: {name}\n"))
            .collect()),
        "status" => {
            let mode = match state.state {
                "playing" | "buffering" => "play",
                "paused" => "pause",
                _ => "stop",
            };
            let mut output = format!("volume: {}\nrepeat: 0\nrandom: 0\nsingle: 1\nconsume: 0\nplaylist: {}\nplaylistlength: {}\nstate: {mode}\n",
                (queue.volume*100.0).round() as u32, queue.version, usize::from(queue.url.is_some()));
            if queue.url.is_some() {
                output.push_str(&format!(
                    "song: 0\nsongid: 1\nelapsed: {:.3}\n",
                    state.position
                ));
                if let Some(duration) = state.duration {
                    output.push_str(&format!(
                        "duration: {duration:.3}\ntime: {}:{}\n",
                        state.position as u64, duration as u64
                    ));
                }
            }
            if let Some(error) = state.last_error {
                output.push_str(&format!("error: {}\n", error.replace(['\r', '\n'], " ")));
            }
            Ok(output)
        }
        "currentsong" | "playlistinfo" | "playlistid" => Ok(current()),
        "listplaylists" => Ok(String::new()),
        "clear" => {
            send(Control::Stop)?;
            queue.url = None;
            queue.needs_load = false;
            queue.version += 1;
            Ok(String::new())
        }
        "add" | "addid" => {
            if queue.url.is_some() {
                return Err((50, "Only one announcement is supported; clear first"));
            }
            let (url, _) = server
                .config
                .prepare(PlayRequest {
                    url: argument(0)?.clone(),
                    volume_level: None,
                    duck_factor: None,
                    duck_targets: None,
                })
                .map_err(|_| (2, "Expected an allowed HTTP(S) audio URL"))?;
            queue.url = Some(url);
            queue.needs_load = true;
            queue.version += 1;
            Ok(if command == "addid" {
                "Id: 1\n".into()
            } else {
                String::new()
            })
        }
        "play" | "playid" => {
            if let Some(index) = args.first() {
                if index != if command == "playid" { "1" } else { "0" } {
                    return Err((50, "No such song"));
                }
            }
            let url = queue
                .url
                .clone()
                .ok_or((50, "No announcement; use play_media first"))?;
            if queue.needs_load || !state.has_media {
                let mut settings = server.config.defaults.clone();
                settings.volume_level = queue.volume;
                send(Control::Load(url, settings))?;
                queue.needs_load = false;
            } else {
                send(Control::Play)?;
            }
            Ok(String::new())
        }
        "stop" | "next" => {
            send(Control::Stop)?;
            Ok(String::new())
        }
        "previous" => {
            send(Control::Seek(0.0))?;
            if state.state == "idle" {
                send(Control::Play)?;
            }
            Ok(String::new())
        }
        "pause" => {
            let paused = match args.first().map(String::as_str) {
                Some("1") => true,
                Some("0") => false,
                None => state.state != "paused",
                _ => return Err((2, "Expected 0 or 1")),
            };
            send(Control::Pause(paused))?;
            Ok(String::new())
        }
        "setvol" => {
            let volume: u8 = argument(0)?
                .parse()
                .map_err(|_| (2, "Volume must be 0..100"))?;
            if volume > 100 {
                return Err((2, "Volume must be 0..100"));
            }
            let volume = f64::from(volume) / 100.0;
            send(Control::Volume(volume))?;
            queue.volume = volume;
            Ok(String::new())
        }
        "getvol" => Ok(format!(
            "volume: {}\n",
            (queue.volume * 100.0).round() as u32
        )),
        "seekcur" | "seek" | "seekid" => {
            let raw = argument(if command == "seekcur" { 0 } else { 1 })?;
            let mut position: f64 = raw.parse().map_err(|_| (2, "Invalid seek position"))?;
            if command == "seekcur" && (raw.starts_with('+') || raw.starts_with('-')) {
                position += state.position;
            }
            if !position.is_finite() || position < 0.0 {
                return Err((2, "Invalid seek position"));
            }
            send(Control::Seek(position))?;
            Ok(String::new())
        }
        "repeat" | "random" | "consume" => {
            if argument(0)? != "0" {
                return Err((5, "Queue, repeat and shuffle are not supported"));
            }
            Ok(String::new())
        }
        "single" => {
            if argument(0)? == "1" {
                Ok(String::new())
            } else {
                Err((5, "Single announcement mode only"))
            }
        }
        "outputs" => Ok(
            "outputid: 0\noutputname: PipeWire announcement\noutputenabled: 1\nplugin: pipewire\n"
                .into(),
        ),
        "tagtypes" => Ok("tagtype: Title\n".into()),
        "urlhandlers" => Ok("handler: http://\nhandler: https://\n".into()),
        _ => Err((5, "Command not supported by this announcement bridge")),
    }
}

async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Ok(None);
        }
        let count = buffer
            .iter()
            .position(|c| *c == b'\n')
            .map_or(buffer.len(), |i| i + 1);
        if line.len() + count > 16384 {
            return Err(io::Error::other("MPD command too long"));
        }
        line.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if line.last() == Some(&b'\n') {
            let value = String::from_utf8(line).map_err(|_| io::Error::other("Invalid UTF-8"))?;
            return Ok(Some(value.trim_end_matches(['\r', '\n']).into()));
        }
    }
}

fn parse(line: &str) -> Result<Vec<String>, ()> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for c in line.chars() {
        if c == '\0' {
            return Err(());
        }
        if escaped {
            word.push(c);
            escaped = false;
            started = true;
        } else if c == '\\' {
            escaped = true;
            started = true;
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            started = true;
        } else if c == ' ' || c == '\t' {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(c);
            started = true;
        }
    }
    if escaped || quote.is_some() {
        return Err(());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_mpd_quoted_arguments_without_a_shell() {
        assert_eq!(
            parse(r#"add "http://ha.local/a?x=1&y=two%20words""#).unwrap(),
            vec!["add", "http://ha.local/a?x=1&y=two%20words"]
        );
        assert_eq!(
            parse(r#"password "some\"quoted\\value""#).unwrap()[1],
            "some\"quoted\\value"
        );
        assert!(parse("add \"unterminated").is_err());
    }
}
