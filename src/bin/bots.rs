use clap::Parser;
use colored::Colorize;
use futures_util::{SinkExt, StreamExt};
use regex::Regex;
use star_racer_server::protocol::{self, ClientMessage, Request};
use std::io::Write;
use std::{env, thread};
use tokio::task::JoinHandle;
use tokio::time::{sleep, Instant};
use tungstenite::Message;

const THREAD_ID_REGEX_STR: &str = "ThreadId\\(([[:digit:]]+)\\)";

const BOT_NAMES: &[&str] = &[
    "Blaze", "Viper", "Phantom", "Storm", "Phoenix", "Titan", "Echo", "Nova", "Cyber", "Shadow", "Nexus", "Forge", "Thunder", "Flux", "Prism", "Velocity", "Apex", "Rival", "Surge", "Axon", "Quest",
    "Zephyr", "Pulse", "Spectre", "Crux", "Helix", "Orbit", "Zenith", "Comet", "Sphinx",
];

fn generate_bot_name() -> String {
    let name = BOT_NAMES[(rand::random::<u8>() % BOT_NAMES.len() as u8) as usize];
    let number: usize = (rand::random::<u8>() % 255 as u8) as usize;
    format!("{}{}", name, number)
}

pub fn log_init(maybe_filter: Option<String>) {
    let launch_time = Instant::now();
    let target_regex_str = if let Some(filter) = maybe_filter {
        filter
    } else {
        let maybe_trace_filter = env::var("STARRACER_TRACE_FILTER");
        if maybe_trace_filter.is_err() {
            format!("(.*)")
        } else {
            maybe_trace_filter.unwrap()
        }
    };

    let mut binding = env_logger::builder();
    let builder = binding.format(move |buf, record| {
        let target_str = record.target();
        let regex = Regex::new(target_regex_str.as_str()).unwrap();
        let mut results = vec![];
        for (_, [target]) in regex.captures_iter(target_str).map(|c| c.extract()) {
            results.push(target);
        }

        if results.len() != 1 {
            return write!(buf, "");
        }

        let thread_id_str = format!("{:?}", thread::current().id());
        let regex = Regex::new(THREAD_ID_REGEX_STR).unwrap();
        let mut results = vec![];

        for (_, [id]) in regex.captures_iter(thread_id_str.as_str()).map(|c| c.extract()) {
            results.push(id);
        }
        assert_eq!(1, results.len());
        let thread_id_str = results.last().unwrap();

        let now_time = Instant::now();
        let elapsed = now_time - launch_time;
        let elapsed = elapsed.as_millis() as f32 / 1000.;

        let args_str = format!("{}", record.args());

        writeln!(buf, "{:>8}|{:>5}|{}", elapsed.to_string().truecolor(21, 153, 83), thread_id_str.truecolor(179, 7, 156), args_str,)
    });
    builder.init();
}

#[derive(Parser, Debug)]
#[command(version, long_about = None)]
struct Args {
    #[arg(value_name = "MIN_PLAYERS", default_value_t = 4)]
    min_players: u8,
    #[arg(value_name = "MAX_PLAYERS", default_value_t = 4)]
    max_players: u8,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    log_init(None);

    #[allow(unused_variables)]
    let args: Args = Args::parse();

    let mut hdls = Vec::new();

    hdls.push(launch_creator_bot("<BotsArgs>".to_string(), args.min_players, args.max_players).await);
    hdls.push(launch_creator_bot("<BotsSolo>".to_string(), 1, 1).await);
    hdls.push(launch_creator_bot("<BotsBig>".to_string(), 6, 6).await);
    hdls.push(launch_creator_bot("<BotsDuel>".to_string(), 2, 2).await);
    hdls.push(launch_creator_bot("<BotsAlone>".to_string(), 1, 6).await);

    hdls.push(launch_joiner_bot("<BotsSolo>".to_string()).await);
    hdls.push(launch_joiner_bot("<BotsBig>".to_string()).await);
    hdls.push(launch_joiner_bot("<BotsBig>".to_string()).await);
    hdls.push(launch_joiner_bot("<BotsBig>".to_string()).await);
    hdls.push(launch_joiner_bot("<BotsBig>".to_string()).await);
    hdls.push(launch_joiner_bot("<BotsDuel>".to_string()).await);
    hdls.push(launch_joiner_bot("<BotsAlone>".to_string()).await);

    for hdl in hdls {
        hdl.await??;
    }
    anyhow::Ok(())
}

async fn launch_joiner_bot(lobby_id: String) -> JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move {
        let ws = tokio_tungstenite::connect_async("ws://localhost:8080").await?;
        let (mut write, mut read) = ws.0.split();

        write
            .send(Message::Text(
                serde_json::to_string(&ClientMessage::Request(Request::JoinLobby {
                    lobby_id: lobby_id.into(),
                    nickname: generate_bot_name(),
                    color: (rand::random::<f32>() * 2. - 1., rand::random::<f32>() * 2. - 1., rand::random::<f32>() * 2. - 1.),
                }))
                .unwrap()
                .into(),
            ))
            .await?;

        if let Some(Ok(msg)) = read.next().await {
            if let Message::Text(text) = msg {
                let server_msg: protocol::ServerMessage = serde_json::from_str(&text).unwrap();
                if let protocol::ServerMessage::Response(response) = server_msg {
                    if let protocol::Response::LobbyJoined {
                        #[allow(unused_variables)]
                        success,
                        #[allow(unused_variables)]
                        info,
                        #[allow(unused_variables)]
                        race_ongoing,
                    } = response
                    {}
                }
            }
        }

        tokio::spawn(async move { while let Some(_) = read.next().await {} });

        // We send random velocity every 100ms in a task
        loop {
            sleep(std::time::Duration::from_millis(100)).await;
            let vel_x = rand::random::<f32>() * 2. - 1.;
            let vel_y = rand::random::<f32>() * 2. - 1.;
            let vel_z = rand::random::<f32>() * 2. - 1.;
            write
                .send(Message::Text(serde_json::to_string(&ClientMessage::Request(Request::Move { vel_x, vel_y, vel_z })).unwrap().into()))
                .await?;
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    })
}

async fn launch_creator_bot(lobby_id: String, min_players: u8, max_players: u8) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move {
        let ws = tokio_tungstenite::connect_async("ws://localhost:8080").await?;
        let (mut write, mut read) = ws.0.split();

        write
            .send(Message::Text(
                serde_json::to_string(&ClientMessage::Request(Request::CreateLobby {
                    lobby_id: lobby_id.into(),
                    nickname: generate_bot_name(),
                    color: (rand::random::<f32>() * 2. - 1., rand::random::<f32>() * 2. - 1., rand::random::<f32>() * 2. - 1.),
                    min_players,
                    max_players,
                }))
                .unwrap()
                .into(),
            ))
            .await?;

        if let Some(Ok(msg)) = read.next().await {
            if let Message::Text(text) = msg {
                let server_msg: protocol::ServerMessage = serde_json::from_str(&text).unwrap();
                if let protocol::ServerMessage::Response(response) = server_msg {
                    if let protocol::Response::LobbyJoined {
                        #[allow(unused_variables)]
                        success,
                        #[allow(unused_variables)]
                        info,
                        #[allow(unused_variables)]
                        race_ongoing,
                    } = response
                    {}
                }
            }
        }

        tokio::spawn(async move { while let Some(_) = read.next().await {} });

        // We send random velocity every 100ms in a task
        loop {
            sleep(std::time::Duration::from_millis(100)).await;
            let vel_x = rand::random::<f32>() * 2. - 1.;
            let vel_y = rand::random::<f32>() * 2. - 1.;
            let vel_z = rand::random::<f32>() * 2. - 1.;
            write
                .send(Message::Text(serde_json::to_string(&ClientMessage::Request(Request::Move { vel_x, vel_y, vel_z })).unwrap().into()))
                .await?;
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    })
}
