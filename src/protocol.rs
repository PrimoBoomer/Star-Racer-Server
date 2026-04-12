use serde::{Deserialize, Serialize};

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

pub type ColorProto = Vec3Proto;

#[derive(Serialize, Deserialize, Clone)]
pub struct PlayerState {
    pub nickname: String,
    pub racing: bool,
    pub laps: u8,
    pub position: Vec3Proto,
    pub rotation: QuatProto,
    pub color: ColorProto,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SpawnInfo {
    pub y_rotation: f64,
    pub position: Vec3Proto,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum JoinError {
    NicknameAlreadyUsed,
    LobbyFull,
    LobbyAlreadyExists,
    LobbyNotFound,
}

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

    LobbyJoined {
        track_id: u8,
        race_ongoing: bool,
        min_players: u8,
        max_players: u8,
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
