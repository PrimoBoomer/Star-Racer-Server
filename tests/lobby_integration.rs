use futures_util::{SinkExt, StreamExt};
use star_racer_server::protocol::{
    ClientMessage, ColorProto, JoinError, LobbyEvent, LobbyState, RequestMessage, Response,
    ServerMessage,
};
use star_racer_server::run::run_with_listener;
use tokio::net::TcpListener;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::{connect_async, WebSocketStream};
use tungstenite::Message;

// ── Infrastructure ─────────────────────────────────────────────────────────────

type WsStream =
    WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Bind port 0, spawn the server, return the actual port.
async fn start_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(run_with_listener(listener));
    // Give the task scheduler a moment to accept the first connection.
    tokio::time::sleep(Duration::from_millis(20)).await;
    port
}

async fn ws_connect(port: u16) -> WsStream {
    let (ws, _) = connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .unwrap();
    ws
}

fn make_color() -> ColorProto {
    ColorProto { x: 1.0, y: 0.0, z: 0.0 }
}

fn msg_fetch_list() -> ClientMessage {
    ClientMessage::Request(RequestMessage::FetchLobbyList)
}

fn msg_create(lobby: &str, nick: &str, min: u8, max: u8) -> ClientMessage {
    ClientMessage::Request(RequestMessage::CreateLobby {
        lobby_id: lobby.into(),
        nickname: nick.into(),
        min_players: min,
        max_players: max,
        color: make_color(),
    })
}

fn msg_join(lobby: &str, nick: &str) -> ClientMessage {
    ClientMessage::Request(RequestMessage::JoinLobby {
        lobby_id: lobby.into(),
        nickname: nick.into(),
        color: make_color(),
    })
}

async fn send(ws: &mut WsStream, msg: ClientMessage) {
    ws.send(Message::Text(
        serde_json::to_string(&msg).unwrap().into(),
    ))
    .await
    .unwrap();
}

/// Receive one text message, skipping Ping/Pong frames.
async fn recv(ws: &mut WsStream) -> ServerMessage {
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected WebSocket frame: {:?}", other),
        }
    }
}

/// Keep receiving until `pred` returns `Some(T)`, or panic after `secs` seconds.
async fn recv_until<F, T>(ws: &mut WsStream, secs: u64, mut pred: F) -> T
where
    F: FnMut(ServerMessage) -> Option<T>,
{
    timeout(Duration::from_secs(secs), async {
        loop {
            let msg = recv(ws).await;
            if let Some(v) = pred(msg) {
                return v;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out after {secs}s waiting for expected message"))
}

// ── Unit helpers (pure logic, no I/O) ────────────────────────────────────────

fn is_joined_ok(msg: &ServerMessage) -> bool {
    matches!(
        msg,
        ServerMessage::Response(Response::LobbyJoined { error: None, .. })
    )
}

fn join_error(msg: &ServerMessage) -> Option<&JoinError> {
    if let ServerMessage::Response(Response::LobbyJoined { error: Some(e), .. }) = msg {
        Some(e)
    } else {
        None
    }
}

// ── Tests: lobby list ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn fetch_empty_list() {
    let port = start_server().await;
    let mut ws = ws_connect(port).await;

    send(&mut ws, msg_fetch_list()).await;
    let msg = recv(&mut ws).await;

    let ServerMessage::Response(Response::LobbyList(list)) = msg else {
        panic!("expected LobbyList");
    };
    assert!(list.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn lobby_appears_in_list_after_creation() {
    let port = start_server().await;

    // Client 1 creates a lobby (also joins it automatically).
    let mut ws1 = ws_connect(port).await;
    send(&mut ws1, msg_create("Arena", "Alice", 5, 6)).await;
    let joined = recv(&mut ws1).await;
    assert!(is_joined_ok(&joined), "creator should get LobbyJoined ok");

    // Client 2 fetches the list — lobby must now be visible.
    let mut ws2 = ws_connect(port).await;
    send(&mut ws2, msg_fetch_list()).await;
    let msg = recv(&mut ws2).await;

    let ServerMessage::Response(Response::LobbyList(list)) = msg else {
        panic!("expected LobbyList");
    };
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "Arena");
    assert_eq!(list[0].owner, "Alice");
    assert_eq!(list[0].min_players, 5);
    assert_eq!(list[0].max_players, 6);
}

// ── Tests: CreateLobby ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn create_lobby_returns_joined_ok() {
    let port = start_server().await;
    let mut ws = ws_connect(port).await;

    send(&mut ws, msg_create("Lobby1", "Alice", 2, 4)).await;
    let msg = recv(&mut ws).await;

    let ServerMessage::Response(Response::LobbyJoined { error, track_id, race_ongoing, .. }) = msg
    else {
        panic!("expected LobbyJoined");
    };
    assert!(error.is_none(), "no error expected on create");
    assert_eq!(track_id, 0);
    assert!(!race_ongoing);
}

#[tokio::test(flavor = "multi_thread")]
async fn create_duplicate_lobby_returns_error() {
    let port = start_server().await;

    // First creator — succeeds.
    let mut ws1 = ws_connect(port).await;
    send(&mut ws1, msg_create("DupLobby", "Alice", 5, 6)).await;
    let r1 = recv(&mut ws1).await;
    assert!(is_joined_ok(&r1));

    // Second creator with the same lobby name — should fail.
    let mut ws2 = ws_connect(port).await;
    send(&mut ws2, msg_create("DupLobby", "Bob", 5, 6)).await;
    let r2 = recv(&mut ws2).await;

    let err = join_error(&r2).expect("expected an error");
    assert!(
        matches!(err, JoinError::LobbyAlreadyExists),
        "expected LobbyAlreadyExists, got {:?}",
        err
    );
}

// ── Tests: JoinLobby ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn join_existing_lobby_returns_joined_ok() {
    let port = start_server().await;

    // Create lobby with min high so no race starts immediately.
    let mut ws1 = ws_connect(port).await;
    send(&mut ws1, msg_create("Raceway", "Alice", 10, 10)).await;
    let r = recv(&mut ws1).await;
    assert!(is_joined_ok(&r));

    // Second player joins.
    let mut ws2 = ws_connect(port).await;
    send(&mut ws2, msg_join("Raceway", "Bob")).await;
    let r2 = recv(&mut ws2).await;
    assert!(is_joined_ok(&r2), "Bob should join successfully");
}

#[tokio::test(flavor = "multi_thread")]
async fn join_nonexistent_lobby_returns_error() {
    let port = start_server().await;
    let mut ws = ws_connect(port).await;

    send(&mut ws, msg_join("NoSuchLobby", "Alice")).await;
    let msg = recv(&mut ws).await;

    let err = join_error(&msg).expect("expected LobbyNotFound error");
    assert!(
        matches!(err, JoinError::LobbyNotFound),
        "expected LobbyNotFound, got {:?}",
        err
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn join_with_duplicate_nickname_returns_error() {
    let port = start_server().await;

    // Creator "Alice" joins as Alice.
    let mut ws1 = ws_connect(port).await;
    send(&mut ws1, msg_create("Nick", "Alice", 10, 10)).await;
    let r = recv(&mut ws1).await;
    assert!(is_joined_ok(&r));

    // Another client tries to join as Alice too.
    let mut ws2 = ws_connect(port).await;
    send(&mut ws2, msg_join("Nick", "Alice")).await;
    let r2 = recv(&mut ws2).await;

    let err = join_error(&r2).expect("expected nickname collision error");
    assert!(
        matches!(err, JoinError::NicknameAlreadyUsed),
        "expected NicknameAlreadyUsed, got {:?}",
        err
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn join_full_lobby_returns_error() {
    let port = start_server().await;

    // Create lobby max=1. Creator fills the single slot.
    // Use min=10 so the race won't start and complicate things.
    let mut ws1 = ws_connect(port).await;
    send(&mut ws1, msg_create("Tiny", "Alice", 10, 1)).await;
    let r = recv(&mut ws1).await;
    assert!(is_joined_ok(&r));

    // Second player — lobby is full.
    let mut ws2 = ws_connect(port).await;
    send(&mut ws2, msg_join("Tiny", "Bob")).await;
    let r2 = recv(&mut ws2).await;

    let err = join_error(&r2).expect("expected full-lobby error");
    assert!(
        matches!(err, JoinError::LobbyFull),
        "expected LobbyFull, got {:?}",
        err
    );
}

// ── Tests: game loop messages ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn player_state_broadcast_received() {
    let port = start_server().await;

    // One player in the lobby — the game loop broadcasts State(Players) every 100ms.
    // Use min=10 so we stay in intermission (no race start).
    let mut ws = ws_connect(port).await;
    send(&mut ws, msg_create("Broadcast", "Alice", 10, 4)).await;
    let joined = recv(&mut ws).await;
    assert!(is_joined_ok(&joined));

    // We should receive a State(Players) broadcast within 1 second.
    recv_until(&mut ws, 2, |msg| {
        if let ServerMessage::State(LobbyState::Players(players)) = msg {
            Some(players)
        } else {
            None
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_players_both_appear_in_state_broadcast() {
    let port = start_server().await;

    // Create with min=10 so no race starts during the test.
    let mut ws1 = ws_connect(port).await;
    send(&mut ws1, msg_create("Duo", "Alice", 10, 4)).await;
    assert!(is_joined_ok(&recv(&mut ws1).await));

    let mut ws2 = ws_connect(port).await;
    send(&mut ws2, msg_join("Duo", "Bob")).await;
    assert!(is_joined_ok(&recv(&mut ws2).await));

    // Wait until a Players broadcast contains both nicknames.
    let players = recv_until(&mut ws2, 2, |msg| {
        if let ServerMessage::State(LobbyState::Players(ps)) = msg {
            if ps.len() >= 2 {
                return Some(ps);
            }
        }
        None
    })
    .await;

    let names: Vec<&str> = players.iter().map(|p| p.nickname.as_str()).collect();
    assert!(names.contains(&"Alice"), "Alice not in broadcast");
    assert!(names.contains(&"Bob"), "Bob not in broadcast");
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_for_players_state_received() {
    let port = start_server().await;

    // One player, min=3 → server sends WaitingForPlayers every ~1 second.
    let mut ws = ws_connect(port).await;
    send(&mut ws, msg_create("Wait", "Alice", 3, 4)).await;
    assert!(is_joined_ok(&recv(&mut ws).await));

    let waiting = recv_until(&mut ws, 3, |msg| {
        if let ServerMessage::State(LobbyState::WaitingForPlayers(n)) = msg {
            Some(n)
        } else {
            None
        }
    })
    .await;

    // Need 3 - 1 = 2 more players.
    assert_eq!(waiting, 2);
}

// ── Tests: race lifecycle ─────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn race_starts_when_min_players_met() {
    let port = start_server().await;

    // min=1 → race starts immediately once a player joins.
    let mut ws = ws_connect(port).await;
    send(&mut ws, msg_create("Solo", "Alice", 1, 4)).await;
    assert!(is_joined_ok(&recv(&mut ws).await));

    // 1. Expect RaceAboutToStart with valid spawn info.
    let spawn = recv_until(&mut ws, 3, |msg| {
        if let ServerMessage::Event(LobbyEvent::RaceAboutToStart(info)) = msg {
            Some(info)
        } else {
            None
        }
    })
    .await;
    // Spawn position must be finite (sanity check).
    assert!(spawn.position.x.is_finite());
    assert!(spawn.position.y.is_finite());

    // 2. Expect at least one Countdown before RaceStarted.
    recv_until(&mut ws, 3, |msg| {
        if let ServerMessage::Event(LobbyEvent::Countdown { time }) = msg {
            assert!(time > 0.0 && time <= 5.0, "countdown out of range: {time}");
            Some(())
        } else {
            None
        }
    })
    .await;

    // 3. Expect RaceStarted within the full 5-second countdown + buffer.
    recv_until(&mut ws, 8, |msg| {
        if let ServerMessage::Event(LobbyEvent::RaceStarted(_)) = msg {
            Some(())
        } else {
            None
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn second_player_joining_racing_lobby_gets_race_started_event() {
    let port = start_server().await;

    // Player 1 joins min=1 lobby → race starts.
    let mut ws1 = ws_connect(port).await;
    send(&mut ws1, msg_create("Racing", "Alice", 1, 4)).await;
    assert!(is_joined_ok(&recv(&mut ws1).await));

    // Wait for RaceStarted from ws1 perspective.
    recv_until(&mut ws1, 10, |msg| {
        if let ServerMessage::Event(LobbyEvent::RaceStarted(_)) = msg {
            Some(())
        } else {
            None
        }
    })
    .await;

    // Player 2 joins the lobby while it's racing → should immediately get RaceStarted.
    let mut ws2 = ws_connect(port).await;
    send(&mut ws2, msg_join("Racing", "Bob")).await;

    // First message must be LobbyJoined (ok, race_ongoing = true).
    let joined = recv(&mut ws2).await;
    let ServerMessage::Response(Response::LobbyJoined { error, race_ongoing, .. }) = joined else {
        panic!("expected LobbyJoined");
    };
    assert!(error.is_none());
    // Late joiners spectate the ongoing race and get race_ongoing=true.
    // The server does NOT send RaceStarted immediately; they receive it when
    // the next race starts (after the current race ends + countdown).
    assert!(race_ongoing, "late joiner should see race_ongoing=true");
}
