mod config;
mod game;
mod map;
mod net;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderValue};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
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
        .route("/favicon.ico", get(favicon))
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
    axum::Json(server_info(&g))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InfoPlayer {
    id: usize,
    name: String,
    team: i8,
    status: i8,
    bot: bool,
    ping: i16,
    kills: i16,
    deaths: i16,
    score: i16,
    returns: i16,
    flag_attempts: i16,
    damage: i16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InfoTimer {
    game_time_left: f32,
    round_time_left: f32,
    round_state: i8,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InfoScore {
    blue: i16,
    red: i16,
    blue_wins: i16,
    red_wins: i16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InfoFlag {
    id: usize,
    name: &'static str,
    state: i8,
    position: [f32; 3],
    pod: [f32; 3],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerInfo {
    name: String,
    map: String,
    game_type: i8,
    players: usize,
    max_players: usize,
    timer: InfoTimer,
    team_score: InfoScore,
    flags: Vec<InfoFlag>,
    scoreboard: Vec<InfoPlayer>,
    recent_chat: Vec<crate::game::RecentChat>,
}

fn server_info(game: &GameServer) -> ServerInfo {
    let scoreboard: Vec<_> = game
        .players
        .iter()
        .flatten()
        .filter(|player| player.handshake_done || player.is_bot)
        .map(|player| InfoPlayer {
            id: player.slot,
            name: player.name.clone(),
            team: player.team_id,
            status: player.status,
            bot: player.is_bot,
            ping: player.ping_ms,
            kills: player.kills,
            deaths: player.deaths,
            score: player.score,
            returns: player.returns,
            flag_attempts: player.flag_attempts,
            damage: player.damage,
        })
        .collect();
    ServerInfo {
        name: game.config.game_name.clone(),
        map: game.config.map_name.clone(),
        game_type: game.config.game_type,
        players: scoreboard.len(),
        max_players: game.config.max_players,
        timer: InfoTimer {
            game_time_left: game.game_time_left,
            round_time_left: game.round_time_left,
            round_state: game.round_state,
        },
        team_score: InfoScore {
            blue: game.blue_score,
            red: game.red_score,
            blue_wins: game.blue_win,
            red_wins: game.red_win,
        },
        flags: (0..2)
            .map(|id| InfoFlag {
                id,
                name: if id == 0 { "blue" } else { "red" },
                state: game.ctf.flag_state[id],
                position: game.ctf.flag_pos[id],
                pod: game.ctf.flag_pod[id],
            })
            .collect(),
        scoreboard,
        recent_chat: game.recent_chat.iter().cloned().collect(),
    }
}

async fn favicon() -> impl IntoResponse {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    (
        headers,
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><circle cx="32" cy="32" r="29" fill="#101318" stroke="#fff" stroke-width="4"/><path d="M32 7v16M32 41v16M7 32h16M41 32h16" stroke="#e53935" stroke-width="7"/><circle cx="32" cy="32" r="7" fill="#2f80ed"/></svg>"##,
    )
}

async fn ws_handler(State(game): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(game, socket))
}

struct MovementBudget {
    tokens: f64,
    updated: std::time::Instant,
}

fn take_complete_frame(buf: &mut Vec<u8>) -> Result<Option<packet::NetFrame>, PacketError> {
    let frame = packet::decode_frame(buf)?;
    if let Some(ref complete) = frame {
        buf.drain(..4 + complete.payload.len());
    }
    Ok(frame)
}

async fn dispatch_client_frame(
    game: &AppState,
    babonet_id: u32,
    movement_budget: &mut MovementBudget,
    frame: packet::NetFrame,
) {
    if frame.type_id == crate::net::ids::NET_CLSV_SVCL_PLAYER_COORD_FRAME
        && !movement_budget.allow()
    {
        return;
    }
    game.write()
        .await
        .handle_frame(babonet_id, frame.type_id, &frame.payload);
}

fn coalesce_movement_frames(frames: Vec<packet::NetFrame>) -> Vec<packet::NetFrame> {
    let mut out = Vec::with_capacity(frames.len());
    let mut pending = None;
    for frame in frames {
        if frame.type_id == crate::net::ids::NET_CLSV_SVCL_PLAYER_COORD_FRAME {
            pending = Some(frame);
            continue;
        }
        if let Some(movement) = pending.take() {
            out.push(movement);
        }
        out.push(frame);
    }
    if let Some(movement) = pending {
        out.push(movement);
    }
    out
}

impl MovementBudget {
    const RATE: f64 = 60.0;
    const BURST: f64 = 12.0;

    fn new() -> Self {
        Self {
            tokens: Self::BURST,
            updated: std::time::Instant::now(),
        }
    }

    fn allow(&mut self) -> bool {
        let now = std::time::Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.updated).as_secs_f64() * Self::RATE)
            .min(Self::BURST);
        self.updated = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

async fn handle_socket(game: AppState, socket: WebSocket) {
    const OUTBOUND_QUEUE_CAPACITY: usize = 256;
    const MAX_BUFFERED_INBOUND: usize = 256 * 1024;
    const MAX_FRAMES_PER_MESSAGE: usize = 128;
    const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    let babonet_id = {
        let g = game.read().await;
        g.alloc_babonet_id()
    };
    let ip = "127.0.0.1".to_string();

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(OUTBOUND_QUEUE_CAPACITY);

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
    let mut movement_budget = MovementBudget::new();
    let handshake_deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
    'socket: loop {
        let authenticated = {
            let g = game.read().await;
            g.find_slot_by_babonet(babonet_id)
                .and_then(|slot| g.players[slot].as_ref())
                .is_some_and(|player| player.handshake_done)
        };
        let msg = if authenticated {
            ws_rx.next().await
        } else {
            match tokio::time::timeout_at(handshake_deadline, ws_rx.next()).await {
                Ok(message) => message,
                Err(_) => {
                    tracing::warn!(babonet_id, "handshake timed out");
                    break;
                }
            }
        };
        let Some(msg) = msg else { break };
        match msg {
            Ok(Message::Binary(data)) => {
                if buf.len().saturating_add(data.len()) > MAX_BUFFERED_INBOUND {
                    tracing::warn!(babonet_id, "inbound buffer limit exceeded");
                    break;
                }
                buf.extend_from_slice(&data);
                let mut frames_in_message = 0usize;
                let mut decoded = Vec::new();
                loop {
                    match take_complete_frame(&mut buf) {
                        Ok(Some(frame)) => {
                            frames_in_message += 1;
                            if frames_in_message > 4096 {
                                tracing::warn!(babonet_id, "excessive frame batch rejected");
                                break 'socket;
                            }
                            decoded.push(frame);
                        }
                        Ok(None) => break,
                        Err(PacketError::Incomplete { .. }) => break,
                    }
                }
                for (index, frame) in coalesce_movement_frames(decoded).into_iter().enumerate() {
                    dispatch_client_frame(&game, babonet_id, &mut movement_budget, frame).await;
                    if (index + 1).is_multiple_of(MAX_FRAMES_PER_MESSAGE) {
                        tokio::task::yield_now().await;
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

#[cfg(test)]
mod websocket_flow_tests {
    use super::{coalesce_movement_frames, take_complete_frame, MovementBudget};

    #[test]
    fn movement_budget_allows_small_jitter_burst_then_sheds_flood() {
        let mut budget = MovementBudget::new();
        for _ in 0..12 {
            assert!(budget.allow());
        }
        assert!(!budget.allow());
    }

    #[test]
    fn complete_tail_frames_do_not_wait_for_another_websocket_message() {
        let frame = crate::net::packet::encode_frame(crate::net::ids::NET_CLSV_PONG, &[1]);
        for count in [128, 129, 257] {
            let mut buf = frame.repeat(count);
            let mut drained = 0;
            while take_complete_frame(&mut buf).unwrap().is_some() {
                drained += 1;
            }
            assert_eq!(drained, count);
            assert!(buf.is_empty());
        }

        let mut fragmented = frame.clone();
        fragmented.extend_from_slice(&frame[..3]);
        assert!(take_complete_frame(&mut fragmented).unwrap().is_some());
        assert!(take_complete_frame(&mut fragmented).unwrap().is_none());
        assert_eq!(fragmented, frame[..3]);
    }

    #[test]
    fn movement_batches_are_newest_wins_without_reordering_control_frames() {
        let movement = |marker| crate::net::packet::NetFrame {
            type_id: crate::net::ids::NET_CLSV_SVCL_PLAYER_COORD_FRAME,
            payload: vec![marker],
        };
        let control = crate::net::packet::NetFrame {
            type_id: crate::net::ids::NET_CLSV_SVCL_TEAM_REQUEST,
            payload: vec![9],
        };
        let frames = coalesce_movement_frames(vec![
            movement(1),
            movement(2),
            control,
            movement(3),
            movement(4),
        ]);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].payload, [2]);
        assert_eq!(
            frames[1].type_id,
            crate::net::ids::NET_CLSV_SVCL_TEAM_REQUEST
        );
        assert_eq!(frames[2].payload, [4]);
    }
}
