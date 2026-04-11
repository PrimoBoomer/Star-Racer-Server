//! Star Racer bot launcher.
//!
//! Each bot is a regular WebSocket client — the server cannot distinguish
//! bots from human players.  Bots read their own position from the 20 Hz
//! server broadcasts and compute steering every 50 ms (20 Hz).
//!
//! # Bot modes
//! | mode     | behaviour |
//! |----------|-----------|
//! | spinner  | fixed full-left turn — deterministic circle, good for stress-tests |
//! | orbiter  | pure-pursuit lookahead on the ideal ring radius |
//! | chaser   | steers toward the nearest opponent; falls back to orbiter if alone |
//! | racer    | orbiter + automatic drift when cornering hard |
//!
//! # CLI
//! ```
//! bots [MODE] [MIN_PLAYERS] [MAX_PLAYERS] [BOT_MODE]
//! ```
//! * **MODE 1** — one lobby `<Bots>` filled with `MAX_PLAYERS` bots of the chosen mode.
//! * **MODE 2** — showcase: 4-player lobby mixing all four bot modes + a solo test lobby.

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use star_racer_server::protocol::{
    ClientMessage, ColorProto, LobbyState, QuatProto, RequestMessage, Response, ServerMessage,
};
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tungstenite::Message;

// ── Track geometry ─────────────────────────────────────────────────────────────
// Derived from the colliders in lobby.rs:
//   inner wall (center block) : half-extent 90 m  → inner edge ≈ 90 m from origin
//   outer walls               : positioned at ±225 m, thickness 25 m → drivable up to ≈ 200 m
//   spawn point               : (145, 3, 0)  → cars start on the +X side heading −Z (CW)

/// Ideal orbit radius (m).  Centred between inner (90 m) and outer (200 m) walls.
const ORBIT_RADIUS: f64 = 145.0;
/// Pure-pursuit lookahead distance along the orbit arc (m).
const LOOKAHEAD_M: f64 = 40.0;
/// Signed steer magnitude beyond which `Racer` activates drift.
const DRIFT_THRESHOLD: f64 = 0.60;
/// Minimum speed (m/s) before drift is sent — mirrors the server-side guard.
const DRIFT_MIN_SPEED: f64 = 3.0;

// ── 2-D geometry helpers (XZ plane, Y ignored for steering) ───────────────────

#[derive(Clone, Copy, Default)]
struct V2 {
    x: f64,
    z: f64,
}

impl V2 {
    fn new(x: f64, z: f64) -> Self {
        Self { x, z }
    }
    fn len(self) -> f64 {
        (self.x * self.x + self.z * self.z).sqrt()
    }
    fn norm(self) -> Self {
        let l = self.len();
        if l < 1e-9 {
            Self::new(0.0, 1.0)
        } else {
            Self::new(self.x / l, self.z / l)
        }
    }
    fn dot(self, o: Self) -> f64 {
        self.x * o.x + self.z * o.z
    }
    fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.z - o.z)
    }
}

/// Returns the XZ **right** direction from a quaternion.
/// Godot's local +X rotated to world space: right = q * (1, 0, 0).
fn quat_right(q: &QuatProto) -> V2 {
    let (qx, qy, qz, qw) = (q.x, q.y, q.z, q.w);
    V2::new(1.0 - 2.0 * (qy * qy + qz * qz), 2.0 * (qx * qz - qw * qy))
}

// ── Bot mode ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum BotMode {
    /// Fixed full-left steer: deterministic circle for stress testing.
    Spinner,
    /// Pure-pursuit orbit around the track center.
    Orbiter,
    /// Steers toward the nearest opponent; falls back to Orbiter when alone.
    Chaser,
    /// Orbiter with automatic drift on sharp corners.
    Racer,
}

// ── Shared state (written by receive task, read by control loop) ───────────────

#[derive(Clone)]
struct BotSnapshot {
    racing: bool,
    pos: V2,
    prev_pos: V2,
    rot: QuatProto,
    speed: f64,
    /// XZ positions of all other racing players.
    others: Vec<V2>,
}

impl Default for BotSnapshot {
    fn default() -> Self {
        Self {
            racing: false,
            pos: V2::default(),
            prev_pos: V2::default(),
            rot: QuatProto { x: 0.0, y: 0.0, z: 0.0, w: 1.0 },
            speed: 0.0,
            others: Vec::new(),
        }
    }
}

// ── Input decisions ────────────────────────────────────────────────────────────

/// Signed steer: +1.0 = full right, −1.0 = full left.
fn signed_steer(mode: BotMode, snap: &BotSnapshot) -> f64 {
    match mode {
        BotMode::Spinner => -1.0, // always full-left
        BotMode::Orbiter => orbit_steer(snap),
        BotMode::Chaser => chaser_steer(snap),
        BotMode::Racer => orbit_steer(snap), // same geometry, drift added separately
    }
    .clamp(-1.0, 1.0)
}

/// `(steer_left, steer_right, star_drift)` ready to send.
fn decide(mode: BotMode, snap: &BotSnapshot) -> (f64, f64, bool) {
    let s = signed_steer(mode, snap);
    let drift = matches!(mode, BotMode::Racer)
        && s.abs() > DRIFT_THRESHOLD
        && snap.speed > DRIFT_MIN_SPEED;
    ((-s).max(0.0), s.max(0.0), drift)
}

/// Pure-pursuit: look ahead LOOKAHEAD_M metres along the clockwise orbit and
/// steer toward that point.  Positive result = target is to our right.
fn orbit_steer(snap: &BotSnapshot) -> f64 {
    let p = snap.pos;
    // Current angle in the XZ plane; advance clockwise (negative delta).
    let angle = p.z.atan2(p.x) - LOOKAHEAD_M / ORBIT_RADIUS;
    let target = V2::new(ORBIT_RADIUS * angle.cos(), ORBIT_RADIUS * angle.sin());
    let to_target = target.sub(p).norm();
    quat_right(&snap.rot).dot(to_target)
}

/// Steers toward the nearest other player; falls back to orbit if no opponents.
fn chaser_steer(snap: &BotSnapshot) -> f64 {
    let nearest = snap
        .others
        .iter()
        .min_by(|a, b| a.sub(snap.pos).len().partial_cmp(&b.sub(snap.pos).len()).unwrap());
    match nearest {
        Some(&t) => quat_right(&snap.rot).dot(t.sub(snap.pos).norm()),
        None => orbit_steer(snap),
    }
}

// ── Bot lifecycle ──────────────────────────────────────────────────────────────

struct BotConfig {
    lobby_id: &'static str,
    mode: BotMode,
    create: bool,
    min_players: u8,
    max_players: u8,
}

fn launch_bot(cfg: BotConfig) -> JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move {
        let (ws, _) = tokio_tungstenite::connect_async("ws://localhost:8080").await?;
        let (mut write, mut read) = ws.split();

        let name = generate_bot_name();

        // ── Join / create ────────────────────────────────────────────────────
        let req = if cfg.create {
            ClientMessage::Request(RequestMessage::CreateLobby {
                lobby_id: cfg.lobby_id.into(),
                nickname: name.clone(),
                min_players: cfg.min_players,
                max_players: cfg.max_players,
                color: random_color(),
            })
        } else {
            ClientMessage::Request(RequestMessage::JoinLobby {
                lobby_id: cfg.lobby_id.into(),
                nickname: name.clone(),
                color: random_color(),
            })
        };
        write.send(to_msg(&req)).await?;

        // Wait for LobbyJoined confirmation; abort on error.
        if let Some(Ok(Message::Text(text))) = read.next().await {
            if let Ok(ServerMessage::Response(Response::LobbyJoined { error: Some(e), .. })) =
                serde_json::from_str::<ServerMessage>(&text)
            {
                eprintln!("[bot {name}] join failed: {e:?}");
                return anyhow::Ok(());
            }
        }

        // ── Receive task: keep BotSnapshot up to date ────────────────────────
        let snapshot = Arc::new(Mutex::new(BotSnapshot::default()));
        let snap_write = snapshot.clone();
        let my_name = name.clone();

        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let Ok(ServerMessage::State(LobbyState::Players(players))) =
                    serde_json::from_str::<ServerMessage>(&text)
                else {
                    continue;
                };
                let mut s = snap_write.lock().unwrap();
                s.others = players
                    .iter()
                    .filter(|p| p.nickname != my_name && p.racing)
                    .map(|p| V2::new(p.position.x, p.position.z))
                    .collect();
                if let Some(me) = players.iter().find(|p| p.nickname == my_name) {
                    s.racing = me.racing;
                    let new_pos = V2::new(me.position.x, me.position.z);
                    // Estimate speed from position delta between broadcasts (~50 ms = 20 Hz).
                    s.speed = new_pos.sub(s.pos).len() * 20.0;
                    s.prev_pos = s.pos;
                    s.pos = new_pos;
                    s.rot = me.rotation;
                }
            }
        });

        // ── Control loop at 20 Hz ────────────────────────────────────────────
        loop {
            sleep(std::time::Duration::from_millis(50)).await;
            let snap = snapshot.lock().unwrap().clone();
            if !snap.racing {
                continue;
            }
            let (sl, sr, drift) = decide(cfg.mode, &snap);
            write
                .send(to_msg(&ClientMessage::State {
                    throttle: true,
                    steer_left: sl,
                    steer_right: sr,
                    star_drift: drift,
                }))
                .await?;
        }

        #[allow(unreachable_code)]
        anyhow::Ok(())
    })
}

// ── Utilities ──────────────────────────────────────────────────────────────────

fn to_msg(v: &impl serde::Serialize) -> Message {
    Message::Text(serde_json::to_string(v).unwrap().into())
}

fn random_color() -> ColorProto {
    ColorProto {
        x: rand::random(),
        y: rand::random(),
        z: rand::random(),
    }
}

const BOT_NAMES: &[&str] = &[
    "Blaze", "Viper", "Phantom", "Storm", "Phoenix", "Titan", "Echo", "Nova", "Cyber", "Shadow", "Nexus", "Forge",
    "Thunder", "Flux", "Prism", "Velocity", "Apex", "Rival", "Surge", "Axon", "Zephyr", "Pulse", "Spectre", "Crux",
    "Helix", "Orbit", "Zenith", "Comet", "Sphinx", "Drift",
];

fn generate_bot_name() -> String {
    let name = BOT_NAMES[(rand::random::<u8>() % BOT_NAMES.len() as u8) as usize];
    format!("{}{}", name, rand::random::<u8>() % 100)
}

// ── CLI ────────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(version, about = "Star Racer bot launcher")]
struct Args {
    /// 1 = single configurable lobby, 2 = multi-mode showcase
    #[arg(default_value_t = 1)]
    mode: u8,
    #[arg(default_value_t = 4)]
    min_players: u8,
    #[arg(default_value_t = 4)]
    max_players: u8,
    /// Bot behaviour (mode 1 only)
    #[arg(value_enum, default_value = "orbiter")]
    bot_mode: BotMode,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args = Args::parse();
    let mut hdls: Vec<JoinHandle<anyhow::Result<()>>> = Vec::new();

    match args.mode {
        // ── Mode 1: single lobby, all bots of the same type ─────────────────
        1 => {
            hdls.push(launch_bot(BotConfig {
                lobby_id: "<Bots>",
                mode: args.bot_mode,
                create: true,
                min_players: args.min_players,
                max_players: args.max_players,
            }));
            // Give the server time to register the lobby before joiners arrive.
            sleep(std::time::Duration::from_millis(400)).await;
            for _ in 1..args.max_players - 1 {
                hdls.push(launch_bot(BotConfig {
                    lobby_id: "<Bots>",
                    mode: args.bot_mode,
                    create: false,
                    min_players: 0,
                    max_players: 0,
                }));
                sleep(std::time::Duration::from_millis(80)).await;
            }
        }

        // ── Mode 2: showcase — one bot of each type racing together ──────────
        2 => {
            // 4-player lobby with one of each mode
            hdls.push(launch_bot(BotConfig {
                lobby_id: "<Showcase>",
                mode: BotMode::Racer,
                create: true,
                min_players: 4,
                max_players: 4,
            }));
            sleep(std::time::Duration::from_millis(400)).await;
            for mode in [BotMode::Orbiter, BotMode::Chaser, BotMode::Spinner] {
                hdls.push(launch_bot(BotConfig {
                    lobby_id: "<Showcase>",
                    mode,
                    create: false,
                    min_players: 0,
                    max_players: 0,
                }));
                sleep(std::time::Duration::from_millis(80)).await;
            }
            // Solo lobby for isolated single-bot observation
            hdls.push(launch_bot(BotConfig {
                lobby_id: "<Solo>",
                mode: BotMode::Racer,
                create: true,
                min_players: 1,
                max_players: 1,
            }));
        }

        _ => eprintln!("Unknown mode {}. Use 1 or 2.", args.mode),
    }

    for hdl in hdls {
        hdl.await??;
    }
    anyhow::Ok(())
}
