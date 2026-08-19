mod config;
mod game;
mod map;
mod net;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, RwLock};
use tracing_subscriber::EnvFilter;

use crate::config::ServerConfig;
use crate::game::{
    bot_scenario_summary, configure_bot_scenario, spawn_initial_bots, GameServer, TICK_MS,
};
use crate::net::packet::{self, PacketError};

type AppState = Arc<RwLock<GameServer>>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("bv2_server=debug")),
        )
        .init();

    let mut config = ServerConfig::from_env();
    if std::env::args().any(|arg| arg == "--bot-sim") {
        if config.bot_count == 0 {
            config.bot_count = 12;
        }
        if let Err(e) = config.validate() {
            eprintln!("bv2-server bot simulation config error: {e}");
            std::process::exit(1);
        }
        let seed = std::env::var("BV2_BOT_SIM_SEED")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1);
        let ticks = std::env::var("BV2_BOT_SIM_TICKS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(3_000);
        let scenario =
            std::env::var("BV2_BOT_SIM_SCENARIO").unwrap_or_else(|_| "full_ctf_round".into());
        let mut game = GameServer::new(config);
        spawn_initial_bots(&mut game);
        configure_bot_scenario(&mut game, &scenario, seed);
        for _ in 0..ticks {
            game.tick(TICK_MS as f32 / 1000.0);
        }
        println!(
            "{}",
            serde_json::to_string(&bot_scenario_summary(&game, seed, ticks, &scenario)).unwrap()
        );
        return;
    }
    if let Err(e) = config.validate() {
        eprintln!("bv2-server config error: {e}");
        eprintln!("Example: BV2_CONTENT_ROOT=/path/to/Content/main BV2_MAP=CTF-BurialMound cargo run --release");
        std::process::exit(1);
    }
    let bind: SocketAddr = config
        .bind_addr
        .parse()
        .expect("BV2_BIND must be host:port");

    tracing::info!(
        "bv2-server map={} game_type={} bots={} content={:?}",
        config.map_name,
        config.game_type,
        config.bot_count,
        config.content_root
    );

    let state: AppState = Arc::new(RwLock::new(GameServer::new(config)));

    let tick_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(TICK_MS));
        loop {
            interval.tick().await;
            let dt = TICK_MS as f32 / 1000.0;
            tick_state.write().await.tick(dt);
        }
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/info", get(info))
        .route("/ws", get(ws_handler))
        .with_state(state);

    tracing::info!("listening on http://{bind}  websocket=ws://{bind}/ws");
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("bv2-server: port {bind} already in use ({e})");
            eprintln!("Stop the other process, or run: BV2_BIND=0.0.0.0:8081 cargo run --release");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("bv2-server: failed to bind {bind}: {e}");
            std::process::exit(1);
        }
    };
    axum::serve(listener, app).await.unwrap();
}

async fn health(State(game): State<AppState>) -> impl IntoResponse {
    let g = game.read().await;
    format!(
        "ok map={} players={} uptime={:.0}s\n",
        g.config.map_name,
        g.sessions.len(),
        g.uptime_secs()
    )
}

async fn info(State(game): State<AppState>) -> impl IntoResponse {
    let g = game.read().await;
    format!(
        "{{\"name\":\"{}\",\"map\":\"{}\",\"gameType\":{},\"players\":{},\"maxPlayers\":{}}}\n",
        g.config.game_name,
        g.config.map_name,
        g.config.game_type,
        g.sessions.len(),
        g.config.max_players
    )
}

async fn ws_handler(State(game): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(game, socket))
}

async fn handle_socket(game: AppState, socket: WebSocket) {
    let babonet_id = {
        let g = game.read().await;
        g.alloc_babonet_id()
    };
    let ip = "127.0.0.1".to_string();

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();

    {
        let mut g = game.write().await;
        g.register_session(babonet_id, tx);
        if g.connect_client(babonet_id, ip).is_none() {
            tracing::warn!("server full — disconnecting {babonet_id}");
            g.disconnect_babonet(babonet_id);
            return;
        }
    }

    let forward = tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if ws_tx.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    let mut buf = Vec::new();
    while let Some(msg) = ws_rx.next().await {
        match msg {
            Ok(Message::Binary(data)) => {
                buf.extend_from_slice(&data);
                loop {
                    match packet::decode_frame(&buf) {
                        Ok(Some(frame)) => {
                            let total = 4 + frame.payload.len();
                            buf.drain(..total);
                            let mut g = game.write().await;
                            g.handle_frame(babonet_id, frame.type_id, &frame.payload);
                        }
                        Ok(None) => break,
                        Err(PacketError::Incomplete { .. }) => break,
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }

    forward.abort();
    game.write().await.disconnect_babonet(babonet_id);
}
