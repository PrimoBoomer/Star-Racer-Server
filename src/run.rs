use crate::{
    error::Error,
    lobby::Lobby,
    protocol::{ClientMessage, ColorProto, JoinError, LobbyInfo, RequestMessage, Response, ServerMessage},
    sr_log, Result,
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc};
use tokio::{net::TcpListener, net::TcpStream, sync::Mutex};
use tokio_tungstenite::{accept_async, WebSocketStream};
use tungstenite::Message;

pub async fn run(port: u16) -> Result<()> {
    let endpoint = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&endpoint).await.map_err(Error::TcpError)?;
    run_with_listener(listener).await
}

pub async fn run_with_listener(listener: TcpListener) -> Result<()> {
    sr_log!(trace, "core", "Listening on {}", listener.local_addr().unwrap());

    let lobbies: Arc<Mutex<HashMap<String, Arc<Mutex<Lobby>>>>> = Arc::new(Mutex::new(HashMap::new()));

    spawn_game_loop(lobbies.clone());
    spawn_debug_logger(lobbies.clone());

    loop {
        let (stream, peer_addr) = listener.accept().await.map_err(Error::TcpError)?;
        sr_log!(trace, peer_addr, ">TCP");
        let lobbies_cln = lobbies.clone();
        tokio::spawn(handle_connection(stream, peer_addr.to_string(), lobbies_cln));
    }
}

// ── Background tasks ──────────────────────────────────────────────────────────

/// Target physics/game tick rate.
const FRAME_DURATION: std::time::Duration = std::time::Duration::from_millis(16); // ~60 Hz
/// Maximum delta passed to physics to avoid instability after stalls.
const MAX_DELTA_SECS: f64 = 0.1;

fn spawn_game_loop(lobbies: Arc<Mutex<HashMap<String, Arc<Mutex<Lobby>>>>>) {
    tokio::spawn(async move {
        let mut now = tokio::time::Instant::now();
        loop {
            if lobbies.lock().await.is_empty() {
                now = tokio::time::Instant::now();
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }

            let new_now = tokio::time::Instant::now();
            // Clamp delta: if the server was stalled, don't feed a huge spike to physics.
            let delta = (new_now - now).as_secs_f64().min(MAX_DELTA_SECS);
            now = new_now;

            let mut to_remove = Vec::new();
            let mut lock = lobbies.lock().await;
            for (name, lobby) in lock.iter() {
                if !lobby.lock().await.update(delta).await {
                    to_remove.push(name.clone());
                }
            }
            for name in to_remove {
                sr_log!(trace, "core", "Removing lobby {}", name);
                assert!(lock.remove(&name).is_some());
            }
            drop(lock);

            // Sleep for the remainder of the frame budget so we target ~60 Hz and
            // yield to other async tasks (connection handlers, readers) between ticks.
            let elapsed = now.elapsed();
            if let Some(remaining) = FRAME_DURATION.checked_sub(elapsed) {
                tokio::time::sleep(remaining).await;
            }
        }
    });
}

fn spawn_debug_logger(lobbies: Arc<Mutex<HashMap<String, Arc<Mutex<Lobby>>>>>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let lock = lobbies.lock().await;
            if lock.is_empty() {
                continue;
            }
            sr_log!(trace, "core", "--- lobbies ({}) ---", lock.len());
            for (name, lobby) in lock.iter() {
                let l = lobby.lock().await;
                let state = if l.is_racing() { "racing" } else { "intermission" };
                sr_log!(trace, "core", "  [{}] owner={} players={}/{} state={}",
                    name, l.owner, l.player_count(), l.max_players, state);
            }
        }
    });
}

// ── Connection handler ────────────────────────────────────────────────────────

async fn handle_connection(
    stream: TcpStream,
    peer_addr: String,
    lobbies: Arc<Mutex<HashMap<String, Arc<Mutex<Lobby>>>>>,
) {
    let Ok(mut ws) = accept_async(stream).await else {
        sr_log!(trace, peer_addr, "WebSocket handshake failed for {}", peer_addr);
        return;
    };
    sr_log!(info, peer_addr, "Connected");

    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                match serde_json::from_str::<ClientMessage>(&text).map_err(Error::ClientInvalidJson) {
                    Err(e) => {
                        sr_log!(trace, peer_addr, "Bad JSON from {}: {}", peer_addr, e);
                        return;
                    }
                    Ok(ClientMessage::State { .. }) => {
                        sr_log!(error, "run", "Got State message before joining a lobby");
                        return;
                    }
                    Ok(ClientMessage::Request(request)) => {
                        // handle_request consumes ws when joining a lobby (returns None)
                        match handle_request(request, &peer_addr, ws, &lobbies).await {
                            Some(returned_ws) => ws = returned_ws,
                            None => return,
                        }
                    }
                }
            }
            Some(Ok(Message::Close(_))) => {
                sr_log!(info, peer_addr, "Closed");
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

/// Returns `Some(ws)` to keep looping, `None` when the WebSocket was consumed
/// (lobby join/create hand off ownership to the lobby task).
async fn handle_request(
    request: RequestMessage,
    peer_addr: &str,
    mut ws: WebSocketStream<TcpStream>,
    lobbies: &Arc<Mutex<HashMap<String, Arc<Mutex<Lobby>>>>>,
) -> Option<WebSocketStream<TcpStream>> {
    match request {
        RequestMessage::FetchLobbyList => {
            sr_log!(info, peer_addr, "Fetching lobby list");
            let list = build_lobby_list(lobbies).await;
            let response = ServerMessage::Response(Response::LobbyList(list));
            if let Err(e) = ws.send(Message::Text(serde_json::to_string(&response).unwrap().into())).await {
                sr_log!(trace, peer_addr, "Send failed: {}", e);
            }
            Some(ws)
        }

        RequestMessage::CreateLobby { lobby_id, nickname, min_players, max_players, color } => {
            sr_log!(info, peer_addr, "{} creating lobby {}", nickname, lobby_id);
            let already_exists = lobbies.lock().await.contains_key(&lobby_id);
            if already_exists {
                sr_log!(trace, peer_addr, "Lobby {} already exists", lobby_id);
                send_join_error(&mut ws, JoinError::LobbyAlreadyExists).await;
                return None;
            }
            let lobby = Lobby::new(
                lobby_id.clone(),
                nickname.clone(),
                Utc::now().format("%H:%M").to_string(),
                min_players,
                max_players,
            );
            lobbies.lock().await.insert(lobby_id.clone(), Arc::new(Mutex::new(lobby)));
            join_lobby(lobby_id, nickname, color, ws, lobbies).await;
            None
        }

        RequestMessage::JoinLobby { lobby_id, nickname, color } => {
            sr_log!(info, peer_addr, "{} joining lobby {}", nickname, lobby_id);
            join_lobby(lobby_id, nickname, color, ws, lobbies).await;
            None
        }
    }
}

async fn build_lobby_list(
    lobbies: &Arc<Mutex<HashMap<String, Arc<Mutex<Lobby>>>>>,
) -> Vec<LobbyInfo> {
    let lock = lobbies.lock().await;
    let mut list = Vec::with_capacity(lock.len());
    for (name, lobby) in lock.iter() {
        let l = lobby.lock().await;
        list.push(LobbyInfo {
            name: name.clone(),
            owner: l.owner.clone(),
            start_time: l.start_time.clone(),
            player_count: l.player_count(),
            min_players: l.min_players,
            max_players: l.max_players,
            racing: l.is_racing(),
        });
    }
    list
}

async fn send_join_error(ws: &mut WebSocketStream<TcpStream>, error: JoinError) {
    let msg = ServerMessage::Response(Response::LobbyJoined {
        track_id: 0,
        race_ongoing: false,
        error: Some(error),
    });
    let _ = ws.send(Message::Text(serde_json::to_string(&msg).unwrap().into())).await;
}

async fn join_lobby(
    lobby_id: String,
    nickname: String,
    color: ColorProto,
    mut ws: WebSocketStream<TcpStream>,
    lobbies: &Arc<Mutex<HashMap<String, Arc<Mutex<Lobby>>>>>,
) {
    // Clone the Arc so we can drop the HashMap lock before awaiting on the inner Mutex.
    let lobby_arc = lobbies.lock().await.get(&lobby_id).cloned();
    let Some(lobby_arc) = lobby_arc else {
        sr_log!(trace, "join", "Lobby {} not found", lobby_id);
        send_join_error(&mut ws, JoinError::LobbyNotFound).await;
        return;
    };
    let mut guard = lobby_arc.lock().await;
    let result = guard.join(nickname, color, ws).await;
    drop(guard);
    if let Err(e) = result {
        sr_log!(trace, "join", "Join failed for {}: {}", lobby_id, e);
    }
}
