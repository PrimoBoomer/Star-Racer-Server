use crate::{
    error::Error,
    lobby::Lobby,
    protocol::{ClientMessage, LobbyInfo, Request, Response, ServerMessage},
    sr_log, Result,
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc};
use tokio::{net::TcpListener, sync::Mutex};
use tokio_tungstenite::accept_async;
use tungstenite::Message;

pub async fn run(port: u16) -> Result<()> {
    let endpoint = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&endpoint).await.map_err(|err| Error::TcpError(err))?;
    sr_log!(trace, "core", "L {}", endpoint);

    let lobbies: Arc<Mutex<HashMap<String, Arc<Mutex<Lobby>>>>> = Arc::new(Mutex::new(HashMap::new()));

    let lobbies_cln = lobbies.clone();
    tokio::spawn(async move {
        let mut now = tokio::time::Instant::now();
        loop {
            if lobbies_cln.lock().await.len() == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }

            let new_now = tokio::time::Instant::now();
            let delta = new_now - now;
            now = new_now;

            let mut to_remove: Vec<String> = Vec::new();
            let mut lobbies_lock = lobbies_cln.lock().await;
            for lobby in lobbies_lock.iter() {
                if !lobby.1.lock().await.update(delta.as_secs_f32()).await {
                    to_remove.push(lobby.0.clone());
                }
            }
            for lobby_name in to_remove {
                sr_log!(trace, "core", "Removing lobby {}", lobby_name);
                assert!(lobbies_lock.remove(&lobby_name).is_some());
            }
        }
    });

    // Every minute we print all details of all lobbies for debugging
    let lobbies_cln = lobbies.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let lobbies_lock = lobbies_cln.lock().await;
            for lobby in lobbies_lock.iter() {
                let lobby_lock = lobby.1.lock().await;
                sr_log!(
                    trace,
                    "core",
                    "- {}: owner={}, players={}/{}, racing={}",
                    lobby.0,
                    lobby_lock.owner,
                    lobby_lock.player_count(),
                    lobby_lock.max_players,
                    lobby_lock.is_racing()
                );
            }
        }
    });
    loop {
        let (stream, peer_addr) = listener.accept().await.map_err(|err| Error::TcpError(err))?;
        sr_log!(trace, peer_addr, ">TCP");

        let lobbies_cln = lobbies.clone();
        tokio::spawn(async move {
            if let Ok(mut ws_stream) = accept_async(stream).await {
                sr_log!(info, peer_addr, "Connected");

                loop {
                    match ws_stream.next().await {
                        Some(Ok(msg)) => match msg {
                            Message::Text(text) => {
                                let json_value: Result<ClientMessage> = serde_json::from_str(&text).map_err(|err| Error::ClientInvalidJson(err));
                                if json_value.is_err() {
                                    sr_log!(trace, peer_addr, "Run: Failed to parse JSON message from {}: {}", peer_addr, json_value.err().unwrap());
                                    return;
                                }
                                match json_value.unwrap() {
                                    ClientMessage::Request(request) => {
                                        match request {
                                            Request::FetchLobbyList => {
                                                sr_log!(info, peer_addr, "Fetching lobby list");

                                                let mut lobby_list: Vec<LobbyInfo> = Vec::new();

                                                for lobby in lobbies_cln.lock().await.iter() {
                                                    let lobby_lock = lobby.1.lock().await;
                                                    lobby_list.push(LobbyInfo {
                                                        name: lobby.0.clone(),
                                                        owner: lobby_lock.owner.clone(),
                                                        start_time: lobby_lock.start_time.clone(),
                                                        player_count: lobby_lock.player_count(),
                                                        min_players: lobby_lock.min_players,
                                                        max_players: lobby_lock.max_players,
                                                        racing: lobby_lock.is_racing(),
                                                    });
                                                }

                                                let response = ServerMessage::Response(Response::LobbyList(lobby_list));
                                                let response_text = serde_json::to_string(&response).unwrap();
                                                if let Err(e) = ws_stream.send(tokio_tungstenite::tungstenite::Message::Text(response_text.into())).await {
                                                    sr_log!(trace, peer_addr, "Failed to send response to {}: {}", peer_addr, e);
                                                }
                                            }
                                            Request::CreateLobby {
                                                lobby_id,
                                                nickname,
                                                min_players,
                                                max_players,
                                                color,
                                            } => {
                                                sr_log!(info, peer_addr, "{} wants to create lobby {}", nickname, lobby_id);
                                                {
                                                    let mut lobbies_lock = lobbies_cln.lock().await;
                                                    if lobbies_lock.contains_key(&lobby_id) {
                                                        sr_log!(trace, peer_addr, "Lobby {} already exists", lobby_id);
                                                        return;
                                                    }

                                                    let lobby = Lobby::new(lobby_id.clone(), nickname.clone(), Utc::now().format("%H:%M").to_string(), min_players, max_players);

                                                    lobbies_lock.insert(lobby_id.clone(), Arc::new(Mutex::new(lobby)));
                                                }
                                                join_lobby(lobby_id.clone(), nickname, color, ws_stream, lobbies_cln.clone()).await;
                                                sr_log!(trace, peer_addr, "Lobby {} created by {}", lobby_id, peer_addr);
                                                return;
                                            }
                                            Request::JoinLobby { lobby_id, nickname, color } => {
                                                join_lobby(lobby_id.clone(), nickname, color, ws_stream, lobbies_cln.clone()).await;

                                                sr_log!(trace, peer_addr, "{} has joined lobby {}", peer_addr, lobby_id);
                                                return;
                                            }
                                            _ => {}
                                        };
                                    }
                                }
                            }
                            Message::Close(_) => {
                                sr_log!(info, peer_addr, "Connection closed");
                                return;
                            }
                            _ => {
                                sr_log!(trace, peer_addr, "Unsupported message type from {}: {:?}", peer_addr, msg);
                                return;
                            }
                        },
                        Some(Err(e)) => {
                            sr_log!(trace, peer_addr, "Error receiving message from {}: {}", peer_addr, e);
                        }
                        None => {
                            sr_log!(trace, peer_addr, "<TCP");
                            return;
                        }
                    }
                }
            } else {
                sr_log!(trace, peer_addr, "Failed to accept WebSocket connection from {}", peer_addr);
            }
        });
    }
}

async fn join_lobby(
    lobby_id: String,
    nickname: String,
    color: (f32, f32, f32),
    ws_stream: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    lobbies_cln: Arc<Mutex<HashMap<String, Arc<Mutex<Lobby>>>>>,
) -> () {
    let mut lobbies_lock = lobbies_cln.lock().await;
    let maybe_lobby = lobbies_lock.get_mut(&lobby_id);
    if maybe_lobby.is_none() {
        sr_log!(trace, "join", "Lobby {} does not exist", lobby_id);
        return;
    }
    let lobby = maybe_lobby.unwrap();

    let maybe_success = lobby.lock().await.join(nickname, color, ws_stream).await;

    if maybe_success.is_err() {
        sr_log!(trace, "join", "Failed to join lobby {}: {}", lobby_id, maybe_success.err().unwrap());
    }
}
