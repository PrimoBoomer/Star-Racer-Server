use serde::{Deserialize, Serialize};

// ── Primitive geometry types ─────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Vec3Proto {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct QuatProto {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub w: f64,
}

/// RGB colour (same memory layout as Vec3Proto, distinct type for clarity).
pub type ColorProto = Vec3Proto;

// ── Complex message types ────────────────────────────────────────────────────

/// Per-player snapshot sent every broadcast tick.
#[derive(Serialize, Deserialize, Clone)]
pub struct PlayerState {
    pub nickname: String,
    pub racing: bool,
    pub laps: u8,
    pub position: Vec3Proto,
    pub rotation: QuatProto,
    pub color: ColorProto,
}

/// Data included in the RaceAboutToStart event.
#[derive(Serialize, Deserialize, Clone)]
pub struct SpawnInfo {
    pub y_rotation: f64,
    pub position: Vec3Proto,
}

/// Typed error returned inside LobbyJoined on failure.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum JoinError {
    NicknameAlreadyUsed,
    LobbyFull,
    LobbyAlreadyExists,
    LobbyNotFound,
}

// ── Request / client → server ────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub enum RequestMessage {
    FetchLobbyList,
    CreateLobby {
        lobby_id: String,
        nickname: String,
        min_players: u8,
        max_players: u8,
        color: ColorProto,
    },
    JoinLobby {
        lobby_id: String,
        nickname: String,
        color: ColorProto,
    },
}

#[derive(Serialize, Deserialize)]
pub enum ClientMessage {
    Request(RequestMessage),
    State {
        throttle: bool,
        steer_left: f64,
        steer_right: f64,
        star_drift: bool,
    },
}

// ── Response / server → client ───────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct LobbyInfo {
    pub name: String,
    pub owner: String,
    pub start_time: String,
    pub player_count: u8,
    pub min_players: u8,
    pub max_players: u8,
    pub racing: bool,
}

#[derive(Serialize, Deserialize)]
pub enum Response {
    LobbyList(Vec<LobbyInfo>),
    /// `error` is None on success. `track_id` and `race_ongoing` are valid only
    /// when `error` is None.
    LobbyJoined {
        track_id: u8,
        race_ongoing: bool,
        error: Option<JoinError>,
    },
}

#[derive(Serialize, Deserialize, Clone)]
pub enum LobbyState {
    WaitingForPlayers(u8),
    Players(Vec<PlayerState>),
}

#[derive(Serialize, Deserialize)]
pub enum LobbyEvent {
    Countdown { time: f64 },
    RaceAboutToStart(SpawnInfo),
    RaceStarted(()),
    RaceFinished { winner: String, rankings: Vec<String> },
    PlayerJoined(String),
    PlayerLeft(String),
}

#[derive(Serialize, Deserialize)]
pub enum ServerMessage {
    Event(LobbyEvent),
    State(LobbyState),
    Response(Response),
}
