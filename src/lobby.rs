use crate::{
    error::Error,
    protocol::{
        ClientMessage, ColorProto, JoinError, LobbyEvent, LobbyState, PlayerState, QuatProto, Response, ServerMessage,
        SpawnInfo, Vec3Proto,
    },
    sr_log, Result,
};
use cgmath::Vector3;
use futures_util::{stream::SplitSink, SinkExt, StreamExt};
use nalgebra::{Isometry3, Matrix3, Rotation3, Translation3, UnitQuaternion};
use rapier3d_f64::{
    math::{Pose, Vec3, Vector},
    prelude::{
        ActiveEvents, BroadPhaseBvh, CCDSolver, ChannelEventCollector, ColliderBuilder, ColliderSet, CollisionEvent,
        ContactForceEvent, Group, ImpulseJointSet, IntegrationParameters, InteractionGroups, InteractionTestMode,
        IslandManager, MultibodyJointSet, NarrowPhase, PhysicsPipeline, RigidBodyBuilder, RigidBodyHandle,
        RigidBodySet,
    },
};
use std::{collections::HashMap, sync::mpsc::Receiver};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tungstenite::Message;

// ── Physics world ─────────────────────────────────────────────────────────────

/// All Rapier state in one place, separate from game logic.
struct PhysicsWorld {
    rigid_body_set: RigidBodySet,
    collider_set: ColliderSet,
    pipeline: PhysicsPipeline,
    island_manager: IslandManager,
    broad_phase: BroadPhaseBvh,
    narrow_phase: NarrowPhase,
    integration_parameters: IntegrationParameters,
    gravity: Vector,
    impulse_joint_set: ImpulseJointSet,
    multibody_joint_set: MultibodyJointSet,
    ccd_solver: CCDSolver,
    physics_hooks: (),
    #[allow(unused)]
    collision_events: ChannelEventCollector,
    collision_recv: Receiver<CollisionEvent>,
    #[allow(unused)]
    force_recv: Receiver<ContactForceEvent>,
}

impl PhysicsWorld {
    fn new() -> Self {
        let (collision_send, collision_recv) = std::sync::mpsc::channel();
        let (force_send, force_recv) = std::sync::mpsc::channel();
        Self {
            rigid_body_set: RigidBodySet::new(),
            collider_set: ColliderSet::new(),
            pipeline: PhysicsPipeline::new(),
            island_manager: IslandManager::new(),
            broad_phase: BroadPhaseBvh::new(),
            narrow_phase: NarrowPhase::new(),
            integration_parameters: IntegrationParameters::default(),
            gravity: Vector::new(0.0, -9.81, 0.0),
            impulse_joint_set: ImpulseJointSet::new(),
            multibody_joint_set: MultibodyJointSet::new(),
            ccd_solver: CCDSolver::new(),
            physics_hooks: (),
            collision_events: ChannelEventCollector::new(collision_send, force_send),
            collision_recv,
            force_recv,
        }
    }

    fn step(&mut self, delta: f64) {
        self.integration_parameters.dt = delta;
        self.pipeline.step(
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
            &self.collision_events,
        );
    }

    fn drain_collision_events(&mut self) {
        while let Ok(_event) = self.collision_recv.try_recv() {
            // Collision events intentionally not logged (high frequency).
        }
    }

    fn insert_body(&mut self, pos: Vec3Proto) -> RigidBodyHandle {
        let rb = RigidBodyBuilder::dynamic()
            .translation(Vec3::new(pos.x, pos.y, pos.z))
            // Linear damping is overridden each frame by the input handler.
            // Angular damping is intentionally low: the P-controller handles settling.
            .linear_damping(NORMAL_LINEAR_DAMPING)
            .angular_damping(0.5)
            .build();
        let handle = self.rigid_body_set.insert(rb);
        // Dimensions match Godot's player BoxShape3D(3.5, 2.8, 4.4) → half-extents (1.75, 1.4, 2.2).
        // density = target_mass / volume = 1000 / (3.5 × 2.8 × 4.4) ≈ 23.19 kg/m³
        // This gives mass ≈ 1000 kg AND a proper inertia tensor (needed for angular physics).
        let collider = ColliderBuilder::cuboid(1.75, 1.4, 2.2)
            .density(23.19)
            .friction(0.0) // traction is handled explicitly; contact friction would fight throttle
            .collision_groups(CAR_COLLISION)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        self.collider_set
            .insert_with_parent(collider, handle, &mut self.rigid_body_set);
        handle
    }

    fn remove_body(&mut self, handle: RigidBodyHandle) {
        self.rigid_body_set.remove(
            handle,
            &mut self.island_manager,
            &mut self.collider_set,
            &mut self.impulse_joint_set,
            &mut self.multibody_joint_set,
            true,
        );
    }

    fn get(&self, handle: RigidBodyHandle) -> Option<&rapier3d_f64::prelude::RigidBody> {
        self.rigid_body_set.get(handle)
    }

    fn get_mut(&mut self, handle: RigidBodyHandle) -> Option<&mut rapier3d_f64::prelude::RigidBody> {
        self.rigid_body_set.get_mut(handle)
    }
}

// ── Car physics constants ──────────────────────────────────────────────────────

/// Forward thrust force in Newtons.
const THROTTLE_FORCE: f64 = 10_000.0;

// Steering: P-controller drives yaw rate toward a target.
// The torque applied each step = STEER_P_GAIN × (target_yaw_rate − current_yaw_rate).
/// Target yaw rate (rad/s) at full steering without drift.  Tight: car can barely corner.
const MAX_TURN_RATE_GRIP: f64 = 1.2;
/// Target yaw rate (rad/s) at full steering while drifting.  Much higher: corners become possible.
const MAX_TURN_RATE_DRIFT: f64 = 3.2;
/// P-controller gain.  Higher = snappier steering response.
const STEER_P_GAIN: f64 = 25_000.0;

// Lateral grip: each physics step we apply an impulse that cancels a fraction
// of the velocity component perpendicular to the car's nose.
// 1.0 = perfect grip (velocity always aligned with nose), 0.0 = hovercraft.
/// Fraction of lateral velocity cancelled per step in normal driving (very grippy).
const LATERAL_GRIP: f64 = 0.90;
/// Fraction cancelled while drifting (almost none → car slides freely).
const LATERAL_DRIFT: f64 = 0.08;

// ── Collision groups ──────────────────────────────────────────────────────────
// GROUP_1 = walls/floor, GROUP_2 = cars.
// Cars collide with walls but NOT with each other.

/// Walls belong to GROUP_1 and interact with both groups.
const WALL_COLLISION: InteractionGroups = InteractionGroups::new(
    Group::GROUP_1,
    Group::GROUP_1.union(Group::GROUP_2),
    InteractionTestMode::And,
);
/// Cars belong to GROUP_2 and only interact with GROUP_1 (walls).
const CAR_COLLISION: InteractionGroups = InteractionGroups::new(
    Group::GROUP_2,
    Group::GROUP_1,
    InteractionTestMode::And,
);

/// Linear damping during normal driving.
const NORMAL_LINEAR_DAMPING: f64 = 0.3;
/// Reduced linear damping while drifting.
const DRIFT_LINEAR_DAMPING: f64 = 0.05;

// ── Race / lap constants ───────────────────────────────────────────────────────

/// Number of laps required to win the race.
const LAPS_TO_WIN: u8 = 3;
/// X coordinate of the finish line (matches spawn point x from level.tscn).
const LAP_FINISH_X: f64 = 145.0;
/// Half-width of the detection zone on the X axis (covers full track width).
const LAP_FINISH_X_HALF: f64 = 55.0;
/// Cars must cross z > LAP_POSITIVE_Z before a finish-line crossing counts.
/// Prevents the first step from the spawn (z=0 → z<0) being counted as a lap.
const LAP_POSITIVE_Z: f64 = 50.0;
/// X coordinate of the halfway checkpoint (opposite side of the track from the finish).
const CHECKPOINT_X: f64 = -145.0;
/// Half-width of the checkpoint detection zone on the X axis.
const CHECKPOINT_X_HALF: f64 = 55.0;
/// Seconds after the first finisher before the race is forcibly ended.
const FINISH_WAIT_SECS: f64 = 30.0;

// ── Racer ─────────────────────────────────────────────────────────────────────

/// Latest input snapshot received from the client.
/// Persists between frames so the car keeps moving if no message arrives.
#[derive(Default, Clone, Copy)]
struct PlayerInput {
    throttle: bool,
    /// Analog left-steer value in [0, 1].
    steer_left: f64,
    /// Analog right-steer value in [0, 1].
    steer_right: f64,
    star_drift: bool,
}

pub(crate) struct Racer {
    nickname: String,
    racing: bool,
    color: ColorProto,
    tx_stream: SplitSink<WebSocketStream<TcpStream>, Message>,
    rx_channel: crossbeam::channel::Receiver<PlayerEvent>,
    idx: u8,
    rigid_body: RigidBodyHandle,
    input: PlayerInput,
    laps: u8,
    prev_z: f64,
    crossed_positive_z: bool,
    crossed_halfway: bool,
    finished: bool,
}

impl Racer {
    fn new(
        nickname: String,
        idx: u8,
        color: ColorProto,
        tx_stream: SplitSink<WebSocketStream<TcpStream>, Message>,
        rx_channel: crossbeam::channel::Receiver<PlayerEvent>,
        handle: RigidBodyHandle,
    ) -> Self {
        Self {
            nickname,
            racing: false,
            color,
            tx_stream,
            rx_channel,
            idx,
            rigid_body: handle,
            input: PlayerInput::default(),
            laps: 0,
            prev_z: 0.0,
            crossed_positive_z: false,
            crossed_halfway: false,
            finished: false,
        }
    }
}

enum PlayerEvent {
    Close,
    Message(ClientMessage),
}

// ── Lobby state machine ───────────────────────────────────────────────────────

enum State {
    Intermission,
    Starting,
    Racing,
}

// ── Lobby ─────────────────────────────────────────────────────────────────────

pub struct Lobby {
    name: String,
    pub(crate) owner: String,
    pub(crate) start_time: String,
    pub(crate) min_players: u8,
    pub(crate) max_players: u8,
    pub(crate) racers: HashMap<String, Racer>,
    state: State,
    sync_timer: f64,
    intermission_timer: f64,
    sync_countdown_timer: f64,
    start_timer: f64,
    spawn_point: Vector3<f64>,
    spawn_y_rotation: f64,
    physics: PhysicsWorld,
    race_timer: f64,
    finish_timer: f64,
    finishers: Vec<String>,
}

impl Lobby {
    pub fn new(name: String, owner: String, start_time: String, min_players: u8, max_players: u8) -> Self {
        let mut lobby = Self {
            name,
            owner,
            start_time,
            min_players,
            max_players,
            racers: HashMap::new(),
            state: State::Intermission,
            sync_timer: 0.,
            intermission_timer: 0.,
            sync_countdown_timer: 0.,
            start_timer: 0.,
            spawn_point: Vector3::new(145.0, 3.0, 0.0),
            spawn_y_rotation: 0.0,
            physics: PhysicsWorld::new(),
            race_timer: 0.,
            finish_timer: 0.,
            finishers: Vec::new(),
        };
        lobby.create_track();
        lobby
    }

    /// Colliders aligned with `Star-Racer-Client/tracks/track/Track.tscn` (`Physical/*`).
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
            let m = Matrix3::new(xx, yx, zx, xy, yy, zy, xz, yz, zz);
            let rot = Rotation3::from_matrix_eps(&m, 1.0e-8, 100, Rotation3::identity());
            Isometry3::from_parts(
                Translation3::new(ox, oy, oz),
                UnitQuaternion::from_rotation_matrix(&rot),
            )
        }

        let floor = ColliderBuilder::cuboid(250.0, 0.5, 250.0)
            .position(godot_transform_to_isometry(1., 0., 0., 0., 1., 0., 0., 0., 1., 0., -0.5, 0.).into())
            .collision_groups(WALL_COLLISION)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        self.physics.collider_set.insert(floor);

        let wall_center = ColliderBuilder::cuboid(90.0, 2.5, 90.0)
            .position(godot_transform_to_isometry(1., 0., 0., 0., 1., 0., 0., 0., 1., 0., 2.5, 0.).into())
            .collision_groups(WALL_COLLISION)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        self.physics.collider_set.insert(wall_center);

        let hx = 225.0_f64;
        let hy = 2.5_f64;
        let hz = 25.0_f64;

        let wall_n = ColliderBuilder::cuboid(hx, hy, hz)
            .position(godot_transform_to_isometry(1., 0., 0., 0., 1., 0., 0., 0., 1., 25., 2.5, -225.).into())
            .collision_groups(WALL_COLLISION)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        self.physics.collider_set.insert(wall_n);

        let wall_e = ColliderBuilder::cuboid(hx, hy, hz)
            .position(
                godot_transform_to_isometry(
                    -4.371139e-08,
                    0.,
                    -1.,
                    0.,
                    1.,
                    0.,
                    1.,
                    0.,
                    -4.371139e-08,
                    -225.,
                    2.5,
                    -25.,
                )
                .into(),
            )
            .collision_groups(WALL_COLLISION)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        self.physics.collider_set.insert(wall_e);

        let wall_s = ColliderBuilder::cuboid(hx, hy, hz)
            .position(
                godot_transform_to_isometry(
                    -1.,
                    0.,
                    -8.742278e-08,
                    0.,
                    1.,
                    0.,
                    8.742278e-08,
                    0.,
                    -1.,
                    -25.,
                    2.5,
                    225.,
                )
                .into(),
            )
            .collision_groups(WALL_COLLISION)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        self.physics.collider_set.insert(wall_s);

        let wall_w = ColliderBuilder::cuboid(hx, hy, hz)
            .position(
                godot_transform_to_isometry(
                    -4.371139e-08,
                    0.,
                    1.,
                    0.,
                    1.,
                    0.,
                    -1.,
                    0.,
                    -4.371139e-08,
                    225.,
                    2.5,
                    25.,
                )
                .into(),
            )
            .collision_groups(WALL_COLLISION)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        self.physics.collider_set.insert(wall_w);
    }

    // ── Public API ──────────────────────────────────────────────────────────

    pub async fn join(
        &mut self,
        nickname: String,
        color: ColorProto,
        mut ws_stream: WebSocketStream<TcpStream>,
    ) -> Result<()> {
        if self.racers.contains_key(&nickname) {
            send_join_error(&mut ws_stream, JoinError::NicknameAlreadyUsed).await;
            return Err(Error::ClientNicknameAlreadyUsed);
        }
        if self.racers.len() >= self.max_players as usize {
            send_join_error(&mut ws_stream, JoinError::LobbyFull).await;
            return Err(Error::ClientLobbyFull);
        }

        let player_idx = self.first_free_idx();
        sr_log!(
            info,
            &nickname,
            "Joined lobby \"{}\" (idx={}, players={}/{})",
            self.name,
            player_idx,
            self.racers.len() + 1,
            self.max_players
        );

        let join_msg = ServerMessage::Response(Response::LobbyJoined {
            track_id: 0,
            race_ongoing: self.is_racing(),
            min_players: self.min_players,
            max_players: self.max_players,
            error: None,
        });
        ws_stream
            .send(Message::Text(serde_json::to_string(&join_msg).unwrap().into()))
            .await
            .unwrap();

        let (tx_stream, rx_stream) = ws_stream.split();
        // Do NOT send RaceStarted to late joiners — they spectate the current race
        // and join as a full participant on the next one.

        let (tx_channel, rx_channel) = crossbeam::channel::unbounded::<PlayerEvent>();
        launch_client_reader(nickname.clone(), tx_channel, rx_stream);

        let handle = self.physics.insert_body(Vec3Proto { x: 0., y: 0., z: 0. });
        let racer = Racer::new(nickname.clone(), player_idx, color, tx_stream, rx_channel, handle);
        self.racers.insert(nickname, racer);
        Ok(())
    }

    pub async fn update(&mut self, delta: f64) -> bool {
        self.process_player_events(delta);
        if self.racers.is_empty() {
            return false;
        }
        // Step physics first so the broadcast reflects the state *after* inputs are
        // integrated — clients receive the current frame, not the previous one.
        self.physics.step(delta);
        self.physics.drain_collision_events();
        self.check_lap_crossings();
        self.broadcast_player_states(delta).await;
        self.tick_state_machine(delta).await;
        true
    }

    pub fn player_count(&self) -> u8 {
        self.racers.len() as u8
    }

    pub fn is_racing(&self) -> bool {
        matches!(self.state, State::Racing)
    }

    // ── Private helpers ─────────────────────────────────────────────────────

    fn process_player_events(&mut self, delta: f64) {
        let mut to_remove = Vec::new();
        let is_racing = self.is_racing();
        let racer_count = self.racers.len();

        for (nickname, racer) in &mut self.racers {
            // Drain all queued messages, keeping only the latest input snapshot.
            // ClientMessage::State is a *state* frame, not an event: multiple messages
            // in the same tick just mean the channel buffered several snapshots.
            // Last-wins is correct: the most recent snapshot is the player's current intent.
            let mut should_remove = false;
            while let Ok(event) = racer.rx_channel.try_recv() {
                match event {
                    PlayerEvent::Close => {
                        sr_log!(
                            info,
                            &racer.nickname,
                            "Left lobby \"{}\" ({} remaining)",
                            self.name,
                            racer_count.saturating_sub(1)
                        );
                        should_remove = true;
                        break; // stop processing events for a racer being removed
                    }
                    PlayerEvent::Message(ClientMessage::State {
                        throttle,
                        steer_left,
                        steer_right,
                        star_drift,
                    }) => {
                        // Overwrite with the freshest snapshot; older ones are stale.
                        racer.input = PlayerInput {
                            throttle,
                            steer_left,
                            steer_right,
                            star_drift,
                        };
                    }
                    PlayerEvent::Message(_) => {}
                }
            }

            if should_remove {
                to_remove.push(nickname.clone());
                continue;
            }

            if !is_racing {
                continue;
            }

            let rb = self.physics.get_mut(racer.rigid_body).unwrap();

            // Apply the latest known input once per physics tick.
            // Drift only engages above a minimum speed — at low speed the huge yaw
            // rate target spins the car in place with no lateral velocity to resist it.
            let speed = rb.linvel().length();
            let is_drifting = racer.input.star_drift && speed > 3.0;
            rb.reset_forces(true);

            // Car axes — computed once, shared by throttle, steering and grip.
            // forward = local +Z in world; thrust is -forward (car moves in -Z like Godot convention).
            let forward = *rb.rotation() * Vec3::new(0., 0., 1.);
            // forward_speed: positive when car moves in its actual travel direction (-Z).
            // Must negate because forward points +Z but motion is in -Z.
            let forward_speed = -forward.dot(rb.linvel());

            // ── Throttle ──────────────────────────────────────────────────────
            if racer.input.throttle {
                rb.add_force(-forward * THROTTLE_FORCE, true);
            }

            // ── Steering: P-controller on yaw rate ────────────────────────────
            // We drive yaw rate toward a target rather than setting it directly.
            // This gives a torque build-up feel (arcade) while remaining snappy.
            // Without drift the target is low → the car can barely corner on its own.
            // With drift the target triples → tight corners become possible.
            let steer = racer.input.steer_right - racer.input.steer_left;
            // Mirror client: invert steer when going backward so controls match apparent travel.

            let effective_steer = if forward_speed >= -0.5 { steer } else { -steer };
            let max_turn = if is_drifting {
                MAX_TURN_RATE_DRIFT
            } else {
                MAX_TURN_RATE_GRIP
            };
            // Positive steer = right = negative Y in right-hand Y-up.
            let target_yaw = -effective_steer * max_turn;
            let yaw_error = target_yaw - rb.angvel().y;
            // apply_torque_impulse(τ × dt) = Δω = τ × dt / I — one-shot per step, no accumulation.
            // Equivalent to Godot's apply_torque() which auto-resets each physics frame.
            rb.apply_torque_impulse(Vec3::new(0., yaw_error * STEER_P_GAIN * delta, 0.), true);

            // ── Lateral grip ──────────────────────────────────────────────────
            // Cancel a fraction of the velocity that is perpendicular to the car's
            // nose each step.  High grip → velocity always aligns with facing direction
            // (the car tracks its nose, tight cornering is impossible without rotating
            // the car first).  Low grip (drift) → car slides freely sideways.
            let right = *rb.rotation() * Vec3::new(1., 0., 0.);
            let lateral_speed = right.dot(rb.linvel());
            let grip_cancel = if is_drifting { LATERAL_DRIFT } else { LATERAL_GRIP };
            let mass = rb.mass();
            rb.apply_impulse(-right * lateral_speed * mass * grip_cancel, true);

            // ── Linear damping ────────────────────────────────────────────────
            rb.set_linear_damping(if is_drifting {
                DRIFT_LINEAR_DAMPING
            } else {
                NORMAL_LINEAR_DAMPING
            });
        }

        for nickname in to_remove {
            if let Some(racer) = self.racers.remove(&nickname) {
                self.physics.remove_body(racer.rigid_body);
            }
        }
    }

    async fn broadcast_player_states(&mut self, delta: f64) {
        self.sync_timer += delta;
        if self.sync_timer <= 0.05 {
            return;
        }
        self.sync_timer = 0.;

        let states: Vec<PlayerState> = self
            .racers
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|nickname| {
                let racer = self.racers.get(&nickname).unwrap();
                let rb = self.physics.get(racer.rigid_body).unwrap();
                let t = rb.translation();
                let r = rb.rotation();
                PlayerState {
                    nickname,
                    racing: racer.racing,
                    laps: racer.laps,
                    position: Vec3Proto { x: t.x, y: t.y, z: t.z },
                    rotation: QuatProto {
                        x: r.x,
                        y: r.y,
                        z: r.z,
                        w: r.w,
                    },
                    color: racer.color,
                }
            })
            .collect();

        self.broadcast_message(ServerMessage::State(LobbyState::Players(states)), false)
            .await;
    }

    async fn tick_state_machine(&mut self, delta: f64) {
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
                    self.to_intermission().await;
                }
            }
        }
    }

    async fn intermission(&mut self, delta: f64) -> bool {
        if self.racers.len() < self.min_players as usize {
            self.intermission_timer += delta;
            if self.intermission_timer > 1. {
                let waiting = self.min_players - self.racers.len() as u8;
                sr_log!(
                    trace,
                    &self.name,
                    "Waiting for {} more player(s) ({}/{})",
                    waiting,
                    self.racers.len(),
                    self.min_players
                );
                self.broadcast_message(ServerMessage::State(LobbyState::WaitingForPlayers(waiting)), false)
                    .await;
                self.intermission_timer = 0.;
            }
            return true;
        }
        self.intermission_timer = 0.;
        false
    }

    async fn to_starting(&mut self) {
        sr_log!(
            info,
            &self.name,
            "-> Starting ({} players, countdown 5s)",
            self.racers.len()
        );
        for (i, racer) in self.racers.values_mut().enumerate() {
            let spawn_pos = Vec3Proto {
                x: self.spawn_point.x + 5. * i as f64,
                y: self.spawn_point.y,
                z: self.spawn_point.z,
            };
            if let Some(rb) = self.physics.rigid_body_set.get_mut(racer.rigid_body) {
                rb.set_position(
                    Pose::new(Vec3::new(spawn_pos.x, spawn_pos.y, spawn_pos.z), Vec3::new(0., 0., 0.)),
                    true,
                );
                rb.set_linvel(Vec3::new(0., 0., 0.), true);
                rb.set_angvel(Vec3::new(0., 0., 0.), true);
            }
            let spawn_info = SpawnInfo {
                y_rotation: self.spawn_y_rotation,
                position: spawn_pos,
            };
            let _ = racer
                .tx_stream
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::Event(LobbyEvent::RaceAboutToStart(spawn_info)))
                        .unwrap()
                        .into(),
                ))
                .await;
            racer.laps = 0;
            racer.prev_z = 0.0;
            racer.crossed_positive_z = false;
            racer.crossed_halfway = false;
            racer.finished = false;
            racer.racing = true;
        }
        self.race_timer = 0.;
        self.finish_timer = 0.;
        self.finishers.clear();
        self.state = State::Starting;
    }

    async fn starting(&mut self, delta: f64) -> bool {
        if self.start_timer < 5. {
            self.start_timer += delta;
            self.sync_countdown_timer += delta;
            if self.sync_countdown_timer > 1. {
                self.sync_countdown_timer = 0.;
                // Guard against negative time if a large delta pushed start_timer past 5.0
                // within this same frame.
                let time = (5. - self.start_timer).max(0.);
                for racer in self.racers.values_mut() {
                    let _ = racer
                        .tx_stream
                        .send(Message::Text(
                            serde_json::to_string(&ServerMessage::Event(LobbyEvent::Countdown { time }))
                                .unwrap()
                                .into(),
                        ))
                        .await;
                }
            }
            return true;
        }
        false
    }

    async fn to_race(&mut self) {
        self.sync_countdown_timer = 0.;
        self.start_timer = 0.;
        self.broadcast_message(ServerMessage::Event(LobbyEvent::RaceStarted(())), false)
            .await;
        sr_log!(info, &self.name, "-> Racing ({} players)", self.racers.len());
        for racer in self.racers.values_mut() {
            racer.racing = true;
        }
        self.state = State::Racing;
    }

    fn race(&mut self, delta: f64) -> bool {
        self.race_timer += delta;

        // Count down after the first finisher arrives.
        if self.finish_timer > 0.0 {
            self.finish_timer -= delta;
            if self.finish_timer <= 0.0 {
                return false; // grace period expired
            }
        }

        // End immediately once every racing player has finished.
        let racing: Vec<_> = self.racers.values().filter(|r| r.racing).collect();
        if !racing.is_empty() && racing.iter().all(|r| r.finished) {
            return false;
        }

        true
    }

    async fn to_intermission(&mut self) {
        // Rankings: finishers in crossing order, then DNF players sorted by name.
        let mut rankings = self.finishers.clone();
        let mut dnf: Vec<String> = self
            .racers
            .values()
            .filter(|r| r.racing && !r.finished)
            .map(|r| r.nickname.clone())
            .collect();
        dnf.sort();
        rankings.extend(dnf);

        let winner = rankings.first().cloned().unwrap_or_default();
        sr_log!(
            info,
            &self.name,
            "-> Intermission  winner={} ({}/{} finished in {:.1}s)",
            winner,
            self.finishers.len(),
            self.racers.len(),
            self.race_timer
        );

        self.broadcast_message(
            ServerMessage::Event(LobbyEvent::RaceFinished { winner, rankings }),
            false,
        )
        .await;

        for racer in self.racers.values_mut() {
            racer.racing = false;
        }
        self.intermission_timer = 0.;
        self.state = State::Intermission;
    }

    fn check_lap_crossings(&mut self) {
        // Collect position snapshots first to release the immutable borrow on self.physics
        // before we mutate racer state or self.finishers.
        let snapshots: Vec<(String, f64, f64)> = self
            .racers
            .iter()
            .filter_map(|(name, racer)| {
                self.physics
                    .get(racer.rigid_body)
                    .map(|rb| (name.clone(), rb.translation().x, rb.translation().z))
            })
            .collect();

        for (nickname, x, z) in snapshots {
            let racer = match self.racers.get_mut(&nickname) {
                Some(r) => r,
                None => continue,
            };

            if racer.finished || !racer.racing {
                racer.prev_z = z;
                continue;
            }

            // Gate: car must pass LAP_POSITIVE_Z before any finish-line crossing counts.
            // This prevents the first physics step from z=0 to z<0 (at spawn) being misread.
            if z > LAP_POSITIVE_Z {
                racer.crossed_positive_z = true;
            }

            // Halfway checkpoint (opposite side of the track from the finish line).
            // The car must cross this before a finish-line crossing counts as a lap,
            // preventing players from driving back and forth across the finish line.
            if (x - CHECKPOINT_X).abs() < CHECKPOINT_X_HALF {
                racer.crossed_halfway = true;
            }

            let prev_z = racer.prev_z;
            // Clockwise lap: car crosses finish line going from +Z side to −Z side.
            // X strip: |x − LAP_FINISH_X| < LAP_FINISH_X_HALF  (covers full track width).
            if racer.crossed_positive_z
                && racer.crossed_halfway
                && prev_z >= 0.0
                && z < 0.0
                && (x - LAP_FINISH_X).abs() < LAP_FINISH_X_HALF
            {
                racer.laps += 1;
                racer.crossed_positive_z = false; // must reach positive-Z again for the next lap
                racer.crossed_halfway = false; // must pass checkpoint again for the next lap

                sr_log!(info, &racer.nickname, "Lap {}/{}", racer.laps, LAPS_TO_WIN);

                if racer.laps >= LAPS_TO_WIN {
                    racer.finished = true;
                    let name = racer.nickname.clone();
                    self.finishers.push(name);
                    if self.finish_timer == 0.0 {
                        self.finish_timer = FINISH_WAIT_SECS;
                    }
                    continue; // skip prev_z update — racer is finished
                }
            }

            racer.prev_z = z;
        }
    }

    async fn broadcast_message(&mut self, message: ServerMessage, for_racing_players: bool) {
        let json = serde_json::to_string(&message).unwrap();
        for racer in self.racers.values_mut() {
            if for_racing_players && !racer.racing {
                continue;
            }
            let _ = racer.tx_stream.send(Message::Text(json.clone().into())).await;
        }
    }

    fn first_free_idx(&self) -> u8 {
        let mut idx = 0u8;
        let mut used: Vec<u8> = self.racers.values().map(|r| r.idx).collect();
        used.sort_unstable();
        for used_idx in used {
            if used_idx == idx {
                idx += 1;
            } else {
                break;
            }
        }
        idx
    }
}

// ── WebSocket reader task ─────────────────────────────────────────────────────

async fn send_join_error(ws_stream: &mut WebSocketStream<TcpStream>, error: JoinError) {
    let msg = ServerMessage::Response(Response::LobbyJoined {
        track_id: 0,
        race_ongoing: false,
        error: Some(error),
    });
    let _ = ws_stream
        .send(Message::Text(serde_json::to_string(&msg).unwrap().into()))
        .await;
}

fn launch_client_reader(
    nickname: String,
    tx_channel: crossbeam::channel::Sender<PlayerEvent>,
    mut rx_stream: futures_util::stream::SplitStream<WebSocketStream<TcpStream>>,
) {
    tokio::spawn(async move {
        loop {
            match rx_stream.next().await {
                Some(Ok(Message::Close(_))) => {
                    sr_log!(trace, nickname, "{} sent close", nickname);
                    let _ = tx_channel.send(PlayerEvent::Close);
                    break;
                }
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ClientMessage>(&text).map_err(Error::ClientInvalidJson) {
                        Ok(msg) => {
                            let _ = tx_channel.send(PlayerEvent::Message(msg));
                        }
                        Err(e) => {
                            sr_log!(trace, nickname, "Bad JSON from {}: {}", nickname, e);
                        }
                    }
                }
                Some(Ok(_)) => {
                    sr_log!(trace, nickname, "Unsupported message type from {}", nickname);
                }
                Some(Err(e)) => {
                    sr_log!(trace, nickname, "Read error from {}: {}", nickname, e);
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
