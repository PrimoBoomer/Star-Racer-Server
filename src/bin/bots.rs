use futures_util::{SinkExt, StreamExt};
use star_racer_server::protocol::{
    ClientMessage, ColorProto, LobbyState, QuatProto, RequestMessage, Response, ServerMessage,
};
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tungstenite::Message;

const ORBIT_RADIUS: f64 = 145.0;

const ORBIT_RADIUS_WIDE: f64 = 180.0;

const ORBIT_RADIUS_TIGHT: f64 = 115.0;

const LOOKAHEAD_M: f64 = 40.0;

const DRIFT_THRESHOLD: f64 = 0.60;

const DRIFT_MIN_SPEED: f64 = 3.0;

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

fn quat_right(q: &QuatProto) -> V2 {
    let (qx, qy, qz, qw) = (q.x, q.y, q.z, q.w);
    V2::new(1.0 - 2.0 * (qy * qy + qz * qz), 2.0 * (qx * qz - qw * qy))
}

#[derive(Clone, Copy, Debug)]
enum BotMode {
    Orbiter,

    Chaser,

    Racer,

    Drunk,

    Wallhugger,

    Hotshot,
}

#[derive(Clone)]
struct BotSnapshot {
    racing: bool,
    pos: V2,
    rot: QuatProto,
    speed: f64,
    others: Vec<V2>,
}

impl Default for BotSnapshot {
    fn default() -> Self {
        Self {
            racing: false,
            pos: V2::default(),
            rot: QuatProto {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            },
            speed: 0.0,
            others: Vec::new(),
        }
    }
}

fn signed_steer(mode: BotMode, snap: &BotSnapshot, tick: u64) -> f64 {
    match mode {
        BotMode::Orbiter => orbit_steer(snap, ORBIT_RADIUS),
        BotMode::Chaser => chaser_steer(snap),
        BotMode::Racer => orbit_steer(snap, ORBIT_RADIUS),
        BotMode::Drunk => {
            let base = orbit_steer(snap, ORBIT_RADIUS);

            let noise = ((tick as f64) * 0.31).sin() * 0.4;
            base + noise
        }
        BotMode::Wallhugger => orbit_steer(snap, ORBIT_RADIUS_WIDE),
        BotMode::Hotshot => orbit_steer(snap, ORBIT_RADIUS_TIGHT),
    }
    .clamp(-1.0, 1.0)
}

fn decide(mode: BotMode, snap: &BotSnapshot, tick: u64) -> (bool, f64, f64, bool) {
    let s = signed_steer(mode, snap, tick);
    let drift = match mode {
        BotMode::Racer => s.abs() > DRIFT_THRESHOLD && snap.speed > DRIFT_MIN_SPEED,

        BotMode::Hotshot => snap.speed > DRIFT_MIN_SPEED,

        BotMode::Drunk => (tick % 80) < 20 && snap.speed > DRIFT_MIN_SPEED,
        _ => false,
    };

    (true, (-s).max(0.0), s.max(0.0), drift)
}

fn orbit_steer(snap: &BotSnapshot, radius: f64) -> f64 {
    let p = snap.pos;
    let angle = p.z.atan2(p.x) - LOOKAHEAD_M / radius;
    let target = V2::new(radius * angle.cos(), radius * angle.sin());
    let to_target = target.sub(p).norm();
    quat_right(&snap.rot).dot(to_target)
}

fn chaser_steer(snap: &BotSnapshot) -> f64 {
    let nearest = snap
        .others
        .iter()
        .min_by(|a, b| a.sub(snap.pos).len().partial_cmp(&b.sub(snap.pos).len()).unwrap());
    match nearest {
        Some(&t) => quat_right(&snap.rot).dot(t.sub(snap.pos).norm()),
        None => orbit_steer(snap, ORBIT_RADIUS),
    }
}

struct BotConfig {
    lobby_id: &'static str,
    mode: BotMode,
    create: bool,
    min_players: u8,
    max_players: u8,
}

fn launch_bot(cfg: BotConfig) -> JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move {
        let (ws, _) = match tokio_tungstenite::connect_async("ws://localhost:8080").await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[bot] Connection failed: {e}");
                return anyhow::Ok(());
            }
        };
        let (mut write, mut read) = ws.split();

        let name = generate_bot_name();

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
        if write.send(to_msg(&req)).await.is_err() {
            eprintln!("[bot {name}] Failed to send join request");
            return anyhow::Ok(());
        }

        if let Some(Ok(Message::Text(text))) = read.next().await {
            if let Ok(ServerMessage::Response(Response::LobbyJoined { error: Some(e), .. })) =
                serde_json::from_str::<ServerMessage>(&text)
            {
                eprintln!("[bot {name}] join failed: {e:?}");
                return anyhow::Ok(());
            }
        }

        let snapshot = Arc::new(Mutex::new(BotSnapshot::default()));
        let snap_write = snapshot.clone();
        let my_name = name.clone();

        let disconnected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let disconnected_recv = disconnected.clone();

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
                    s.speed = new_pos.sub(s.pos).len() * 20.0;
                    s.pos = new_pos;
                    s.rot = me.rotation;
                }
            }

            disconnected_recv.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        let mut tick: u64 = 0;
        loop {
            sleep(std::time::Duration::from_millis(50)).await;
            if disconnected.load(std::sync::atomic::Ordering::Relaxed) {
                return anyhow::Ok(());
            }
            let snap = snapshot.lock().unwrap().clone();
            if !snap.racing {
                continue;
            }
            tick += 1;
            let (throttle, sl, sr, drift) = decide(cfg.mode, &snap, tick);
            if write
                .send(to_msg(&ClientMessage::State {
                    throttle,
                    steer_left: sl,
                    steer_right: sr,
                    star_drift: drift,
                }))
                .await
                .is_err()
            {
                eprintln!("[bot {name}] Server disconnected");
                return anyhow::Ok(());
            }
        }

        #[allow(unreachable_code)]
        anyhow::Ok(())
    })
}

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
    "Helix", "Orbit", "Zenith", "Comet", "Sphinx", "Drift", "Turbo", "Neon",
];

fn generate_bot_name() -> String {
    let name = BOT_NAMES[(rand::random::<u8>() % BOT_NAMES.len() as u8) as usize];
    format!("<Bot>{}{}", name, rand::random::<u8>() % 100)
}

async fn spawn_lobby(
    hdls: &mut Vec<JoinHandle<anyhow::Result<()>>>,
    lobby_id: &'static str,
    min_players: u8,
    max_players: u8,
    modes: &[BotMode],
) {
    if modes.is_empty() {
        return;
    }

    hdls.push(launch_bot(BotConfig {
        lobby_id,
        mode: modes[0],
        create: true,
        min_players,
        max_players,
    }));

    sleep(std::time::Duration::from_millis(1200)).await;

    for &mode in &modes[1..] {
        hdls.push(launch_bot(BotConfig {
            lobby_id,
            mode,
            create: false,
            min_players: 0,
            max_players: 0,
        }));
        sleep(std::time::Duration::from_millis(150)).await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let mut hdls: Vec<JoinHandle<anyhow::Result<()>>> = Vec::new();

    println!("Launching 6 showcase lobbies…");

    spawn_lobby(
        &mut hdls,
        "<Bots>AllStars",
        4,
        4,
        &[BotMode::Orbiter, BotMode::Chaser, BotMode::Racer, BotMode::Wallhugger],
    )
    .await;

    spawn_lobby(
        &mut hdls,
        "<Bots>DriftKings",
        3,
        3,
        &[BotMode::Hotshot, BotMode::Racer, BotMode::Drunk],
    )
    .await;

    spawn_lobby(
        &mut hdls,
        "<Bots>Chaos",
        4,
        4,
        &[BotMode::Drunk, BotMode::Hotshot, BotMode::Chaser, BotMode::Wallhugger],
    )
    .await;

    spawn_lobby(
        &mut hdls,
        "<Bots>Arena",
        4,
        4,
        &[BotMode::Racer, BotMode::Chaser, BotMode::Orbiter],
    )
    .await;

    spawn_lobby(&mut hdls, "<Bots>Duo", 2, 3, &[BotMode::Racer]).await;

    spawn_lobby(
        &mut hdls,
        "<Bots>FFA",
        6,
        6,
        &[
            BotMode::Orbiter,
            BotMode::Chaser,
            BotMode::Racer,
            BotMode::Drunk,
            BotMode::Hotshot,
        ],
    )
    .await;

    println!("All bots launched ({} total). Ctrl+C to stop.", hdls.len());

    for hdl in hdls {
        match hdl.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("[bot] Exited with error: {e}"),
            Err(e) => eprintln!("[bot] Task panicked: {e}"),
        }
    }
    anyhow::Ok(())
}
