use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize)]
pub enum Request {
    FetchLobbyList,
    CreateLobby {
        lobby_id: String,
        nickname: String,
        min_players: u8,
        max_players: u8,
        color: (f32, f32, f32),
    },
    JoinLobby {
        lobby_id: String,
        nickname: String,
        color: (f32, f32, f32),
    },
    Move {
        vel_x: f32,
        vel_y: f32,
        vel_z: f32,
    },
}

#[derive(Serialize, Deserialize)]
pub enum ClientMessage {
    Request(Request),
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
    LobbyJoined { success: bool, info: i8, race_ongoing: bool },
}

#[derive(Serialize, Deserialize, Clone)]
pub enum LobbyState {
    WaitingForPlayers(u8),
    Players(Vec<(String, bool, (f32, f32, f32), (f32, f32, f32), (f32, f32, f32))>),
}

#[derive(Serialize, Deserialize)]
pub enum LobbyEvent {
    Countdown { time: f32 },
    RaceAboutToStart((f32, (f32, f32, f32))),
    RaceStarted(()),
    RaceFinished { winner: String },
    PlayerJoined(String),
    PlayerLeft(String),
}

#[derive(Serialize, Deserialize)]
pub enum ServerMessage {
    Event(LobbyEvent),
    State(LobbyState),
    Response(Response),
}
