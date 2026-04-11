use star_racer_server::protocol::{
    ClientMessage, ColorProto, JoinError, LobbyEvent, LobbyInfo, LobbyState, PlayerState, QuatProto, RequestMessage,
    Response, ServerMessage, SpawnInfo, Vec3Proto,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn red() -> ColorProto {
    ColorProto { x: 1.0, y: 0.0, z: 0.0 }
}

fn origin() -> Vec3Proto {
    Vec3Proto { x: 0.0, y: 0.0, z: 0.0 }
}

fn identity_quat() -> QuatProto {
    QuatProto {
        x: 0.0,
        y: 0.0,
        z: 0.0,
        w: 1.0,
    }
}

fn ser<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

fn de<T: serde::de::DeserializeOwned>(s: &str) -> T {
    serde_json::from_str(s).unwrap()
}

// ── Vec3Proto / QuatProto ─────────────────────────────────────────────────────

#[test]
fn vec3_serializes() {
    let v = Vec3Proto { x: 1.0, y: 2.0, z: 3.0 };
    assert_eq!(ser(&v), r#"{"x":1.0,"y":2.0,"z":3.0}"#);
}

#[test]
fn vec3_deserializes() {
    let v: Vec3Proto = de(r#"{"x":1.0,"y":2.0,"z":3.0}"#);
    assert_eq!(v.x, 1.0);
    assert_eq!(v.y, 2.0);
    assert_eq!(v.z, 3.0);
}

#[test]
fn quat_serializes() {
    let q = QuatProto {
        x: 0.0,
        y: 0.5,
        z: 0.0,
        w: 0.866,
    };
    assert_eq!(ser(&q), r#"{"x":0.0,"y":0.5,"z":0.0,"w":0.866}"#);
}

#[test]
fn quat_deserializes() {
    let q: QuatProto = de(r#"{"x":0.0,"y":0.0,"z":0.0,"w":1.0}"#);
    assert_eq!(q.w, 1.0);
}

// ── JoinError ─────────────────────────────────────────────────────────────────

#[test]
fn join_error_variants_serialize_as_strings() {
    assert_eq!(ser(&JoinError::NicknameAlreadyUsed), r#""NicknameAlreadyUsed""#);
    assert_eq!(ser(&JoinError::LobbyFull), r#""LobbyFull""#);
    assert_eq!(ser(&JoinError::LobbyAlreadyExists), r#""LobbyAlreadyExists""#);
    assert_eq!(ser(&JoinError::LobbyNotFound), r#""LobbyNotFound""#);
}

#[test]
fn join_error_roundtrip() {
    for err in [
        JoinError::NicknameAlreadyUsed,
        JoinError::LobbyFull,
        JoinError::LobbyAlreadyExists,
        JoinError::LobbyNotFound,
    ] {
        let json = ser(&err);
        let back: JoinError = de(&json);
        assert_eq!(ser(&back), json);
    }
}

// ── ClientMessage ─────────────────────────────────────────────────────────────

#[test]
fn client_fetch_lobby_list_serializes() {
    let msg = ClientMessage::Request(RequestMessage::FetchLobbyList);
    assert_eq!(ser(&msg), r#"{"Request":"FetchLobbyList"}"#);
}

#[test]
fn client_create_lobby_serializes() {
    let msg = ClientMessage::Request(RequestMessage::CreateLobby {
        lobby_id: "Arena".into(),
        nickname: "Alice".into(),
        min_players: 2,
        max_players: 4,
        color: red(),
    });
    let json = ser(&msg);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let inner = &v["Request"]["CreateLobby"];
    assert_eq!(inner["lobby_id"], "Arena");
    assert_eq!(inner["nickname"], "Alice");
    assert_eq!(inner["min_players"], 2);
    assert_eq!(inner["max_players"], 4);
    assert_eq!(inner["color"]["x"], 1.0);
    assert_eq!(inner["color"]["y"], 0.0);
}

#[test]
fn client_join_lobby_serializes() {
    let msg = ClientMessage::Request(RequestMessage::JoinLobby {
        lobby_id: "Arena".into(),
        nickname: "Bob".into(),
        color: red(),
    });
    let v: serde_json::Value = serde_json::from_str(&ser(&msg)).unwrap();
    let inner = &v["Request"]["JoinLobby"];
    assert_eq!(inner["lobby_id"], "Arena");
    assert_eq!(inner["nickname"], "Bob");
    assert_eq!(inner["color"]["z"], 0.0);
}

#[test]
fn client_state_serializes() {
    let msg = ClientMessage::State {
        throttle: true,
        steer_left: 0.5,
        steer_right: 0.0,
        star_drift: false,
    };
    let v: serde_json::Value = serde_json::from_str(&ser(&msg)).unwrap();
    assert_eq!(v["State"]["throttle"], true);
    assert_eq!(v["State"]["steer_left"], 0.5);
    assert_eq!(v["State"]["steer_right"], 0.0);
    assert_eq!(v["State"]["star_drift"], false);
}

#[test]
fn client_message_fetch_list_roundtrip() {
    let original = ClientMessage::Request(RequestMessage::FetchLobbyList);
    let json = ser(&original);
    let back: ClientMessage = de(&json);
    assert_eq!(ser(&back), json);
}

#[test]
fn client_message_state_roundtrip() {
    let original = ClientMessage::State {
        throttle: false,
        steer_left: 0.3,
        steer_right: 0.7,
        star_drift: true,
    };
    let json = ser(&original);
    let back: ClientMessage = de(&json);
    assert_eq!(ser(&back), json);
}

#[test]
fn invalid_client_message_json_fails() {
    let result = serde_json::from_str::<ClientMessage>(r#"{"NotAVariant": {}}"#);
    assert!(result.is_err());
}

#[test]
fn malformed_json_fails() {
    let result = serde_json::from_str::<ClientMessage>("not json at all");
    assert!(result.is_err());
}

// ── ServerMessage — Response ──────────────────────────────────────────────────

#[test]
fn server_lobby_list_empty_serializes() {
    let msg = ServerMessage::Response(Response::LobbyList(vec![]));
    assert_eq!(ser(&msg), r#"{"Response":{"LobbyList":[]}}"#);
}

#[test]
fn server_lobby_list_with_entry_serializes() {
    let msg = ServerMessage::Response(Response::LobbyList(vec![LobbyInfo {
        name: "Arena".into(),
        owner: "Alice".into(),
        start_time: "12:00".into(),
        player_count: 1,
        min_players: 2,
        max_players: 4,
        racing: false,
    }]));
    let v: serde_json::Value = serde_json::from_str(&ser(&msg)).unwrap();
    let entry = &v["Response"]["LobbyList"][0];
    assert_eq!(entry["name"], "Arena");
    assert_eq!(entry["owner"], "Alice");
    assert_eq!(entry["player_count"], 1);
    assert_eq!(entry["min_players"], 2);
    assert_eq!(entry["max_players"], 4);
    assert_eq!(entry["racing"], false);
}

#[test]
fn server_lobby_joined_success_serializes() {
    let msg = ServerMessage::Response(Response::LobbyJoined {
        track_id: 0,
        race_ongoing: false,
        min_players: 2,
        max_players: 4,
        error: None,
    });
    assert_eq!(
        ser(&msg),
        r#"{"Response":{"LobbyJoined":{"track_id":0,"race_ongoing":false,"min_players":2,"max_players":4,"error":null}}}"#
    );
}

#[test]
fn server_lobby_joined_error_lobby_full_serializes() {
    let msg = ServerMessage::Response(Response::LobbyJoined {
        track_id: 0,
        race_ongoing: false,
        min_players: 0,
        max_players: 0,
        error: Some(JoinError::LobbyFull),
    });
    let v: serde_json::Value = serde_json::from_str(&ser(&msg)).unwrap();
    assert_eq!(v["Response"]["LobbyJoined"]["error"], "LobbyFull");
}

#[test]
fn server_lobby_joined_all_error_variants_serialize() {
    for (err, expected) in [
        (JoinError::NicknameAlreadyUsed, "NicknameAlreadyUsed"),
        (JoinError::LobbyFull, "LobbyFull"),
        (JoinError::LobbyAlreadyExists, "LobbyAlreadyExists"),
        (JoinError::LobbyNotFound, "LobbyNotFound"),
    ] {
        let msg = ServerMessage::Response(Response::LobbyJoined {
            track_id: 0,
            race_ongoing: false,
            min_players: 0,
            max_players: 0,
            error: Some(err),
        });
        let v: serde_json::Value = serde_json::from_str(&ser(&msg)).unwrap();
        assert_eq!(v["Response"]["LobbyJoined"]["error"], expected);
    }
}

// ── ServerMessage — Events ────────────────────────────────────────────────────

#[test]
fn server_countdown_serializes() {
    let msg = ServerMessage::Event(LobbyEvent::Countdown { time: 3.0 });
    assert_eq!(ser(&msg), r#"{"Event":{"Countdown":{"time":3.0}}}"#);
}

#[test]
fn server_race_about_to_start_serializes() {
    let msg = ServerMessage::Event(LobbyEvent::RaceAboutToStart(SpawnInfo {
        y_rotation: 90.0,
        position: Vec3Proto { x: 1.0, y: 2.0, z: 3.0 },
    }));
    let v: serde_json::Value = serde_json::from_str(&ser(&msg)).unwrap();
    let info = &v["Event"]["RaceAboutToStart"];
    assert_eq!(info["y_rotation"], 90.0);
    assert_eq!(info["position"]["x"], 1.0);
    assert_eq!(info["position"]["y"], 2.0);
    assert_eq!(info["position"]["z"], 3.0);
}

#[test]
fn server_race_started_serializes() {
    let msg = ServerMessage::Event(LobbyEvent::RaceStarted(()));
    assert_eq!(ser(&msg), r#"{"Event":{"RaceStarted":null}}"#);
}

#[test]
fn server_race_finished_serializes() {
    let msg = ServerMessage::Event(LobbyEvent::RaceFinished {
        winner: "Alice".into(),
        rankings: vec![],
    });
    assert_eq!(ser(&msg), r#"{"Event":{"RaceFinished":{"winner":"Alice","rankings":[]}}}"#);
}

#[test]
fn server_player_joined_serializes() {
    let msg = ServerMessage::Event(LobbyEvent::PlayerJoined("Bob".into()));
    assert_eq!(ser(&msg), r#"{"Event":{"PlayerJoined":"Bob"}}"#);
}

#[test]
fn server_player_left_serializes() {
    let msg = ServerMessage::Event(LobbyEvent::PlayerLeft("Bob".into()));
    assert_eq!(ser(&msg), r#"{"Event":{"PlayerLeft":"Bob"}}"#);
}

// ── ServerMessage — State ─────────────────────────────────────────────────────

#[test]
fn server_waiting_for_players_serializes() {
    let msg = ServerMessage::State(LobbyState::WaitingForPlayers(3));
    assert_eq!(ser(&msg), r#"{"State":{"WaitingForPlayers":3}}"#);
}

#[test]
fn server_players_state_empty_serializes() {
    let msg = ServerMessage::State(LobbyState::Players(vec![]));
    assert_eq!(ser(&msg), r#"{"State":{"Players":[]}}"#);
}

#[test]
fn server_players_state_with_player_serializes() {
    let msg = ServerMessage::State(LobbyState::Players(vec![PlayerState {
        nickname: "Alice".into(),
        racing: true,
        position: Vec3Proto { x: 1.0, y: 2.0, z: 3.0 },
        rotation: identity_quat(),
        color: red(),
        laps: 1,
    }]));
    let v: serde_json::Value = serde_json::from_str(&ser(&msg)).unwrap();
    let p = &v["State"]["Players"][0];
    assert_eq!(p["nickname"], "Alice");
    assert_eq!(p["racing"], true);
    assert_eq!(p["position"]["x"], 1.0);
    assert_eq!(p["rotation"]["w"], 1.0);
    assert_eq!(p["color"]["x"], 1.0);
}

// ── Full roundtrip ────────────────────────────────────────────────────────────

#[test]
fn server_lobby_list_roundtrip() {
    let original = ServerMessage::Response(Response::LobbyList(vec![LobbyInfo {
        name: "X".into(),
        owner: "Y".into(),
        start_time: "00:00".into(),
        player_count: 0,
        min_players: 1,
        max_players: 2,
        racing: true,
    }]));
    let json = ser(&original);
    let back: ServerMessage = de(&json);
    assert_eq!(ser(&back), json);
}

#[test]
fn server_players_state_roundtrip() {
    let original = ServerMessage::State(LobbyState::Players(vec![PlayerState {
        nickname: "Bob".into(),
        racing: false,
        position: origin(),
        rotation: identity_quat(),
        color: ColorProto { x: 0.0, y: 1.0, z: 0.0 },
        laps: 0,
    }]));
    let json = ser(&original);
    let back: ServerMessage = de(&json);
    assert_eq!(ser(&back), json);
}

#[test]
fn server_race_about_to_start_roundtrip() {
    let original = ServerMessage::Event(LobbyEvent::RaceAboutToStart(SpawnInfo {
        y_rotation: 45.0,
        position: Vec3Proto {
            x: 10.0,
            y: 3.0,
            z: -5.0,
        },
    }));
    let json = ser(&original);
    let back: ServerMessage = de(&json);
    assert_eq!(ser(&back), json);
}
