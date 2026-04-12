use crate::{
    error::Error,
    lobby::{send_join_error, spawn_ws_writer, Lobby, OutgoingMessage},
    protocol::{ClientMessage, ColorProto, JoinError, LobbyInfo, RequestMessage, Response, ServerMessage},
    sr_log, Result,
};
use chrono::Utc;
use futures_util::{stream::SplitStream, SinkExt, StreamExt};
use std::{collections::HashMap, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};
use tokio_tungstenite::{accept_async, WebSocketStream};
use tungstenite::Message;

enum LobbyCommand {
    FetchList {
        resp: oneshot::Sender<Vec<LobbyInfo>>,
    },
    Create {
        lobby_id: String,
        nickname: String,
        min_players: u8,
        max_players: u8,
        color: ColorProto,
        tx_out: mpsc::UnboundedSender<OutgoingMessage>,
        rx_stream: SplitStream<WebSocketStream<TcpStream>>,
    },
    Join {
        lobby_id: String,
        nickname: String,
        color: ColorProto,
        tx_out: mpsc::UnboundedSender<OutgoingMessage>,
        rx_stream: SplitStream<WebSocketStream<TcpStream>>,
    },
}

pub async fn run(port: u16) -> Result<()> {
    let endpoint = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&endpoint).await.map_err(Error::TcpError)?;
    run_with_listener(listener).await
}

pub async fn run_with_listener(listener: TcpListener) -> Result<()> {
    sr_log!(trace, "core", "Listening on {}", listener.local_addr().unwrap());

    let (cmd_tx, cmd_rx) = mpsc::channel::<LobbyCommand>(64);

    spawn_core_loop(cmd_rx);

    loop {
        let (stream, peer_addr) = listener.accept().await.map_err(Error::TcpError)?;
        if let Err(e) = stream.set_nodelay(true) {
            sr_log!(trace, peer_addr, "Failed to enable TCP_NODELAY: {}", e);
        }
        sr_log!(trace, peer_addr, ">TCP");
        tokio::spawn(handle_connection(stream, peer_addr.to_string(), cmd_tx.clone()));
    }
}

const FIXED_DT: f64 = 1.0 / 60.0;
const FIXED_DT_DURATION: Duration = Duration::from_nanos((FIXED_DT * 1_000_000_000.0) as u64);
const MAX_STEPS_PER_FRAME: u32 = 5;
const DEBUG_LOG_INTERVAL: Duration = Duration::from_secs(10);

fn spawn_core_loop(mut rx: mpsc::Receiver<LobbyCommand>) {
    tokio::spawn(async move {
        let mut lobbies: HashMap<String, Lobby> = HashMap::new();
        let mut next_tick = tokio::time::Instant::now() + FIXED_DT_DURATION;
        let mut accumulator = Duration::ZERO;
        let mut last_now = tokio::time::Instant::now();
        let mut last_debug = tokio::time::Instant::now();

        loop {
            tokio::select! {

                biased;

                _ = tokio::time::sleep_until(next_tick) => {
                    let now = tokio::time::Instant::now();
                    accumulator += now - last_now;
                    last_now = now;

                    let mut steps = 0;
                    while accumulator >= FIXED_DT_DURATION && steps < MAX_STEPS_PER_FRAME {
                        accumulator -= FIXED_DT_DURATION;
                        steps += 1;
                        if !lobbies.is_empty() {
                            run_tick(&mut lobbies, FIXED_DT);
                        }
                    }
                    if steps == MAX_STEPS_PER_FRAME {
                        accumulator = Duration::ZERO;
                    }
                    next_tick = now + FIXED_DT_DURATION - accumulator;

                    if last_debug.elapsed() >= DEBUG_LOG_INTERVAL {
                        last_debug = tokio::time::Instant::now();
                        log_debug_snapshot(&lobbies);
                    }
                }

                Some(cmd) = rx.recv() => {
                    handle_command(cmd, &mut lobbies);
                }

                else => return,
            }
        }
    });
}

fn run_tick(lobbies: &mut HashMap<String, Lobby>, delta: f64) {
    lobbies.retain(|name, lobby| {
        let alive = lobby.update(delta);
        if !alive {
            sr_log!(trace, "core", "Removing lobby {}", name);
        }
        alive
    });
}

fn handle_command(cmd: LobbyCommand, lobbies: &mut HashMap<String, Lobby>) {
    match cmd {
        LobbyCommand::FetchList { resp } => {
            let _ = resp.send(build_lobby_list(lobbies));
        }

        LobbyCommand::Create {
            lobby_id,
            nickname,
            min_players,
            max_players,
            color,
            tx_out,
            rx_stream,
        } => {
            if lobbies.contains_key(&lobby_id) {
                sr_log!(trace, "core", "Lobby {} already exists", lobby_id);
                send_join_error(&tx_out, JoinError::LobbyAlreadyExists);
                return;
            }
            let mut lobby = Lobby::new(
                lobby_id.clone(),
                nickname.clone(),
                Utc::now().format("%H:%M").to_string(),
                min_players,
                max_players,
            );
            sr_log!(info, "core", "{} created lobby {}", nickname, lobby_id);
            if let Err(e) = lobby.join(nickname, color, tx_out, rx_stream) {
                sr_log!(trace, "join", "Join failed for {}: {}", lobby_id, e);
                return;
            }
            lobbies.insert(lobby_id, lobby);
        }

        LobbyCommand::Join {
            lobby_id,
            nickname,
            color,
            tx_out,
            rx_stream,
        } => {
            let Some(lobby) = lobbies.get_mut(&lobby_id) else {
                sr_log!(trace, "join", "Lobby {} not found", lobby_id);
                send_join_error(&tx_out, JoinError::LobbyNotFound);
                return;
            };
            if let Err(e) = lobby.join(nickname, color, tx_out, rx_stream) {
                sr_log!(trace, "join", "Join failed for {}: {}", lobby_id, e);
            }
        }
    }
}

fn build_lobby_list(lobbies: &HashMap<String, Lobby>) -> Vec<LobbyInfo> {
    lobbies
        .iter()
        .map(|(name, l)| LobbyInfo {
            name: name.clone(),
            owner: l.owner.clone(),
            start_time: l.start_time.clone(),
            player_count: l.player_count(),
            min_players: l.min_players,
            max_players: l.max_players,
            racing: l.is_racing(),
        })
        .collect()
}

fn log_debug_snapshot(lobbies: &HashMap<String, Lobby>) {
    if lobbies.is_empty() {
        return;
    }
    sr_log!(trace, "core", "--- lobbies ({}) ---", lobbies.len());
    for (name, l) in lobbies {
        let state = if l.is_racing() { "racing" } else { "intermission" };
        sr_log!(
            trace,
            "core",
            "  [{}] owner={} players={}/{} state={}",
            name,
            l.owner,
            l.player_count(),
            l.max_players,
            state
        );
    }
}

async fn handle_connection(stream: TcpStream, peer_addr: String, cmd_tx: mpsc::Sender<LobbyCommand>) {
    let Ok(mut ws) = accept_async(stream).await else {
        sr_log!(trace, peer_addr, "WebSocket handshake failed for {}", peer_addr);
        return;
    };
    sr_log!(trace, peer_addr, "Connected");

    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                let msg = serde_json::from_str::<ClientMessage>(&text).map_err(Error::ClientInvalidJson);
                match msg {
                    Err(e) => {
                        sr_log!(trace, peer_addr, "Bad JSON from {}: {}", peer_addr, e);
                        return;
                    }
                    Ok(ClientMessage::State { .. }) => {
                        return;
                    }
                    Ok(ClientMessage::Request(request)) => {
                        match handle_request(request, &peer_addr, ws, &cmd_tx).await {
                            Some(returned_ws) => ws = returned_ws,
                            None => return,
                        }
                    }
                }
            }
            Some(Ok(Message::Close(_))) => {
                sr_log!(trace, peer_addr, "Closed");
                return;
            }
            Some(Ok(_)) => {
                sr_log!(trace, peer_addr, "Unsupported message type");
                return;
            }
            Some(Err(e)) => {
                sr_log!(trace, peer_addr, "Read error: {}", e);
                return;
            }
            None => {
                sr_log!(trace, peer_addr, "<TCP");
                return;
            }
        }
    }
}

async fn handle_request(
    request: RequestMessage,
    peer_addr: &str,
    mut ws: WebSocketStream<TcpStream>,
    cmd_tx: &mpsc::Sender<LobbyCommand>,
) -> Option<WebSocketStream<TcpStream>> {
    match request {
        RequestMessage::FetchLobbyList => {
            sr_log!(trace, peer_addr, "Fetching lobby list");
            let (resp_tx, resp_rx) = oneshot::channel();
            if cmd_tx.send(LobbyCommand::FetchList { resp: resp_tx }).await.is_err() {
                return None;
            }
            let list = resp_rx.await.unwrap_or_default();
            let response = ServerMessage::Response(Response::LobbyList(list));
            if let Err(e) = ws
                .send(Message::Text(serde_json::to_string(&response).unwrap().into()))
                .await
            {
                sr_log!(trace, peer_addr, "Send failed: {}", e);
            }
            Some(ws)
        }

        RequestMessage::CreateLobby {
            lobby_id,
            nickname,
            min_players,
            max_players,
            color,
        } => {
            sr_log!(trace, peer_addr, "{} creating lobby {}", nickname, lobby_id);
            let (tx_stream, rx_stream) = ws.split();
            let tx_out = spawn_ws_writer(tx_stream);
            let _ = cmd_tx
                .send(LobbyCommand::Create {
                    lobby_id,
                    nickname,
                    min_players,
                    max_players,
                    color,
                    tx_out,
                    rx_stream,
                })
                .await;
            None
        }

        RequestMessage::JoinLobby {
            lobby_id,
            nickname,
            color,
        } => {
            sr_log!(trace, peer_addr, "{} joining lobby {}", nickname, lobby_id);
            let (tx_stream, rx_stream) = ws.split();
            let tx_out = spawn_ws_writer(tx_stream);
            let _ = cmd_tx
                .send(LobbyCommand::Join {
                    lobby_id,
                    nickname,
                    color,
                    tx_out,
                    rx_stream,
                })
                .await;
            None
        }
    }
}
