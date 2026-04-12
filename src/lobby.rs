use crate::{
    error::Error,
    protocol::{
        ClientMessage, ColorProto, JoinError, LobbyEvent, LobbyState, PlayerState, QuatProto, Response, ServerMessage,
        SpawnInfo, Vec3Proto,
    },
    sr_log, Result,
};
use cgmath::Vector3;
use futures_util::{stream::SplitStream, SinkExt, StreamExt};
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

#[derive(Clone)]
pub(crate) enum OutgoingMessage {
    Reliable(Message),
    State(Message),
}

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
        while let Ok(_event) = self.collision_recv.try_recv() {}
    }

    fn insert_body(&mut self, pos: Vec3Proto) -> RigidBodyHandle {
        let rb = RigidBodyBuilder::dynamic()
            .translation(Vec3::new(pos.x, pos.y, pos.z))
            .enabled_rotations(false, true, false)
            .linear_damping(NORMAL_LINEAR_DAMPING)
            .angular_damping(0.5)
            .build();
        let handle = self.rigid_body_set.insert(rb);
        let collider = ColliderBuilder::cuboid(1.75, 1.4, 2.2)
            .density(23.19)
            .friction(0.0)
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

const THROTTLE_FORCE: f64 = 10_000.0;
const MAX_TURN_RATE_GRIP: f64 = 1.2;
const MAX_TURN_RATE_DRIFT: f64 = 3.2;
const STEER_P_GAIN: f64 = 25_000.0;
const LATERAL_GRIP: f64 = 0.90;
const LATERAL_DRIFT: f64 = 0.08;
const MOTION_DIRECTION_EPSILON: f64 = 0.25;
const TURN_DRAG_PER_STEER: f64 = 0.16;
const WALL_COLLISION: InteractionGroups = InteractionGroups::new(
    Group::GROUP_1,
    Group::GROUP_1.union(Group::GROUP_2),
    InteractionTestMode::And,
);
const CAR_COLLISION: InteractionGroups =
    InteractionGroups::new(Group::GROUP_2, Group::GROUP_1, InteractionTestMode::And);

const NORMAL_LINEAR_DAMPING: f64 = 0.3;
const DRIFT_LINEAR_DAMPING: f64 = 0.18;
const DRIFT_MIN_SPEED: f64 = 3.0;

const LAPS_TO_WIN: u8 = 3;

const LAP_FINISH_X: f64 = 145.0;

const LAP_FINISH_X_HALF: f64 = 55.0;

const LAP_POSITIVE_Z: f64 = 50.0;

const CHECKPOINT_X: f64 = -145.0;

const CHECKPOINT_X_HALF: f64 = 55.0;

const FINISH_WAIT_SECS: f64 = 30.0;
const STATE_SYNC_INTERVAL: f64 = 0.05;

#[derive(Default, Clone, Copy)]
struct PlayerInput {
    throttle: bool,

    steer_left: f64,

    steer_right: f64,
    star_drift: bool,
}

fn update_reverse_mode(was_reversing: bool, forward_speed: f64) -> bool {
    if forward_speed <= -MOTION_DIRECTION_EPSILON {
        true
    } else if forward_speed >= MOTION_DIRECTION_EPSILON {
        false
    } else {
        was_reversing
    }
}

fn effective_steer_input(steer: f64, is_reversing: bool) -> f64 {
    if is_reversing {
        -steer
    } else {
        steer
    }
}

fn stabilize_quaternion(prev: Option<QuatProto>, current: QuatProto) -> QuatProto {
    let Some(prev) = prev else {
        return current;
    };

    let dot = prev.x * current.x + prev.y * current.y + prev.z * current.z + prev.w * current.w;
    if dot < 0.0 {
        QuatProto {
            x: -current.x,
            y: -current.y,
            z: -current.z,
            w: -current.w,
        }
    } else {
        current
    }
}

pub(crate) struct Racer {
    nickname: String,
    racing: bool,
    color: ColorProto,
    tx: tokio::sync::mpsc::UnboundedSender<OutgoingMessage>,
    rx_channel: crossbeam::channel::Receiver<PlayerEvent>,
    idx: u8,
    rigid_body: RigidBodyHandle,
    input: PlayerInput,
    laps: u8,
    prev_z: f64,
    crossed_positive_z: bool,
    crossed_halfway: bool,
    finished: bool,
    reversing: bool,
    last_sent_rotation: Option<QuatProto>,
}

impl Racer {
    fn new(
        nickname: String,
        idx: u8,
        color: ColorProto,
        tx: tokio::sync::mpsc::UnboundedSender<OutgoingMessage>,
        rx_channel: crossbeam::channel::Receiver<PlayerEvent>,
        handle: RigidBodyHandle,
    ) -> Self {
        Self {
            nickname,
            racing: false,
            color,
            tx,
            rx_channel,
            idx,
            rigid_body: handle,
            input: PlayerInput::default(),
            laps: 0,
            prev_z: 0.0,
            crossed_positive_z: false,
            crossed_halfway: false,
            finished: false,
            reversing: false,
            last_sent_rotation: None,
        }
    }
}

enum PlayerEvent {
    Close,
    Message(ClientMessage),
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

    fn create_track(&mut self) {
        fn godot_transform_to_isometry(basis: [[f64; 3]; 3], origin: [f64; 3]) -> Isometry3<f64> {
            let m = Matrix3::new(
                basis[0][0],
                basis[1][0],
                basis[2][0],
                basis[0][1],
                basis[1][1],
                basis[2][1],
                basis[0][2],
                basis[1][2],
                basis[2][2],
            );
            let rot = Rotation3::from_matrix_eps(&m, 1.0e-8, 100, Rotation3::identity());
            Isometry3::from_parts(
                Translation3::new(origin[0], origin[1], origin[2]),
                UnitQuaternion::from_rotation_matrix(&rot),
            )
        }

        let floor = ColliderBuilder::cuboid(250.0, 0.5, 250.0)
            .position(godot_transform_to_isometry([[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]], [0., -0.5, 0.]).into())
            .collision_groups(WALL_COLLISION)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        self.physics.collider_set.insert(floor);

        let wall_center = ColliderBuilder::cuboid(90.0, 2.5, 90.0)
            .position(godot_transform_to_isometry([[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]], [0., 2.5, 0.]).into())
            .collision_groups(WALL_COLLISION)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        self.physics.collider_set.insert(wall_center);

        let hx = 225.0_f64;
        let hy = 2.5_f64;
        let hz = 25.0_f64;

        let wall_n = ColliderBuilder::cuboid(hx, hy, hz)
            .position(godot_transform_to_isometry([[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]], [25., 2.5, -225.]).into())
            .collision_groups(WALL_COLLISION)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        self.physics.collider_set.insert(wall_n);

        let wall_e = ColliderBuilder::cuboid(hx, hy, hz)
            .position(
                godot_transform_to_isometry(
                    [[-4.371139e-08, 0., -1.], [0., 1., 0.], [1., 0., -4.371139e-08]],
                    [-225., 2.5, -25.],
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
                    [[-1., 0., -8.742278e-08], [0., 1., 0.], [8.742278e-08, 0., -1.]],
                    [-25., 2.5, 225.],
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
                    [[-4.371139e-08, 0., 1.], [0., 1., 0.], [-1., 0., -4.371139e-08]],
                    [225., 2.5, 25.],
                )
                .into(),
            )
            .collision_groups(WALL_COLLISION)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        self.physics.collider_set.insert(wall_w);
    }

    pub(crate) fn join(
        &mut self,
        nickname: String,
        color: ColorProto,
        tx_out: tokio::sync::mpsc::UnboundedSender<OutgoingMessage>,
        rx_stream: SplitStream<WebSocketStream<TcpStream>>,
    ) -> Result<()> {
        if self.racers.contains_key(&nickname) {
            send_join_error(&tx_out, JoinError::NicknameAlreadyUsed);
            return Err(Error::ClientNicknameAlreadyUsed);
        }
        if self.racers.len() >= self.max_players as usize {
            send_join_error(&tx_out, JoinError::LobbyFull);
            return Err(Error::ClientLobbyFull);
        }

        let player_idx = self.first_free_idx();
        sr_log!(
            trace,
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
        let _ = tx_out.send(outgoing_server_message(&join_msg));

        let (tx_channel, rx_channel) = crossbeam::channel::unbounded::<PlayerEvent>();
        launch_client_reader(nickname.clone(), tx_channel, rx_stream);

        let handle = self.physics.insert_body(Vec3Proto { x: 0., y: 0., z: 0. });
        let racer = Racer::new(nickname.clone(), player_idx, color, tx_out, rx_channel, handle);
        self.racers.insert(nickname, racer);
        Ok(())
    }

    pub fn update(&mut self, delta: f64) -> bool {
        self.process_player_events(delta);
        if self.racers.is_empty() {
            return false;
        }

        self.physics.step(delta);
        self.physics.drain_collision_events();
        self.check_lap_crossings();
        self.broadcast_player_states(delta);
        self.tick_state_machine(delta);
        true
    }

    pub fn player_count(&self) -> u8 {
        self.racers.len() as u8
    }

    pub fn is_racing(&self) -> bool {
        matches!(self.state, State::Racing)
    }

    fn process_player_events(&mut self, delta: f64) {
        let mut to_remove = Vec::new();
        let is_racing = self.is_racing();
        let racer_count = self.racers.len();

        for (nickname, racer) in &mut self.racers {
            let mut should_remove = false;
            while let Ok(event) = racer.rx_channel.try_recv() {
                match event {
                    PlayerEvent::Close => {
                        sr_log!(
                            trace,
                            &racer.nickname,
                            "Left lobby \"{}\" ({} remaining)",
                            self.name,
                            racer_count.saturating_sub(1)
                        );
                        should_remove = true;
                        break;
                    }
                    PlayerEvent::Message(ClientMessage::State {
                        throttle,
                        steer_left,
                        steer_right,
                        star_drift,
                    }) => {
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

            let speed = rb.linvel().length();
            let is_drifting = racer.input.star_drift && speed > DRIFT_MIN_SPEED;
            rb.reset_forces(true);

            let forward = *rb.rotation() * Vec3::new(0., 0., 1.);

            let forward_speed = -forward.dot(rb.linvel());
            racer.reversing = update_reverse_mode(racer.reversing, forward_speed);

            if racer.input.throttle {
                rb.add_force(-forward * THROTTLE_FORCE, true);
            }

            let steer = racer.input.steer_right - racer.input.steer_left;

            let effective_steer = effective_steer_input(steer, racer.reversing);
            let max_turn = if is_drifting {
                MAX_TURN_RATE_DRIFT
            } else {
                MAX_TURN_RATE_GRIP
            };

            let target_yaw = -effective_steer * max_turn;
            let yaw_error = target_yaw - rb.angvel().y;

            rb.apply_torque_impulse(Vec3::new(0., yaw_error * STEER_P_GAIN * delta, 0.), true);

            let right = *rb.rotation() * Vec3::new(1., 0., 0.);
            let lateral_speed = right.dot(rb.linvel());
            let grip_cancel = if is_drifting { LATERAL_DRIFT } else { LATERAL_GRIP };
            let mass = rb.mass();
            rb.apply_impulse(-right * lateral_speed * mass * grip_cancel, true);

            let steer_amount = effective_steer.abs();
            if steer_amount > 0.0 && forward_speed > 0.0 {
                let turn_drag = (1.0 - TURN_DRAG_PER_STEER * steer_amount * delta).clamp(0.0, 1.0);
                let linvel = rb.linvel();
                let lateral_component = right * lateral_speed;
                let forward_component = linvel - lateral_component;
                rb.set_linvel(forward_component * turn_drag + lateral_component, true);
            }

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

    fn broadcast_player_states(&mut self, delta: f64) {
        self.sync_timer += delta;
        if self.sync_timer < STATE_SYNC_INTERVAL {
            return;
        }
        self.sync_timer = 0.;

        let physics = &self.physics;
        let mut states = Vec::with_capacity(self.racers.len());
        for (nickname, racer) in &mut self.racers {
            let Some(rb) = physics.get(racer.rigid_body) else {
                continue;
            };
            let t = rb.translation();
            let r = rb.rotation();
            let rotation = stabilize_quaternion(
                racer.last_sent_rotation,
                QuatProto {
                    x: r.x,
                    y: r.y,
                    z: r.z,
                    w: r.w,
                },
            );
            racer.last_sent_rotation = Some(rotation);
            states.push(PlayerState {
                nickname: nickname.clone(),
                racing: racer.racing,
                laps: racer.laps,
                position: Vec3Proto { x: t.x, y: t.y, z: t.z },
                rotation,
                color: racer.color,
            });
        }

        self.broadcast_message(ServerMessage::State(LobbyState::Players(states)), false);
    }

    fn tick_state_machine(&mut self, delta: f64) {
        match self.state {
            State::Intermission => {
                if !self.intermission(delta) {
                    self.enter_starting();
                }
            }
            State::Starting => {
                if !self.starting(delta) {
                    self.enter_race();
                }
            }
            State::Racing => {
                if !self.race(delta) {
                    self.enter_intermission();
                }
            }
        }
    }

    fn intermission(&mut self, delta: f64) -> bool {
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
                self.broadcast_message(ServerMessage::State(LobbyState::WaitingForPlayers(waiting)), false);
                self.intermission_timer = 0.;
            }
            return true;
        }
        self.intermission_timer = 0.;
        false
    }

    fn enter_starting(&mut self) {
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
            let _ = racer.tx.send(outgoing_server_message(&ServerMessage::Event(
                LobbyEvent::RaceAboutToStart(spawn_info),
            )));
            racer.laps = 0;
            racer.prev_z = 0.0;
            racer.crossed_positive_z = false;
            racer.crossed_halfway = false;
            racer.finished = false;
            racer.racing = true;
            racer.reversing = false;
            racer.last_sent_rotation = None;
        }
        self.race_timer = 0.;
        self.finish_timer = 0.;
        self.finishers.clear();
        self.state = State::Starting;
    }

    fn starting(&mut self, delta: f64) -> bool {
        if self.start_timer < 5. {
            self.start_timer += delta;
            self.sync_countdown_timer += delta;
            if self.sync_countdown_timer > 1. {
                self.sync_countdown_timer = 0.;

                let time = (5. - self.start_timer).max(0.);
                let msg = outgoing_server_message(&ServerMessage::Event(LobbyEvent::Countdown { time }));
                for racer in self.racers.values() {
                    let _ = racer.tx.send(msg.clone());
                }
            }
            return true;
        }
        false
    }

    fn enter_race(&mut self) {
        self.sync_countdown_timer = 0.;
        self.start_timer = 0.;
        self.broadcast_message(ServerMessage::Event(LobbyEvent::RaceStarted(())), false);
        sr_log!(info, &self.name, "-> Racing ({} players)", self.racers.len());
        for racer in self.racers.values_mut() {
            racer.racing = true;
        }
        self.state = State::Racing;
    }

    fn race(&mut self, delta: f64) -> bool {
        self.race_timer += delta;

        if self.finish_timer > 0.0 {
            self.finish_timer -= delta;
            if self.finish_timer <= 0.0 {
                return false;
            }
        }

        let mut has_active_racer = false;
        for racer in self.racers.values() {
            if racer.racing {
                has_active_racer = true;
                if !racer.finished {
                    return true;
                }
            }
        }

        !has_active_racer
    }

    fn enter_intermission(&mut self) {
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
        );

        for racer in self.racers.values_mut() {
            racer.racing = false;
        }
        self.intermission_timer = 0.;
        self.state = State::Intermission;
    }

    fn check_lap_crossings(&mut self) {
        let physics = &self.physics;
        let finishers = &mut self.finishers;
        let finish_timer = &mut self.finish_timer;

        for racer in self.racers.values_mut() {
            let Some(rb) = physics.get(racer.rigid_body) else {
                continue;
            };
            let translation = rb.translation();
            let x = translation.x;
            let z = translation.z;

            if racer.finished || !racer.racing {
                racer.prev_z = z;
                continue;
            }

            if z > LAP_POSITIVE_Z {
                racer.crossed_positive_z = true;
            }

            if (x - CHECKPOINT_X).abs() < CHECKPOINT_X_HALF {
                racer.crossed_halfway = true;
            }

            let prev_z = racer.prev_z;

            if racer.crossed_positive_z
                && racer.crossed_halfway
                && prev_z >= 0.0
                && z < 0.0
                && (x - LAP_FINISH_X).abs() < LAP_FINISH_X_HALF
            {
                racer.laps += 1;
                racer.crossed_positive_z = false;
                racer.crossed_halfway = false;

                sr_log!(trace, &racer.nickname, "Lap {}/{}", racer.laps, LAPS_TO_WIN);

                if racer.laps >= LAPS_TO_WIN {
                    racer.finished = true;
                    finishers.push(racer.nickname.clone());
                    if *finish_timer == 0.0 {
                        *finish_timer = FINISH_WAIT_SECS;
                    }
                    continue;
                }
            }

            racer.prev_z = z;
        }
    }

    fn broadcast_message(&self, message: ServerMessage, for_racing_players: bool) {
        let message = outgoing_server_message(&message);
        for racer in self.racers.values() {
            if for_racing_players && !racer.racing {
                continue;
            }
            let _ = racer.tx.send(message.clone());
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

fn serialize_server_message(message: &ServerMessage) -> Message {
    Message::Text(serde_json::to_string(message).unwrap().into())
}

fn outgoing_server_message(message: &ServerMessage) -> OutgoingMessage {
    let encoded = serialize_server_message(message);
    match message {
        ServerMessage::State(_) => OutgoingMessage::State(encoded),
        ServerMessage::Event(_) | ServerMessage::Response(_) => OutgoingMessage::Reliable(encoded),
    }
}

fn collect_outgoing_batch(
    first: OutgoingMessage,
    rx_out: &mut tokio::sync::mpsc::UnboundedReceiver<OutgoingMessage>,
) -> Vec<Message> {
    fn push_message(batch: &mut Vec<Message>, latest_state: &mut Option<Message>, outgoing: OutgoingMessage) {
        match outgoing {
            OutgoingMessage::Reliable(message) => {
                if let Some(state) = latest_state.take() {
                    batch.push(state);
                }
                batch.push(message);
            }
            OutgoingMessage::State(message) => {
                *latest_state = Some(message);
            }
        }
    }

    let mut batch = Vec::with_capacity(4);
    let mut latest_state = None;

    push_message(&mut batch, &mut latest_state, first);
    while let Ok(next) = rx_out.try_recv() {
        push_message(&mut batch, &mut latest_state, next);
    }
    if let Some(state) = latest_state {
        batch.push(state);
    }

    batch
}

pub(crate) fn send_join_error(tx_out: &tokio::sync::mpsc::UnboundedSender<OutgoingMessage>, error: JoinError) {
    let msg = ServerMessage::Response(Response::LobbyJoined {
        track_id: 0,
        race_ongoing: false,
        min_players: 0,
        max_players: 0,
        error: Some(error),
    });
    let _ = tx_out.send(outgoing_server_message(&msg));
}

pub(crate) fn spawn_ws_writer(
    tx_stream: futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>,
) -> tokio::sync::mpsc::UnboundedSender<OutgoingMessage> {
    let (tx_out, mut rx_out) = tokio::sync::mpsc::unbounded_channel::<OutgoingMessage>();
    tokio::spawn(async move {
        let mut sink = tx_stream;
        while let Some(first) = rx_out.recv().await {
            for msg in collect_outgoing_batch(first, &mut rx_out) {
                if sink.send(msg).await.is_err() {
                    return;
                }
            }
        }
    });
    tx_out
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

#[cfg(test)]
mod tests {
    use super::{
        collect_outgoing_batch, effective_steer_input, stabilize_quaternion, update_reverse_mode, OutgoingMessage,
    };
    use crate::protocol::QuatProto;
    use tokio::sync::mpsc::unbounded_channel;
    use tungstenite::Message;

    fn text(label: &str) -> Message {
        Message::Text(label.to_string().into())
    }

    fn labels(messages: Vec<Message>) -> Vec<String> {
        messages
            .into_iter()
            .map(|message| match message {
                Message::Text(text) => text.to_string(),
                other => panic!("unexpected message: {:?}", other),
            })
            .collect()
    }

    #[test]
    fn outgoing_batch_keeps_only_latest_state() {
        let (tx, mut rx) = unbounded_channel();
        tx.send(OutgoingMessage::State(text("state-1"))).unwrap();
        tx.send(OutgoingMessage::State(text("state-2"))).unwrap();
        tx.send(OutgoingMessage::State(text("state-3"))).unwrap();

        let first = rx.try_recv().unwrap();
        let batch = collect_outgoing_batch(first, &mut rx);

        assert_eq!(labels(batch), vec!["state-3"]);
    }

    #[test]
    fn outgoing_batch_preserves_reliable_order_between_states() {
        let (tx, mut rx) = unbounded_channel();
        tx.send(OutgoingMessage::State(text("state-1"))).unwrap();
        tx.send(OutgoingMessage::State(text("state-2"))).unwrap();
        tx.send(OutgoingMessage::Reliable(text("event-a"))).unwrap();
        tx.send(OutgoingMessage::State(text("state-3"))).unwrap();
        tx.send(OutgoingMessage::Reliable(text("event-b"))).unwrap();
        tx.send(OutgoingMessage::State(text("state-4"))).unwrap();

        let first = rx.try_recv().unwrap();
        let batch = collect_outgoing_batch(first, &mut rx);

        assert_eq!(
            labels(batch),
            vec!["state-2", "event-a", "state-3", "event-b", "state-4"]
        );
    }

    #[test]
    fn reverse_mode_sticks_around_zero_speed() {
        assert!(update_reverse_mode(true, 0.0));
        assert!(update_reverse_mode(true, -0.1));
        assert!(!update_reverse_mode(false, 0.0));
    }

    #[test]
    fn reverse_mode_tracks_motion_direction() {
        assert!(update_reverse_mode(false, -1.0));
        assert!(!update_reverse_mode(true, 1.0));
    }

    #[test]
    fn steering_mirrors_only_in_reverse_mode() {
        assert_eq!(effective_steer_input(0.75, false), 0.75);
        assert_eq!(effective_steer_input(0.75, true), -0.75);
    }

    #[test]
    fn quaternion_sign_is_kept_continuous() {
        let prev = QuatProto {
            x: 0.0,
            y: 0.3,
            z: 0.0,
            w: 0.95,
        };
        let current = QuatProto {
            x: -prev.x,
            y: -prev.y,
            z: -prev.z,
            w: -prev.w,
        };

        let stabilized = stabilize_quaternion(Some(prev), current);

        assert!(stabilized.x >= 0.0 || prev.x == 0.0);
        assert_eq!(stabilized.y, prev.y);
        assert_eq!(stabilized.z, prev.z);
        assert_eq!(stabilized.w, prev.w);
    }
}
