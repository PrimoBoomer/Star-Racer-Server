use crate::{
    error::Error,
    protocol::{ClientMessage, LobbyEvent, LobbyState, Request, Response, ServerMessage},
    sr_log, Result,
};
use cgmath::{InnerSpace, Vector3};
use futures_util::{stream::SplitSink, SinkExt, StreamExt};
use nalgebra::{Isometry3, Matrix3, Rotation3, Translation3, UnitQuaternion};
use rapier3d_f64::{
    math::Vector,
    prelude::{BroadPhaseBvh, CCDSolver, ColliderBuilder, ColliderSet, ImpulseJointSet, IntegrationParameters, IslandManager, MultibodyJointSet, NarrowPhase, PhysicsPipeline, RigidBodySet},
};
use std::collections::HashMap;
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tungstenite::Message;

pub struct Racer {
    #[allow(dead_code)]
    nickname: String,
    #[allow(dead_code)]
    racing: bool,
    position: Vector3<f32>,
    direction: Vector3<f32>,
    color: (f32, f32, f32),
    tx_stream: SplitSink<WebSocketStream<TcpStream>, Message>,
    rx_channel: crossbeam::channel::Receiver<PlayerEvent>,
    idx: u8,
}

impl Racer {
    fn new(nickname: String, idx: u8, color: (f32, f32, f32), tx_stream: SplitSink<WebSocketStream<TcpStream>, Message>, rx_channel: crossbeam::channel::Receiver<PlayerEvent>) -> Self {
        Self {
            nickname: nickname,
            racing: false,
            position: Vector3::new(0., 0., 0.),
            direction: Vector3::new(0., 0., 1.),
            color: color,
            tx_stream,
            rx_channel,
            idx: idx,
        }
    }
}

enum State {
    Intermission,
    Starting,
    Racing,
}

pub struct Lobby {
    name: String,
    pub(crate) owner: String,
    pub(crate) start_time: String,
    pub(crate) min_players: u8,
    pub(crate) max_players: u8,
    pub(crate) racers: HashMap<String, Racer>,
    state: State,
    sync_timer: f32,
    sync_countdown_timer: f32,
    start_timer: f32,
    spawn_point: Vector3<f32>,
    spawn_y_rotation: f32,
    rigid_body_set: RigidBodySet,
    collider_set: ColliderSet,
    physics_pipeline: PhysicsPipeline,
    island_manager: IslandManager,
    broad_phase: BroadPhaseBvh,
    narrow_phase: NarrowPhase,
    integration_parameters: IntegrationParameters,
    gravity: Vector,
    impulse_joint_set: ImpulseJointSet,
    multibody_joint_set: MultibodyJointSet,
    ccd_solver: CCDSolver,
    physics_hooks: (),
    event_handler: (),
}

enum PlayerEvent {
    Close,
    Message(ClientMessage),
}

impl Lobby {
    pub fn new(name: String, owner: String, start_time: String, min_players: u8, max_players: u8) -> Self {
        let rigid_body_set = RigidBodySet::new();
        let collider_set = ColliderSet::new();
        let physics_pipeline = PhysicsPipeline::new();
        let island_manager = IslandManager::new();
        let broad_phase = BroadPhaseBvh::new();
        let narrow_phase = NarrowPhase::new();
        let integration_parameters = IntegrationParameters::default();
        let gravity = Vector::new(0.0, -9.81, 0.0);
        let impulse_joint_set = ImpulseJointSet::new();
        let multibody_joint_set = MultibodyJointSet::new();
        let ccd_solver = CCDSolver::new();
        let physics_hooks = ();
        let event_handler = ();

        let mut lobby = Self {
            name,
            owner,
            start_time,
            min_players,
            max_players,
            racers: HashMap::new(),
            state: State::Intermission,
            sync_timer: 0.,
            sync_countdown_timer: 0.,
            start_timer: 0.,
            spawn_point: Vector3::new(145.0, 3.0, 0.0),
            spawn_y_rotation: 0.0,
            rigid_body_set,
            collider_set,
            physics_pipeline,
            island_manager,
            broad_phase,
            narrow_phase,
            integration_parameters,
            gravity,
            impulse_joint_set,
            multibody_joint_set,
            ccd_solver,
            physics_hooks,
            event_handler,
        };

        lobby.create_track();

        lobby
    }

    /// Colliders aligned with `Star-Racer-Client/tracks/circuit_one/level.tscn` (`Physical/*`).
    fn create_track(&mut self) {
        fn godot_transform_to_isometry(
            xx: f64,
            xy: f64,
            xz: f64,
            yx: f64,
            yy: f64,
            yz: f64,
            zx: f64,
            zy: f64,
            zz: f64,
            ox: f64,
            oy: f64,
            oz: f64,
        ) -> Isometry3<f64> {
            let m = Matrix3::new(
                xx, yx, zx, //
                xy, yy, zy, //
                xz, yz, zz, //
            );
            let rot = Rotation3::from_matrix_eps(&m, 1.0e-8, 100, Rotation3::identity());
            Isometry3::from_parts(
                Translation3::new(ox, oy, oz),
                UnitQuaternion::from_rotation_matrix(&rot),
            )
        }

        // FloorCollider: flat HeightMapShape3D + PlaneMesh 500×500 → thin slab (top near y = 0).
        let floor = ColliderBuilder::cuboid(250.0, 0.5, 250.0)
            .position(
                Isometry3::from_parts(
                    Translation3::new(0.0, -0.5, 0.0),
                    UnitQuaternion::identity(),
                )
                .into(),
            )
            .build();
        self.collider_set.insert(floor);

        // LimitCuboid1 — BoxShape3D (180, 5, 180).
        let wall_outer = ColliderBuilder::cuboid(90.0, 2.5, 90.0)
            .position(
                godot_transform_to_isometry(
                    1.0, 0.0, 0.0, //
                    0.0, 1.0, 0.0, //
                    0.0, 0.0, 1.0, //
                    0.0, 2.5, 0.0,
                )
                .into(),
            )
            .build();
        self.collider_set.insert(wall_outer);

        // LimitCuboid2–5 — BoxShape3D (450, 5, 50).
        let hx = 225.0;
        let hy = 2.5;
        let hz = 25.0;

        let wall_n = ColliderBuilder::cuboid(hx, hy, hz)
            .position(
                godot_transform_to_isometry(
                    1.0, 0.0, 0.0, //
                    0.0, 1.0, 0.0, //
                    0.0, 0.0, 1.0, //
                    25.0, 2.5, -225.0,
                )
                .into(),
            )
            .build();
        self.collider_set.insert(wall_n);

        let wall_e = ColliderBuilder::cuboid(hx, hy, hz)
            .position(
                godot_transform_to_isometry(
                    -4.371139e-08,
                    0.0,
                    -1.0, //
                    0.0,
                    1.0,
                    0.0, //
                    1.0,
                    0.0,
                    -4.371139e-08, //
                    -225.0,
                    2.5,
                    -25.0,
                )
                .into(),
            )
            .build();
        self.collider_set.insert(wall_e);

        let wall_s = ColliderBuilder::cuboid(hx, hy, hz)
            .position(
                godot_transform_to_isometry(
                    -1.0,
                    0.0,
                    -8.742278e-08, //
                    0.0,
                    1.0,
                    0.0, //
                    8.742278e-08,
                    0.0,
                    -1.0, //
                    -25.0,
                    2.5,
                    225.0,
                )
                .into(),
            )
            .build();
        self.collider_set.insert(wall_s);

        let wall_w = ColliderBuilder::cuboid(hx, hy, hz)
            .position(
                godot_transform_to_isometry(
                    -4.371139e-08,
                    0.0,
                    1.0, //
                    0.0,
                    1.0,
                    0.0, //
                    -1.0,
                    0.0,
                    -4.371139e-08, //
                    225.0,
                    2.5,
                    25.0,
                )
                .into(),
            )
            .build();
        self.collider_set.insert(wall_w);
    }

    pub async fn join(&mut self, nickname: String, color: (f32, f32, f32), mut ws_stream: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Result<()> {
        if self.racers.contains_key(&nickname) {
            let msg = Message::Text(
                serde_json::to_string(&ServerMessage::Response(Response::LobbyJoined {
                    race_ongoing: false,
                    info: -1,
                    success: false,
                }))
                .unwrap()
                .into(),
            );
            let _ = ws_stream.send(msg).await;
            return Err(Error::ClientNicknameAlreadyUsed);
        }

        if self.racers.len() >= self.max_players as usize {
            let msg = Message::Text(
                serde_json::to_string(&ServerMessage::Response(Response::LobbyJoined {
                    race_ongoing: false,
                    info: -2,
                    success: false,
                }))
                .unwrap()
                .into(),
            );
            let _ = ws_stream.send(msg).await;
            return Err(Error::ClientLobbyFull);
        }

        // (self.spawn_point.x, self.spawn_point.y, self.spawn_point.z)
        let mut player_spawn_point = Vector3::new(self.spawn_point.x, self.spawn_point.y, self.spawn_point.z);
        let player_idx = self.first_free_idx();
        player_spawn_point.x += player_idx as f32 * 2.5;
        sr_log!(
            trace,
            nickname,
            "Player {} joined the lobby {} with color {:?} and spawn point {:?}",
            nickname,
            self.name,
            color,
            player_spawn_point
        );
        ws_stream
            .send(Message::Text(
                serde_json::to_string(&ServerMessage::Response(Response::LobbyJoined {
                    race_ongoing: self.is_racing(),
                    info: 0,
                    success: true,
                }))
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let (mut tx_stream, rx_stream) = ws_stream.split();
        let (tx_channel, rx_channel) = crossbeam::channel::unbounded::<PlayerEvent>();

        if let State::Racing = self.state {
            let _ = tx_stream.send(Message::Text(serde_json::to_string(&ServerMessage::Event(LobbyEvent::RaceStarted(()))).unwrap().into()));
        }
        let nickname_cln = nickname.clone();
        launch_client_reader(nickname_cln, tx_channel.clone(), rx_stream);

        let racer = Racer::new(nickname.clone(), player_idx, color, tx_stream, rx_channel);
        self.racers.insert(nickname, racer);
        Ok(())
    }

    pub async fn update(&mut self, delta: f32) -> bool {
        let mut to_remove = Vec::new();
        for (nickname, racer) in &mut self.racers {
            let mut next_position = racer.position;
            while let Ok(event) = racer.rx_channel.try_recv() {
                match event {
                    PlayerEvent::Close => {
                        sr_log!(trace, racer.nickname, "Racing {} left the lobby {}", nickname, self.name);
                        to_remove.push(nickname.clone());
                    }
                    PlayerEvent::Message(client_message) =>
                    {
                        #[allow(irrefutable_let_patterns)]
                        if let ClientMessage::Request(request) = client_message {
                            match request {
                                #[allow(unused_variables)]
                                Request::Move { vel_x, vel_y, vel_z } => {
                                    let velocity = cgmath::Vector3::new(vel_x, 0., vel_z);
                                    racer.direction = velocity.normalize();
                                    next_position.x += racer.direction.x * delta * 5.;
                                    next_position.y += racer.direction.y * delta * 5.;
                                    next_position.z += racer.direction.z * delta * 5.;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            racer.position = next_position;
        }

        for nickname in to_remove {
            self.racers.remove(&nickname);
        }

        if self.racers.len() == 0 {
            return false;
        }

        self.broadcast_all_players_state(delta).await;

        self.physics_pipeline.step(
            self.gravity,
            &self.integration_parameters,
            &mut self.island_manager,
            &mut self.broad_phase,
            &mut self.narrow_phase,
            &mut self.rigid_body_set,
            &mut self.collider_set,
            &mut self.impulse_joint_set,
            &mut self.multibody_joint_set,
            &mut self.ccd_solver,
            &self.physics_hooks,    
            &self.event_handler,
        );

        match self.state {
            State::Intermission => {
                if !self.intermission(delta).await {
                    self.to_starting().await;
                }
            }
            State::Starting => {
                if !self.starting(delta).await {
                    self.to_race().await;
                }
            }
            State::Racing => {
                if !self.race(delta) {
                    self.to_intermission()
                }
            }
        }
        true
    }

    async fn intermission(&mut self, delta: f32) -> bool {
        if self.racers.len() < self.min_players as usize {
            self.sync_timer += delta;
            if self.sync_timer > 1. {
                self.broadcast_message(ServerMessage::State(LobbyState::WaitingForPlayers(self.min_players - self.racers.len() as u8)), false)
                    .await;
                self.sync_timer = 0.;
            }
            return true;
        }
        false
    }

    fn race(&self, _delta: f32) -> bool {
        true
    }

    fn to_intermission(&mut self) {
        for racer in self.racers.values_mut() {
            racer.racing = false;
        }
        self.state = State::Intermission
    }

    async fn to_starting(&mut self) {
        let mut i = 0;
        for racer in self.racers.values_mut() {
            let _ = racer
                .tx_stream
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::to_string(&ServerMessage::Event(LobbyEvent::RaceAboutToStart((
                        self.spawn_y_rotation,
                        (self.spawn_point.x + 0.5 * i as f32, self.spawn_point.y, self.spawn_point.z + 0.5 * i as f32),
                    ))))
                    .unwrap()
                    .into(),
                ))
                .await;
            i += 1;
        }
        for racer in self.racers.values_mut() {
            racer.racing = true;
        }
        self.state = State::Starting;
    }

    async fn broadcast_message(&mut self, message: ServerMessage, for_racing_players: bool) {
        for racer in self.racers.values_mut() {
            if for_racing_players && !racer.racing {
                continue;
            }
            let _ = racer
                .tx_stream
                .send(tokio_tungstenite::tungstenite::Message::Text(serde_json::to_string(&message).unwrap().into()))
                .await;
        }
    }

    pub fn player_count(&self) -> u8 {
        self.racers.len() as u8
    }

    pub fn is_racing(&self) -> bool {
        matches!(self.state, State::Racing)
    }

    fn first_free_idx(&self) -> u8 {
        let mut idx = 0;
        let mut used_indices: Vec<u8> = self.racers.values().map(|r| r.idx).collect();
        used_indices.sort_unstable();
        for used_idx in used_indices {
            if used_idx == idx {
                idx += 1;
            } else {
                break;
            }
        }
        idx
    }

    async fn starting(&mut self, delta: f32) -> bool {
        self.sync_timer = 0.;

        if self.start_timer < 5. {
            self.sync_countdown_timer += delta;
            if self.sync_countdown_timer > 1. {
                for racer in self.racers.values_mut() {
                    racer
                        .tx_stream
                        .send(Message::Text(
                            serde_json::to_string(&ServerMessage::Event(LobbyEvent::Countdown { time: 5. - self.start_timer })).unwrap().into(),
                        ))
                        .await
                        .unwrap();
                    self.sync_countdown_timer = 0.;
                }
                self.start_timer += delta;
                return true;
            }
        }
        false
    }

    async fn to_race(&mut self) {
        self.sync_countdown_timer = 0.;
        self.start_timer = 0.;
        self.broadcast_message(ServerMessage::Event(LobbyEvent::RaceStarted(())), false).await;
        sr_log!(info, "run", "Lobby {} -> Race started", self.name);
        for racer in self.racers.values_mut() {
            racer.racing = true;
        }
        self.state = State::Racing;
    }

    async fn broadcast_all_players_state(&mut self, delta: f32) {
        self.sync_timer += delta;
        if self.sync_timer > 0.1 {
            let names: Vec<(String, bool, (f32, f32, f32), (f32, f32, f32), (f32, f32, f32))> = self
                .racers
                .keys()
                .cloned()
                .collect::<Vec<String>>()
                .into_iter()
                .map(|name| {
                    let racer = self.racers.get(&name).unwrap();
                    (
                        name,
                        racer.racing,
                        (racer.position.x, racer.position.y, racer.position.z),
                        (racer.direction.x, racer.direction.y, racer.direction.z),
                        racer.color,
                    )
                })
                .collect();
            self.broadcast_message(ServerMessage::State(LobbyState::Players(names)), false).await;
            self.sync_timer = 0.;
        }
    }
}

fn launch_client_reader(nickname: String, tx_channel: crossbeam::channel::Sender<PlayerEvent>, mut rx_stream: futures_util::stream::SplitStream<WebSocketStream<TcpStream>>) -> () {
    tokio::spawn(async move {
        loop {
            match rx_stream.next().await {
                Some(Ok(msg)) => match msg {
                    Message::Close(_) => {
                        sr_log!(trace, nickname, "{} sent close message", nickname);
                        let _ = tx_channel.send(PlayerEvent::Close);
                        break;
                    }
                    Message::Text(text) => {
                        let json_value: Result<ClientMessage> = serde_json::from_str(&text).map_err(|err| Error::ClientInvalidJson(err));
                        if json_value.is_err() {
                            sr_log!(trace, nickname, "Client reader: Failed to parse JSON message from {}: {}", nickname, json_value.err().unwrap());
                            continue;
                        }
                        tx_channel.send(PlayerEvent::Message(json_value.unwrap())).unwrap();
                    }
                    _ => {
                        sr_log!(trace, nickname, "Unsupported message type from {}: {:?}", nickname, msg);
                    }
                },
                Some(Err(e)) => {
                    sr_log!(trace, nickname, "Update: Error receiving message from {}: {}", nickname, e);
                    let _ = tx_channel.send(PlayerEvent::Close);
                    break;
                }
                None => {
                    sr_log!(trace, nickname, "{} disconnected", nickname);
                    let _ = tx_channel.send(PlayerEvent::Close);
                    break;
                }
            }
        }
    });
}
