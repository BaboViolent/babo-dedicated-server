use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::mpsc;

use crate::config::{load_map_spawns, MapSpawns, ServerConfig};
use crate::map::{perform_collision_xy, MapGrid};
use crate::net::ids;
use crate::net::packet;

mod bot_roster;
mod bots;
mod ctf;
mod projectile;
mod shoot;
mod snd;
mod voting;

pub use bots::{
    bot_scenario_summary, configure_bot_scenario, reconcile_bot_roster, spawn_initial_bots,
    tick_bots, BotBrain,
};
pub use ctf::{send_flag_enum_to, CtfState, GAME_TYPE_CTF};
pub use voting::VoteState;

pub const MAX_PLAYER: usize = 32;
pub const TICK_MS: u64 = 20;

pub const PLAYER_STATUS_ALIVE: i8 = 0;
pub const PLAYER_STATUS_DEAD: i8 = 1;
pub const PLAYER_STATUS_LOADING: i8 = 2;

pub const PLAYER_TEAM_SPECTATOR: i8 = -1;
pub const PLAYER_TEAM_BLUE: i8 = 0;
pub const PLAYER_TEAM_RED: i8 = 1;
/** Chat addressed to every team (ServerRecv.cpp:562). */
pub const CHAT_TEAM_ALL: i8 = -2;
const IDLE_SPECTATE_SECONDS: f32 = 30.0;
const IDLE_COUNTDOWN_SECONDS: u8 = 10;
const IDLE_MOVEMENT_EPSILON_SQ: f32 = 0.05 * 0.05;
const RECENT_CHAT_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BotIdentity {
    pub name: String,
    pub version: String,
    pub author: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentChat {
    pub player_id: usize,
    pub player_name: String,
    pub message: String,
    pub at_seconds: f64,
}

fn wire_player_slot(payload: &[u8]) -> Option<usize> {
    let slot = *payload.first()? as usize;
    (slot < MAX_PLAYER).then_some(slot)
}

fn is_advertised_map(map_list: &[String], name: &str) -> bool {
    map_list.iter().any(|advertised| advertised == name)
}

fn map_allowed_for_game_type(game_type: i8, name: &str) -> bool {
    if game_type == GAME_TYPE_SND {
        name.starts_with("CHP-")
    } else {
        !name.starts_with("CHP-")
    }
}

fn map_compatible_for_game_type(game_type: i8, spawns: &MapSpawns) -> bool {
    match game_type {
        GAME_TYPE_CTF => {
            spawns.flag_pod.iter().all(|pod| *pod != [0.0; 3])
                && !spawns.blue.is_empty()
                && !spawns.red.is_empty()
        }
        1 => !spawns.blue.is_empty() && !spawns.red.is_empty(),
        _ => !spawns.dm.is_empty(),
    }
}

fn valid_player_team(team: i8) -> bool {
    matches!(
        team,
        PLAYER_TEAM_SPECTATOR | PLAYER_TEAM_BLUE | PLAYER_TEAM_RED
    )
}

fn duration_ms_i16(duration: Duration) -> i16 {
    duration.as_millis().min(i16::MAX as u128) as i16
}

/// GameVar.cpp sv_timeToSpawn — seconds before dead players may spawn.
pub const SV_TIME_TO_SPAWN: f32 = 5.0;
pub const SPAWN_IMMUNITY_TIME: f32 = 2.0;
pub const GAME_PLAYING: i8 = -1;
pub const GAME_BLUE_WIN: i8 = 0;
pub const GAME_RED_WIN: i8 = 1;
pub const GAME_DRAW: i8 = 2;
pub const GAME_TYPE_SND: i8 = 3;
const AUTO_BALANCE_DELAY: f32 = 4.0;

pub type BaboNetId = u32;

#[derive(Debug, Clone)]
pub struct CoordFrame {
    pub frame_id: i32,
    pub position: [f32; 3],
    pub vel: [f32; 3],
    pub mouse: [f32; 3],
}

impl Default for CoordFrame {
    fn default() -> Self {
        Self {
            frame_id: 0,
            position: [0.0, 0.0, 0.25],
            vel: [0.0, 0.0, 0.0],
            mouse: [0.0, 0.0, 0.0],
        }
    }
}

#[derive(Debug)]
pub struct Player {
    pub slot: usize,
    pub babonet_id: BaboNetId,
    pub name: String,
    pub ip: String,
    pub team_id: i8,
    pub status: i8,
    pub weapon_id: i8,
    pub melee_id: i8,
    pub skin: [u8; 7],
    pub red_decal: [u8; 3],
    pub green_decal: [u8; 3],
    pub blue_decal: [u8; 3],
    pub kills: i16,
    pub deaths: i16,
    pub score: i16,
    pub returns: i16,
    pub flag_attempts: i16,
    pub damage: i16,
    pub life: f32,
    pub dmg: f32,
    pub cf: CoordFrame,
    pub wait_for_pong: bool,
    pub ping_cycle_started: Instant,
    pub ping_sent_at: Option<Instant>,
    pub ping_ms: i16,
    pub handshake_done: bool,
    pub spawned_once: bool,
    pub is_bot: bool,
    /// Remotely controlled bot connected through `/bot/ws`.
    pub external_bot: bool,
    pub bot_identity: Option<BotIdentity>,
    pub bot_fire_cooldown: f32,
    pub bot_respawn_timer: f32,
    pub bot_wander_angle: f32,
    pub bot_stuck_time: f32,
    pub bot_goal_refresh: f32,
    pub bot_goal: [f32; 2],
    pub bot_brain: BotBrain,
    pub time_to_spawn: f32,
    /// `_PRO_` Champion/S&D keeps one five-point spawn group per mini-round.
    pub snd_spawn_slot: Option<usize>,
    /// Server-owned weapon clock (`Player::mfElapsedSinceLastShot`).
    pub elapsed_since_last_shot: f32,
    /// Server-owned flamethrower hold timer (used for authoritative aim spread timing).
    pub flame_seconds_fired: f32,
    /// Server-owned native bloom angle in degrees (`Weapon::currentImp`).
    pub weapon_current_imp: f32,
    pub weapon_shot_sequence: u32,
    /// Number of pellets accepted in the current shotgun/sniper burst.
    pub burst_shot_count: u8,
    pub grenades_left: u8,
    pub molotovs_left: u8,
    pub grenade_elapsed: f32,
    pub molotov_elapsed: f32,
    pub melee_elapsed: f32,
    pub protection_remaining: f32,
    pub spawn_immunity_remaining: f32,
    pub nuke_remaining: f32,
    pub photon_frames: u8,
    pub photon_from: [f32; 3],
    pub photon_to: [f32; 3],
    /// Server time since the last accepted client movement sample.
    pub coord_elapsed: f32,
    /// Time spent at effectively the same world position while alive.
    pub idle_elapsed: f32,
    pub idle_position: [f32; 3],
    pub idle_warning: Option<u8>,
}

impl Player {
    pub fn new(slot: usize, babonet_id: BaboNetId, ip: String) -> Self {
        Self {
            slot,
            babonet_id,
            name: format!("Player{slot}"),
            ip,
            team_id: PLAYER_TEAM_SPECTATOR,
            status: PLAYER_STATUS_LOADING,
            weapon_id: 0,
            melee_id: 8,
            skin: *b"skin10\0",
            red_decal: [255, 255, 255],
            green_decal: [255, 255, 255],
            blue_decal: [255, 255, 255],
            kills: 0,
            deaths: 0,
            score: 0,
            returns: 0,
            flag_attempts: 0,
            damage: 0,
            life: 1.0,
            dmg: 0.0,
            cf: CoordFrame::default(),
            wait_for_pong: false,
            ping_cycle_started: Instant::now(),
            ping_sent_at: None,
            ping_ms: 0,
            handshake_done: false,
            spawned_once: false,
            is_bot: false,
            external_bot: false,
            bot_identity: None,
            bot_fire_cooldown: 0.0,
            bot_respawn_timer: 0.0,
            bot_wander_angle: 0.0,
            bot_stuck_time: 0.0,
            bot_goal_refresh: 0.0,
            bot_goal: [0.0, 0.0],
            bot_brain: BotBrain::default(),
            time_to_spawn: 0.0,
            snd_spawn_slot: None,
            // A newly-created/spawned player may fire immediately.
            elapsed_since_last_shot: f32::INFINITY,
            flame_seconds_fired: 0.0,
            weapon_current_imp: 1.0,
            weapon_shot_sequence: 0,
            burst_shot_count: 0,
            grenades_left: 2,
            molotovs_left: 1,
            grenade_elapsed: f32::INFINITY,
            molotov_elapsed: f32::INFINITY,
            melee_elapsed: f32::INFINITY,
            protection_remaining: 0.0,
            spawn_immunity_remaining: 0.0,
            nuke_remaining: 0.0,
            photon_frames: 0,
            photon_from: [0.0; 3],
            photon_to: [0.0; 3],
            coord_elapsed: 0.0,
            idle_elapsed: 0.0,
            idle_position: [0.0, 0.0, 0.25],
            idle_warning: None,
        }
    }

    pub fn enum_state_bytes(&self) -> Vec<u8> {
        let mut p = vec![0u8; 96];
        p[0] = self.slot as u8;
        write_name31(&mut p[1..33], &self.name);
        p[33] = self.team_id as u8;
        p[34] = self.status as u8;
        p[35..37].copy_from_slice(&self.kills.to_le_bytes());
        p[37..39].copy_from_slice(&self.deaths.to_le_bytes());
        p[39..41].copy_from_slice(&self.score.to_le_bytes());
        p[41..43].copy_from_slice(&self.returns.to_le_bytes());
        p[43..45].copy_from_slice(&self.flag_attempts.to_le_bytes());
        p[45..47].copy_from_slice(&self.damage.to_le_bytes());
        p[47..51].copy_from_slice(&self.life.to_le_bytes());
        p[51..55].copy_from_slice(&self.dmg.to_le_bytes());
        p[55] = self.weapon_id as u8;
        write_ip16(&mut p[56..72], &self.ip);
        p[72..76].copy_from_slice(&(self.babonet_id as i32).to_le_bytes());
        p[76..83].copy_from_slice(&self.skin);
        p[83..86].copy_from_slice(&self.red_decal);
        p[86..89].copy_from_slice(&self.green_decal);
        p[89..92].copy_from_slice(&self.blue_decal);
        p
    }
}

#[derive(Debug)]
pub struct MapTransfer {
    pub babonet_id: BaboNetId,
    pub data: Vec<u8>,
    pub offset: usize,
}

pub struct SessionSender {
    tx: mpsc::Sender<Vec<u8>>,
    reliable_backpressure: Arc<AtomicBool>,
}

pub struct GameServer {
    pub config: ServerConfig,
    pub players: [Option<Player>; MAX_PLAYER],
    pub sessions: HashMap<BaboNetId, SessionSender>,
    request_limits: HashMap<BaboNetId, RequestLimit>,
    next_babonet: AtomicU32,
    pub map_spawns: MapSpawns,
    pub map_grid: MapGrid,
    pub map_list: Vec<String>,
    pub blue_score: i16,
    pub red_score: i16,
    pub blue_win: i16,
    pub red_win: i16,
    pub round_state: i8,
    pub round_end_remaining: f32,
    pub auto_balance_remaining: f32,
    pub frame_id: i32,
    pub game_time_left: f32,
    pub round_time_left: f32,
    pub map_transfers: Vec<MapTransfer>,
    pub projectiles: Vec<projectile::ServerProjectile>,
    pub next_projectile_id: i32,
    diagnostics_enabled: bool,
    pub ctf: CtfState,
    pub voting_enabled: bool,
    pub vote: VoteState,
    pub recent_chat: VecDeque<RecentChat>,
    started: Instant,
}

#[derive(Debug)]
struct RequestLimit {
    started: Instant,
    chat: u8,
    vote: u8,
    map: u8,
    profile: u8,
}

impl Default for RequestLimit {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            chat: 0,
            vote: 0,
            map: 0,
            profile: 0,
        }
    }
}

impl GameServer {
    pub fn new(config: ServerConfig) -> Self {
        let game_time_limit = config.game_time_limit;
        let round_time_left = if config.game_type == GAME_TYPE_SND {
            snd::SND_ROUND_TIME
        } else {
            600.0
        };
        let map_path = config.map_path(&config.map_name);
        let map_spawns = load_map_spawns(&map_path).unwrap_or_else(|e| {
            tracing::warn!("map spawn parse {:?}: {e}", map_path);
            MapSpawns {
                name: config.map_name.clone(),
                ..Default::default()
            }
        });
        let map_grid = MapGrid::load(&map_path).unwrap_or_else(|e| {
            tracing::warn!("map grid parse {:?}: {e}", map_path);
            MapGrid::empty(map_spawns.clone())
        });
        tracing::info!(
            "map {} ({}x{}, {} dm spawns)",
            map_spawns.name,
            map_spawns.size_x,
            map_spawns.size_y,
            map_spawns.dm.len()
        );
        let flag_pod = ctf::map_flag_pod(&map_spawns);
        let ctf_state = CtfState::new(flag_pod);
        if config.game_type == GAME_TYPE_CTF {
            if ctf_state.supports_ctf() {
                tracing::info!(
                    "CTF flags: blue pod ({:.1},{:.1}) red pod ({:.1},{:.1})",
                    flag_pod[0][0],
                    flag_pod[0][1],
                    flag_pod[1][0],
                    flag_pod[1][1],
                );
            } else {
                tracing::warn!(
                    "map {} has no flag pods — CTF scoring disabled",
                    map_spawns.name
                );
            }
        }
        let map_list = crate::config::list_maps(&config.maps_dir());
        let mut server = Self {
            config,
            players: std::array::from_fn(|_| None),
            sessions: HashMap::new(),
            request_limits: HashMap::new(),
            next_babonet: AtomicU32::new(1),
            map_spawns,
            map_grid,
            map_list,
            blue_score: 0,
            red_score: 0,
            blue_win: 0,
            red_win: 0,
            round_state: GAME_PLAYING,
            round_end_remaining: 0.0,
            auto_balance_remaining: 0.0,
            frame_id: 0,
            game_time_left: game_time_limit,
            round_time_left,
            map_transfers: Vec::new(),
            projectiles: Vec::new(),
            next_projectile_id: 1,
            diagnostics_enabled: std::env::var("BV2_DIAGNOSTICS").is_ok_and(|value| {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            }),
            ctf: ctf_state,
            voting_enabled: std::env::var("BV2_ENABLE_VOTE").is_ok_and(|value| {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            }),
            vote: VoteState::default(),
            recent_chat: VecDeque::with_capacity(RECENT_CHAT_LIMIT),
            started: Instant::now(),
        };
        spawn_initial_bots(&mut server);
        server
    }

    pub fn alloc_babonet_id(&self) -> BaboNetId {
        self.next_babonet.fetch_add(1, Ordering::Relaxed)
    }

    pub fn register_session(&mut self, id: BaboNetId, tx: mpsc::Sender<Vec<u8>>) {
        self.sessions.insert(
            id,
            SessionSender {
                tx,
                reliable_backpressure: Arc::new(AtomicBool::new(false)),
            },
        );
        self.request_limits.insert(id, RequestLimit::default());
    }

    pub fn unregister_session(&mut self, id: BaboNetId) {
        self.sessions.remove(&id);
        self.request_limits.remove(&id);
    }

    pub fn send_to(&self, id: BaboNetId, data: Vec<u8>) {
        if let Some(session) = self.sessions.get(&id) {
            let coalescible = coalescible_packet(&data);
            if session.tx.try_send(data).is_err() && !coalescible {
                session.reliable_backpressure.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn broadcast(&self, data: Vec<u8>) {
        for session in self.sessions.values() {
            if session.tx.try_send(data.clone()).is_err() && !coalescible_packet(&data) {
                session.reliable_backpressure.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn find_slot_by_babonet(&self, id: BaboNetId) -> Option<usize> {
        self.players
            .iter()
            .position(|p| p.as_ref().map(|pl| pl.babonet_id == id).unwrap_or(false))
    }

    pub fn connect_client(&mut self, babonet_id: BaboNetId, ip: String) -> Option<usize> {
        self.connect_player(babonet_id, ip, false)
    }

    pub fn connect_external_bot(&mut self, babonet_id: BaboNetId, ip: String) -> Option<usize> {
        self.connect_player(babonet_id, ip, true)
    }

    fn connect_player(
        &mut self,
        babonet_id: BaboNetId,
        ip: String,
        external_bot: bool,
    ) -> Option<usize> {
        let max = self.config.max_players.min(MAX_PLAYER);
        if !(0..max).any(|slot| self.players[slot].is_none()) {
            if let Some(slot) = (0..max).rev().find(|&slot| {
                self.players[slot]
                    .as_ref()
                    .is_some_and(|player| player.is_bot)
            }) {
                self.remove_bot(slot);
            }
        }
        for slot in 0..max {
            if self.players[slot].is_none() {
                let mut player = Player::new(slot, babonet_id, ip);
                player.external_bot = external_bot;
                if player.external_bot {
                    player.name = bot_display_name(&player.name);
                }
                self.players[slot] = Some(player);
                let pid = slot as i8;
                self.broadcast(packet::newplayer(pid, babonet_id as i32));
                self.send_to(babonet_id, packet::gameversion());
                reconcile_bot_roster(self);
                tracing::info!(
                    "player slot {slot} babonet {babonet_id} external_bot={external_bot}"
                );
                return Some(slot);
            }
        }
        None
    }

    pub fn disconnect_babonet(&mut self, babonet_id: BaboNetId) {
        let mut removed_human = false;
        if let Some(slot) = self.find_slot_by_babonet(babonet_id) {
            let pid = slot as i8;
            ctf::drop_carrier_flags(self, slot);
            removed_human = self.players[slot]
                .as_ref()
                .is_some_and(|player| !player.is_bot);
            self.players[slot] = None;
            self.broadcast(packet::player_disconnect(pid));
            tracing::info!("disconnect slot {slot}");
        }
        self.unregister_session(babonet_id);
        self.map_transfers
            .retain(|transfer| transfer.babonet_id != babonet_id);
        if self.config.game_type == GAME_TYPE_SND {
            snd::rebalance_sections(self);
        }
        if removed_human {
            reconcile_bot_roster(self);
        }
    }

    fn team_counts_without(&self, excluded: usize) -> (usize, usize) {
        self.players
            .iter()
            .enumerate()
            .filter(|(slot, _)| *slot != excluded)
            .filter_map(|(_, player)| player.as_ref())
            .fold((0, 0), |(blue, red), player| match player.team_id {
                PLAYER_TEAM_BLUE => (blue + 1, red),
                PLAYER_TEAM_RED => (blue, red + 1),
                _ => (blue, red),
            })
    }

    fn team_join_allowed(&self, slot: usize, requested: i8) -> bool {
        if requested == PLAYER_TEAM_SPECTATOR || !matches!(self.config.game_type, 1 | GAME_TYPE_CTF)
        {
            return true;
        }
        let (mut blue, mut red) = self.team_counts_without(slot);
        if requested == PLAYER_TEAM_BLUE {
            blue += 1;
        } else {
            red += 1;
        }
        blue.abs_diff(red) <= 1
    }

    fn tick_auto_balance(&mut self, dt: f32) {
        if !matches!(self.config.game_type, 1 | GAME_TYPE_CTF) {
            self.auto_balance_remaining = 0.0;
            return;
        }
        let (blue, red) = self.team_counts_without(MAX_PLAYER);
        if blue.abs_diff(red) <= 1 {
            self.auto_balance_remaining = 0.0;
            return;
        }
        if self.auto_balance_remaining == 0.0 {
            self.auto_balance_remaining = AUTO_BALANCE_DELAY;
            self.broadcast(packet::encode_frame(ids::NET_SVCL_AUTOBALANCE, &[]));
            return;
        }
        self.auto_balance_remaining = (self.auto_balance_remaining - dt).max(0.0);
        if self.auto_balance_remaining > 0.0 {
            return;
        }
        let (from, to, carried_flag) = if blue > red {
            (PLAYER_TEAM_BLUE, PLAYER_TEAM_RED, 1usize)
        } else {
            (PLAYER_TEAM_RED, PLAYER_TEAM_BLUE, 0usize)
        };
        let eligible = |slot: usize, bots_only: bool| {
            self.ctf.flag_state[carried_flag] != slot as i8
                && self.players[slot]
                    .as_ref()
                    .is_some_and(|player| player.team_id == from && (!bots_only || player.is_bot))
        };
        let candidate = (0..MAX_PLAYER)
            .rev()
            .find(|&slot| eligible(slot, true))
            .or_else(|| (0..MAX_PLAYER).rev().find(|&slot| eligible(slot, false)));
        if let Some(slot) = candidate {
            if let Some(player) = self.players[slot].as_mut() {
                player.team_id = to;
            }
            self.broadcast(packet::encode_frame(
                ids::NET_CLSV_SVCL_TEAM_REQUEST,
                &[slot as u8, to as u8],
            ));
        }
    }

    /// Native GameSpawn.cpp selection, with occupied-point tie avoidance.
    pub fn pick_spawn_for(&self, team_id: i8, exclude_slot: Option<usize>) -> [f32; 3] {
        let list = if !self.map_spawns.dm.is_empty() {
            &self.map_spawns.dm
        } else {
            match team_id {
                PLAYER_TEAM_BLUE if !self.map_spawns.blue.is_empty() => &self.map_spawns.blue,
                PLAYER_TEAM_RED if !self.map_spawns.red.is_empty() => &self.map_spawns.red,
                _ => {
                    return [
                        self.map_spawns.size_x as f32 * 0.5,
                        self.map_spawns.size_y as f32 * 0.5,
                        0.25,
                    ];
                }
            }
        };
        let relevant: Vec<_> = self
            .players
            .iter()
            .enumerate()
            .filter_map(|(slot, player)| {
                let player = player.as_ref()?;
                if Some(slot) == exclude_slot || player.status != PLAYER_STATUS_ALIVE {
                    return None;
                }
                let enemy =
                    matches!(self.config.game_type, 0 | GAME_TYPE_SND) || player.team_id != team_id;
                enemy.then_some(player.cf.position)
            })
            .collect();
        if relevant.is_empty() {
            return list[(self.frame_id as usize + exclude_slot.unwrap_or(0)) % list.len()];
        }
        let score = |point: &[f32; 3]| {
            relevant.iter().fold(f32::INFINITY, |nearest, player| {
                nearest.min((point[0] - player[0]).powi(2) + (point[1] - player[1]).powi(2))
            })
        };
        list.iter()
            .copied()
            .enumerate()
            .max_by(|(ia, a), (ib, b)| score(a).total_cmp(&score(b)).then_with(|| ib.cmp(ia)))
            .map(|(_, point)| [point[0], point[1], 0.25])
            .unwrap()
    }

    pub fn tick(&mut self, dt: f32) {
        let stalled: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(id, session)| {
                session
                    .reliable_backpressure
                    .load(Ordering::Relaxed)
                    .then_some(*id)
            })
            .collect();
        for id in stalled {
            tracing::warn!(id, "disconnecting reliably backpressured client");
            self.disconnect_babonet(id);
        }
        self.frame_id += 1;
        if self.round_state == GAME_PLAYING && self.config.game_time_limit > 0.0 {
            self.game_time_left = (self.game_time_left - dt).max(0.0);
        }
        self.round_time_left = (self.round_time_left - dt).max(0.0);
        self.tick_auto_balance(dt);

        let mut done = Vec::new();
        let mut chunks: Vec<(BaboNetId, Vec<u8>)> = Vec::new();
        for (i, tr) in self.map_transfers.iter_mut().enumerate() {
            if tr.offset >= tr.data.len() {
                done.push(i);
                continue;
            }
            let end = (tr.offset + 250).min(tr.data.len());
            chunks.push((tr.babonet_id, packet::map_chunk(&tr.data[tr.offset..end])));
            tr.offset = end;
            if tr.offset >= tr.data.len() {
                done.push(i);
            }
        }
        for (id, chunk) in chunks {
            self.send_to(id, chunk);
        }
        for i in done.into_iter().rev() {
            self.map_transfers.remove(i);
        }

        if self.round_state == GAME_PLAYING {
            tick_bots(self, dt);
            projectile::tick_projectiles(self, dt);
            if self.diagnostics_enabled && self.frame_id.rem_euclid(12) == 0 {
                projectile::log_diagnostic_snapshot(self);
            }
            shoot::tick_special_weapons(self, dt);
            shoot::tick_photon_beams(self);
            ctf::tick_ctf(self, dt);
            snd::tick_snd(self);
        }
        self.tick_idle_spectators(dt);
        // Transition packets provide immediate feedback; this compact snapshot
        // periodically repairs any client that joined or loaded a map across a
        // transition boundary. WebSocket ordering makes the latest snapshot
        // authoritative without requiring a second flag-state protocol.
        if self.frame_id.rem_euclid(60) == 0 {
            ctf::broadcast_flag_snapshot(self);
        }
        ctf::tick_match_lifecycle(self, dt);
        voting::tick_vote(self, dt);

        for slot in 0..MAX_PLAYER {
            if let Some(p) = self.players[slot].as_mut() {
                p.elapsed_since_last_shot = (p.elapsed_since_last_shot + dt).min(60.0);
                p.weapon_current_imp =
                    shoot::recover_weapon_spread(p.weapon_id, p.weapon_current_imp, dt);
                p.grenade_elapsed = (p.grenade_elapsed + dt).min(60.0);
                p.molotov_elapsed = (p.molotov_elapsed + dt).min(60.0);
                p.melee_elapsed = (p.melee_elapsed + dt).min(60.0);
                p.coord_elapsed = (p.coord_elapsed + dt).min(1.0);
                p.protection_remaining = (p.protection_remaining - dt).max(0.0);
                p.spawn_immunity_remaining = (p.spawn_immunity_remaining - dt).max(0.0);
                if !p.is_bot && p.status == PLAYER_STATUS_DEAD && p.time_to_spawn > 0.0 {
                    p.time_to_spawn = (p.time_to_spawn - dt).max(0.0);
                }
            }
        }

        let frame_id = self.frame_id;
        let timer = packet::synchronize_timer(frame_id, self.game_time_left, self.round_time_left);

        for slot in 0..MAX_PLAYER {
            let ping_to_send = {
                let Some(player) = self.players[slot].as_ref() else {
                    continue;
                };
                if player.is_bot {
                    continue;
                }
                if !player.handshake_done {
                    continue;
                }
                if !player.wait_for_pong
                    && player.ping_cycle_started.elapsed() >= Duration::from_secs(1)
                {
                    Some((player.babonet_id, slot as i8))
                } else {
                    None
                }
            };
            if let Some((bid, pid)) = ping_to_send {
                if let Some(p) = self.players[slot].as_mut() {
                    p.wait_for_pong = true;
                    p.ping_sent_at = Some(Instant::now());
                }
                self.send_to(bid, packet::ping(pid));
            }

            let disconnect_bid = {
                let Some(player) = self.players[slot].as_mut() else {
                    continue;
                };
                if !player.handshake_done {
                    continue;
                }
                let pong_wait = player.ping_sent_at.map(|sent| sent.elapsed());
                if player.wait_for_pong
                    && pong_wait.is_some_and(|wait| wait > Duration::from_secs(6))
                {
                    Some(player.babonet_id)
                } else {
                    None
                }
            };
            if let Some(bid) = disconnect_bid {
                self.disconnect_babonet(bid);
            }
        }

        for j in 0..MAX_PLAYER {
            let Some(pj) = self.players[j].as_ref() else {
                continue;
            };
            if pj.status != PLAYER_STATUS_ALIVE {
                continue;
            }
            let cf = build_coord_frame(j as i8, pj.babonet_id as i32, &pj.cf);
            for i in 0..MAX_PLAYER {
                let Some(pi) = self.players[i].as_ref() else {
                    continue;
                };
                if pi.handshake_done {
                    self.send_to(pi.babonet_id, cf.clone());
                }
            }
        }

        for i in 0..MAX_PLAYER {
            let Some(pi) = self.players[i].as_ref() else {
                continue;
            };
            if !pi.handshake_done {
                continue;
            }
            self.send_to(pi.babonet_id, timer.clone());
            for j in 0..MAX_PLAYER {
                let Some(pj) = self.players[j].as_ref() else {
                    continue;
                };
                self.send_to(pi.babonet_id, packet::player_ping(j as i8, pj.ping_ms));
            }
        }
    }

    fn idle_message(message: &str) -> Vec<u8> {
        let mut body = vec![0u8; 132];
        body[1] = CHAT_TEAM_ALL as u8;
        let bytes = message.as_bytes();
        let length = bytes.len().min(129);
        body[2..2 + length].copy_from_slice(&bytes[..length]);
        packet::encode_frame(ids::NET_SVCL_MSG, &body)
    }

    fn tick_idle_spectators(&mut self, dt: f32) {
        let mut warnings = Vec::new();
        let mut idle_slots = Vec::new();

        for slot in 0..MAX_PLAYER {
            let Some(player) = self.players[slot].as_mut() else {
                continue;
            };
            if !matches!(player.team_id, PLAYER_TEAM_BLUE | PLAYER_TEAM_RED)
                || player.status == PLAYER_STATUS_LOADING
            {
                player.idle_elapsed = 0.0;
                player.idle_position = player.cf.position;
                player.idle_warning = None;
                continue;
            }

            let dx = player.cf.position[0] - player.idle_position[0];
            let dy = player.cf.position[1] - player.idle_position[1];
            let moving = player.cf.vel[0] * player.cf.vel[0] + player.cf.vel[1] * player.cf.vel[1]
                >= IDLE_MOVEMENT_EPSILON_SQ;
            if player.status == PLAYER_STATUS_ALIVE
                && moving
                && dx * dx + dy * dy >= IDLE_MOVEMENT_EPSILON_SQ
            {
                player.idle_elapsed = 0.0;
                player.idle_position = player.cf.position;
                player.idle_warning = None;
                continue;
            }

            player.idle_elapsed += dt;
            let remaining = (IDLE_SPECTATE_SECONDS - player.idle_elapsed)
                .ceil()
                .max(0.0) as u8;
            if remaining == 0 {
                idle_slots.push(slot);
            } else if remaining <= IDLE_COUNTDOWN_SECONDS && player.idle_warning != Some(remaining)
            {
                player.idle_warning = Some(remaining);
                if !player.is_bot || player.external_bot {
                    warnings.push((
                        player.babonet_id,
                        format!("Move within {remaining}s or you will be moved to spectators."),
                    ));
                }
            }
        }

        for (babonet_id, message) in warnings {
            self.send_to(babonet_id, Self::idle_message(&message));
        }
        for slot in idle_slots {
            ctf::drop_carrier_flags(self, slot);
            let Some(player) = self.players[slot].as_mut() else {
                continue;
            };
            let name = player.name.clone();
            let babonet_id = player.babonet_id;
            let notify_client = !player.is_bot || player.external_bot;
            player.team_id = PLAYER_TEAM_SPECTATOR;
            player.status = PLAYER_STATUS_DEAD;
            player.life = 0.0;
            player.time_to_spawn = 0.0;
            player.idle_elapsed = 0.0;
            player.idle_warning = None;
            self.broadcast(packet::encode_frame(
                ids::NET_CLSV_SVCL_TEAM_REQUEST,
                &[slot as u8, PLAYER_TEAM_SPECTATOR as u8],
            ));
            shoot::broadcast_enum(self, slot);
            if notify_client {
                self.send_to(
                    babonet_id,
                    Self::idle_message("You were moved to spectators for inactivity."),
                );
            }
            tracing::info!(
                player = slot,
                player_name = name,
                "idle player moved to spectators"
            );
        }
    }

    fn rotate_to_next_map(&mut self) {
        if self.map_list.is_empty() {
            return;
        }
        let current = self
            .map_list
            .iter()
            .position(|name| name == &self.config.map_name)
            .unwrap_or(0);
        for offset in 1..=self.map_list.len() {
            let name = self.map_list[(current + offset) % self.map_list.len()].clone();
            if !map_allowed_for_game_type(self.config.game_type, &name) {
                continue;
            }
            let path = self.config.map_path(&name);
            let Ok(spawns) = load_map_spawns(&path) else {
                continue;
            };
            if !map_compatible_for_game_type(self.config.game_type, &spawns) {
                continue;
            }
            let Ok(grid) = MapGrid::load(&path) else {
                continue;
            };
            self.config.map_name = name;
            self.map_spawns = spawns;
            self.map_grid = grid;
            self.ctf = CtfState::new(ctf::map_flag_pod(&self.map_spawns));
            break;
        }
    }

    fn request_rate_limited(&mut self, babonet_id: BaboNetId, type_id: u16) -> bool {
        let limit = self.request_limits.entry(babonet_id).or_default();
        if limit.started.elapsed() >= Duration::from_secs(1) {
            *limit = RequestLimit::default();
        }
        let (counter, maximum) = match type_id {
            ids::NET_CLSV_SVCL_CHAT => (&mut limit.chat, 8),
            ids::NET_CLSV_SVCL_VOTE_REQUEST | ids::NET_CLSV_VOTE => (&mut limit.vote, 4),
            ids::NET_CLSV_MAP_REQUEST | ids::NET_CLSV_MAP_LIST_REQUEST => (&mut limit.map, 4),
            ids::NET_CLSV_SVCL_PLAYER_CHANGE_NAME | ids::NET_CLSV_SVCL_PLAYER_UPDATE_SKIN => {
                (&mut limit.profile, 10)
            }
            _ => return false,
        };
        *counter = counter.saturating_add(1);
        *counter > maximum
    }

    pub fn handle_frame(&mut self, babonet_id: BaboNetId, type_id: u16, payload: &[u8]) {
        let authenticated = self
            .find_slot_by_babonet(babonet_id)
            .and_then(|slot| self.players[slot].as_ref())
            .is_some_and(|player| player.handshake_done);
        if !authenticated
            && !matches!(
                type_id,
                ids::NET_CLSV_GAMEVERSION_ACCEPTED | ids::NET_CLSV_PONG
            )
        {
            tracing::warn!(babonet_id, type_id, "pre-auth application packet rejected");
            self.disconnect_babonet(babonet_id);
            return;
        }
        if authenticated && type_id == ids::NET_CLSV_GAMEVERSION_ACCEPTED {
            tracing::warn!(babonet_id, "repeated handshake ignored");
            return;
        }
        if authenticated && self.request_rate_limited(babonet_id, type_id) {
            tracing::warn!(babonet_id, type_id, "application request rate limited");
            return;
        }
        match type_id {
            ids::NET_CLSV_PONG => {
                let Some(pid) = wire_player_slot(payload) else {
                    return;
                };
                if let Some(p) = self.players.get_mut(pid).and_then(Option::as_mut) {
                    if p.babonet_id == babonet_id {
                        p.wait_for_pong = false;
                        if let Some(sent) = p.ping_sent_at.take() {
                            p.ping_ms = duration_ms_i16(sent.elapsed());
                        }
                        p.ping_cycle_started = Instant::now();
                    }
                }
            }
            ids::NET_CLSV_GAMEVERSION_ACCEPTED => {
                if payload.len() < 17 {
                    return;
                }
                let Some(pid) = wire_player_slot(payload) else {
                    return;
                };
                let pass = packet::read_str(payload, 1, 16);
                if !self.config.password.is_empty() && pass != self.config.password {
                    self.disconnect_babonet(babonet_id);
                    return;
                }
                let external_bot = self.players[pid]
                    .as_ref()
                    .is_some_and(|player| player.external_bot && player.babonet_id == babonet_id);
                let bot_handshake = if external_bot {
                    match parse_bot_handshake(payload) {
                        Ok(handshake) => Some(handshake),
                        Err(reason) => {
                            tracing::warn!(babonet_id, reason, "external bot handshake rejected");
                            self.disconnect_babonet(babonet_id);
                            return;
                        }
                    }
                } else {
                    None
                };
                let Some(p) = self.players.get_mut(pid).and_then(Option::as_mut) else {
                    return;
                };
                if p.babonet_id != babonet_id {
                    return;
                }
                p.wait_for_pong = false;
                p.ping_sent_at = None;
                p.ping_cycle_started = Instant::now();
                p.handshake_done = true;
                p.status = PLAYER_STATUS_DEAD;
                if let Some((identity, player_name)) = bot_handshake {
                    p.name = bot_display_name(&normalize_bv2_player_name(&player_name));
                    tracing::info!(
                        slot = pid,
                        babonet_id,
                        bot_name = %identity.name,
                        bot_version = %identity.version,
                        bot_author = %identity.author,
                        player_name = %p.name,
                        "external bot authenticated"
                    );
                    p.bot_identity = Some(identity);
                }

                self.send_to(
                    babonet_id,
                    packet::server_info(
                        &self.config.map_name,
                        self.config.game_type,
                        self.blue_score,
                        self.red_score,
                        self.blue_win,
                        self.red_win,
                    ),
                );
                self.send_to(babonet_id, packet::round_state(self.round_state, false));
                send_flag_enum_to(self, babonet_id);
                for frame in projectile::snapshot_frames(&self.projectiles) {
                    self.send_to(babonet_id, frame);
                }

                for i in 0..MAX_PLAYER {
                    if i == pid {
                        continue;
                    }
                    let Some(other) = self.players[i].as_ref() else {
                        continue;
                    };
                    let body = other.enum_state_bytes();
                    self.send_to(
                        babonet_id,
                        packet::encode_frame(ids::NET_SVCL_PLAYER_ENUM_STATE, &body),
                    );
                    if other.spawned_once && other.status == PLAYER_STATUS_ALIVE {
                        self.send_to(
                            babonet_id,
                            packet::encode_frame(
                                ids::NET_SVCL_PLAYER_SPAWN,
                                &player_spawn_bytes(i, other),
                            ),
                        );
                    }
                }
                if let Some(p) = self.players[pid].as_ref() {
                    let body = p.enum_state_bytes();
                    self.broadcast(packet::encode_frame(ids::NET_SVCL_PLAYER_ENUM_STATE, &body));
                }
            }
            ids::NET_CLSV_SVCL_PLAYER_INFO => {
                if payload.len() < 122 {
                    return;
                }
                let Some(pid) = wire_player_slot(payload) else {
                    return;
                };
                let name = packet::read_str(payload, 17, 32);
                if let Some(p) = self.players.get_mut(pid).and_then(Option::as_mut) {
                    if p.babonet_id != babonet_id {
                        return;
                    }
                    let name = sanitize_player_name(&name);
                    if !name.is_empty() {
                        p.name = if p.external_bot {
                            bot_display_name(&name)
                        } else {
                            name
                        };
                    }
                    let mut out = payload.to_vec();
                    write_ip16(&mut out[1..17], &p.ip);
                    write_name31(&mut out[17..49], &p.name);
                    let enum_body = p.enum_state_bytes();
                    self.broadcast(packet::encode_frame(ids::NET_CLSV_SVCL_PLAYER_INFO, &out));
                    self.broadcast(packet::encode_frame(
                        ids::NET_SVCL_PLAYER_ENUM_STATE,
                        &enum_body,
                    ));
                }
            }
            ids::NET_CLSV_SVCL_TEAM_REQUEST => {
                if payload.len() < 2 {
                    return;
                }
                let Some(pid) = wire_player_slot(payload) else {
                    return;
                };
                let team = payload[1] as i8;
                if !valid_player_team(team) {
                    tracing::warn!("team request rejected: player={pid} invalid team={team}");
                    return;
                }
                if !self.team_join_allowed(pid, team) {
                    let current = self.players[pid]
                        .as_ref()
                        .map(|player| player.team_id)
                        .unwrap_or(PLAYER_TEAM_SPECTATOR);
                    tracing::info!(
                        "team request rejected: player={pid} requested={team} would unbalance teams"
                    );
                    self.send_to(
                        babonet_id,
                        packet::encode_frame(
                            ids::NET_CLSV_SVCL_TEAM_REQUEST,
                            &[pid as u8, current as u8],
                        ),
                    );
                    return;
                }
                let mut changed_team = false;
                if let Some(p) = self.players.get_mut(pid).and_then(Option::as_mut) {
                    if p.babonet_id != babonet_id {
                        return;
                    }
                    // Game.cpp:942 — only an actual switch kills you and costs a respawn wait.
                    if p.team_id != team {
                        changed_team = true;
                        p.team_id = team;
                        p.status = PLAYER_STATUS_DEAD;
                        p.life = 0.0;
                        p.time_to_spawn = if team == PLAYER_TEAM_SPECTATOR {
                            0.0
                        } else {
                            SV_TIME_TO_SPAWN
                        };
                    }
                } else {
                    return;
                }
                if changed_team {
                    crate::game::ctf::drop_carrier_flags(self, pid);
                }
                self.broadcast(packet::encode_frame(
                    ids::NET_CLSV_SVCL_TEAM_REQUEST,
                    payload,
                ));
                if changed_team {
                    tracing::info!(player = pid, team, "player changed team");
                    shoot::broadcast_enum(self, pid);
                    reconcile_bot_roster(self);
                }
            }
            ids::NET_CLSV_SPAWN_REQUEST => {
                if self.round_state != GAME_PLAYING {
                    return;
                }
                if payload.len() < 19 {
                    return;
                }
                let Some(pid) = wire_player_slot(payload) else {
                    return;
                };
                let weapon = payload[1] as i8;
                let melee = payload[2] as i8;
                if !shoot::is_primary_weapon(weapon) || !shoot::is_melee_weapon(melee) {
                    tracing::warn!(
                        "spawn rejected: player={pid} invalid weapon={weapon} melee={melee}"
                    );
                    return;
                }
                let team_id = self
                    .players
                    .get(pid)
                    .and_then(|p| p.as_ref())
                    .filter(|p| p.babonet_id == babonet_id)
                    .map(|p| p.team_id)
                    .unwrap_or(PLAYER_TEAM_SPECTATOR);
                // PlayerUpdate.cpp:388 — only blue/red ever request a spawn.
                if team_id != PLAYER_TEAM_BLUE && team_id != PLAYER_TEAM_RED {
                    return;
                }
                let pos = if self.config.game_type == GAME_TYPE_SND {
                    snd::pick_spawn(self, pid)
                } else {
                    self.pick_spawn_for(team_id, Some(pid))
                };
                let Some(p) = self.players[pid].as_mut() else {
                    return;
                };
                if p.babonet_id != babonet_id {
                    return;
                }
                if p.time_to_spawn > 0.0 {
                    return;
                }

                p.weapon_id = weapon;
                p.melee_id = melee;
                p.skin.copy_from_slice(&payload[3..10]);
                p.red_decal.copy_from_slice(&payload[10..13]);
                p.green_decal.copy_from_slice(&payload[13..16]);
                p.blue_decal.copy_from_slice(&payload[16..19]);
                p.cf.position = pos;
                p.cf.vel = [0.0, 0.0, 0.0];
                p.status = PLAYER_STATUS_ALIVE;
                p.life = 1.0;
                p.time_to_spawn = 0.0;
                p.spawned_once = true;
                p.elapsed_since_last_shot = f32::INFINITY;
                p.flame_seconds_fired = 0.0;
                p.weapon_current_imp = shoot::weapon_start_spread(weapon);
                p.weapon_shot_sequence = 0;
                p.burst_shot_count = 0;
                p.grenades_left = 2;
                p.molotovs_left = 1;
                p.grenade_elapsed = f32::INFINITY;
                p.molotov_elapsed = f32::INFINITY;
                p.melee_elapsed = f32::INFINITY;
                p.protection_remaining = 0.0;
                p.spawn_immunity_remaining = SPAWN_IMMUNITY_TIME;
                p.nuke_remaining = 0.0;
                p.photon_frames = 0;
                p.coord_elapsed = 0.0;

                let body = player_spawn_bytes(pid, p);
                let enum_body = p.enum_state_bytes();
                self.broadcast(packet::encode_frame(ids::NET_SVCL_PLAYER_SPAWN, &body));
                self.broadcast(packet::encode_frame(
                    ids::NET_SVCL_PLAYER_ENUM_STATE,
                    &enum_body,
                ));
            }
            ids::NET_CLSV_SVCL_PLAYER_COORD_FRAME => {
                if self.round_state != GAME_PLAYING {
                    return;
                }
                if payload.len() < 28 {
                    return;
                }
                let Some(pid) = wire_player_slot(payload) else {
                    return;
                };
                let live_positions: [Option<[f32; 3]>; MAX_PLAYER] = std::array::from_fn(|slot| {
                    self.players[slot]
                        .as_ref()
                        .filter(|player| player.status == PLAYER_STATUS_ALIVE)
                        .map(|player| player.cf.position)
                });
                let Some(p) = self.players.get_mut(pid).and_then(Option::as_mut) else {
                    return;
                };
                if p.babonet_id != babonet_id || p.status != PLAYER_STATUS_ALIVE {
                    return;
                }

                let frame_id = packet::read_i32(payload, 4).unwrap_or(0);
                if !valid_movement_frame(p.cf.frame_id, frame_id) {
                    return;
                }
                let previous = p.cf.position;
                let previous_velocity = p.cf.vel;
                let mut requested = [
                    packet::read_i16(payload, 8).unwrap_or(0) as f32 / ids::WIRE_POS,
                    packet::read_i16(payload, 10).unwrap_or(0) as f32 / ids::WIRE_POS,
                    packet::read_i16(payload, 12).unwrap_or(0) as f32 / ids::WIRE_POS,
                ];
                let mut velocity = [
                    payload.get(14).copied().unwrap_or(0) as i8 as f32 / ids::WIRE_VEL,
                    payload.get(15).copied().unwrap_or(0) as i8 as f32 / ids::WIRE_VEL,
                    payload.get(16).copied().unwrap_or(0) as i8 as f32 / ids::WIRE_VEL,
                ];
                bound_client_movement(
                    &self.map_grid,
                    previous,
                    previous_velocity,
                    &mut requested,
                    &mut velocity,
                    p.coord_elapsed,
                );
                perform_collision_xy(
                    &self.map_grid,
                    previous,
                    &mut requested,
                    &mut velocity,
                    0.25,
                );
                clip_player_motion_xy(
                    pid,
                    &live_positions,
                    previous,
                    &mut requested,
                    &mut velocity,
                );

                p.cf.frame_id = frame_id;
                p.cf.position = requested;
                p.cf.vel = velocity;
                p.cf.mouse[0] = packet::read_i16(payload, 18).unwrap_or(0) as f32 / ids::WIRE_POS;
                p.cf.mouse[1] = packet::read_i16(payload, 20).unwrap_or(0) as f32 / ids::WIRE_POS;
                p.cf.mouse[2] = packet::read_i16(payload, 22).unwrap_or(0) as f32 / ids::WIRE_POS;
                p.coord_elapsed = 0.0;

                let relay = build_coord_frame(pid as i8, p.babonet_id as i32, &p.cf);
                let fell_to_death = p.cf.position[2] <= 0.0;
                let echo_authoritative = p.external_bot;
                for other in self.players.iter().flatten() {
                    if other.babonet_id != babonet_id {
                        self.send_to(other.babonet_id, relay.clone());
                    }
                }
                // Browser/native clients predict and reconcile their own movement.
                // Headless external bots need the collision-corrected position to
                // measure waypoint progress and replan without dead-reckoning drift.
                if echo_authoritative {
                    self.send_to(babonet_id, relay);
                }
                if fell_to_death {
                    shoot::apply_world_damage(self, pid, 13, 2.0);
                }
            }
            ids::NET_CLSV_PICKUP_REQUEST => {
                if payload.len() < 10 {
                    return;
                }
                let Some(pid) = wire_player_slot(payload) else {
                    return;
                };
                let item_type = payload[1] as i8;
                if !matches!(item_type, 1..=3) {
                    return;
                }
                let reported = [
                    packet::read_i16(payload, 2).unwrap_or(0) as f32 / ids::WIRE_POS,
                    packet::read_i16(payload, 4).unwrap_or(0) as f32 / ids::WIRE_POS,
                ];
                let Some(player) = self.players.get(pid).and_then(Option::as_ref) else {
                    return;
                };
                if player.babonet_id != babonet_id || player.status != PLAYER_STATUS_ALIVE {
                    return;
                }
                let player_distance = (player.cf.position[0] - reported[0])
                    .hypot(player.cf.position[1] - reported[1]);
                if player_distance > 0.55 {
                    return;
                }
                let unique_id = packet::read_i32(payload, 6).unwrap_or(0);
                let Some(index) = self.projectiles.iter().position(|pickup| {
                    pickup.unique_id == unique_id
                        && ((item_type == 1 && pickup.kind == 4)
                            || (item_type == 2 && pickup.kind == 5)
                            || (item_type == 3 && pickup.kind == 6))
                        && (pickup.position[0] - player.cf.position[0])
                            .hypot(pickup.position[1] - player.cf.position[1])
                            <= 0.5
                }) else {
                    return;
                };
                let pickup = self.projectiles.remove(index);
                let mut dropped_weapon = None;
                if let Some(player) = self.players[pid].as_mut() {
                    if item_type == 1 {
                        player.life = (player.life + 0.5).min(1.0);
                    } else if item_type == 2 {
                        dropped_weapon =
                            Some((player.weapon_id, player.cf.position, player.cf.vel));
                        player.weapon_id = pickup.weapon_id;
                        player.elapsed_since_last_shot = f32::INFINITY;
                        player.weapon_current_imp = shoot::weapon_start_spread(pickup.weapon_id);
                    } else {
                        player.grenades_left = (player.grenades_left + 1).min(3);
                    }
                }
                if let Some((weapon_id, position, velocity)) = dropped_weapon {
                    projectile::spawn_weapon_pickup(self, pid, weapon_id, position, velocity);
                }
                self.broadcast(packet::encode_frame(
                    ids::NET_SVCL_PICKUP_ITEM,
                    &[pid as u8, item_type as u8, pickup.weapon_id as u8],
                ));
                self.broadcast(packet::encode_frame(
                    ids::NET_SVCL_DELETE_PROJECTILE,
                    &unique_id.to_le_bytes(),
                ));
                shoot::broadcast_enum(self, pid);
            }
            ids::NET_CLSV_SVCL_CHAT => {
                if payload.is_empty() {
                    return;
                }
                let requested_team = payload[0] as i8;
                let Some(sender) = self
                    .players
                    .iter()
                    .flatten()
                    .find(|player| player.babonet_id == babonet_id && player.handshake_done)
                else {
                    return;
                };
                let team = if requested_team == CHAT_TEAM_ALL {
                    CHAT_TEAM_ALL
                } else if matches!(sender.team_id, PLAYER_TEAM_BLUE | PLAYER_TEAM_RED)
                    && requested_team == sender.team_id
                {
                    sender.team_id
                } else {
                    // Spectators may use global chat, but have no private team channel.
                    return;
                };
                let raw_message = packet::read_str(payload, 1, 130);
                let message = sanitize_chat_text(&raw_message);
                if message.is_empty() {
                    return;
                }
                let player_id = sender.slot;
                // The authenticated profile name was already bounded and sanitized by
                // sanitize_player_name. Preserve its native colour control bytes and
                // legacy Alt-code glyphs when prefixing chat, just as Client.cpp does.
                let player_name = sender.name.clone();
                let visible = format!("{player_name}: {message}");
                let mut outgoing = vec![0u8; 131];
                outgoing[0] = team as u8;
                let bytes = visible.as_bytes();
                let length = bytes.len().min(129);
                outgoing[1..1 + length].copy_from_slice(&bytes[..length]);
                let frame = packet::encode_frame(ids::NET_CLSV_SVCL_CHAT, &outgoing);
                if self.recent_chat.len() == RECENT_CHAT_LIMIT {
                    self.recent_chat.pop_front();
                }
                self.recent_chat.push_back(RecentChat {
                    player_id,
                    player_name,
                    message,
                    at_seconds: self.uptime_secs(),
                });
                if team == CHAT_TEAM_ALL {
                    self.broadcast(frame);
                } else {
                    for player in self.players.iter().flatten().filter(|p| p.team_id == team) {
                        self.send_to(player.babonet_id, frame.clone());
                    }
                }
            }
            ids::NET_CLSV_SVCL_PLAYER_CHANGE_NAME => {
                if payload.len() != 33 {
                    return;
                }
                let Some(pid) = wire_player_slot(payload) else {
                    return;
                };
                let name = sanitize_player_name(&packet::read_str(payload, 1, 32));
                if name.is_empty() {
                    return;
                }
                let Some(player) = self.players.get_mut(pid).and_then(Option::as_mut) else {
                    return;
                };
                if player.babonet_id != babonet_id {
                    tracing::warn!(
                        babonet_id,
                        claimed_player = pid,
                        "spoofed profile name rejected"
                    );
                    return;
                }
                let name = if player.external_bot {
                    bot_display_name(&name)
                } else {
                    name
                };
                player.name = name.clone();
                let mut outgoing = vec![0u8; 33];
                outgoing[0] = pid as u8;
                write_name31(&mut outgoing[1..33], &name);
                self.broadcast(packet::encode_frame(type_id, &outgoing));
            }
            ids::NET_CLSV_SVCL_PLAYER_UPDATE_SKIN => {
                if payload.len() != 17 {
                    return;
                }
                let Some(pid) = wire_player_slot(payload) else {
                    return;
                };
                let skin_name = normalize_skin_name(&packet::read_str(payload, 1, 7));
                let Some(player) = self.players.get_mut(pid).and_then(Option::as_mut) else {
                    return;
                };
                if player.babonet_id != babonet_id {
                    tracing::warn!(
                        babonet_id,
                        claimed_player = pid,
                        "spoofed profile skin rejected"
                    );
                    return;
                }
                player.skin = skin_name;
                player.red_decal.copy_from_slice(&payload[8..11]);
                player.green_decal.copy_from_slice(&payload[11..14]);
                player.blue_decal.copy_from_slice(&payload[14..17]);
                let mut outgoing = payload.to_vec();
                outgoing[0] = pid as u8;
                outgoing[1..8].copy_from_slice(&skin_name);
                self.broadcast(packet::encode_frame(type_id, &outgoing));
            }
            ids::NET_CLSV_SVCL_VOTE_REQUEST => {
                voting::handle_vote_request(self, babonet_id, payload)
            }
            ids::NET_CLSV_VOTE => voting::handle_vote(self, babonet_id, payload),
            ids::NET_CLSV_MAP_LIST_REQUEST => {
                for name in &self.map_list {
                    self.send_to(babonet_id, packet::map_list(name));
                }
            }
            ids::NET_CLSV_MAP_REQUEST => {
                if payload.len() < 16 {
                    return;
                }
                let name = packet::read_str(payload, 0, 16);
                if !is_advertised_map(&self.map_list, &name) {
                    tracing::warn!("map request rejected: unadvertised name={name:?}");
                    return;
                }
                let path = self.config.map_path(&name);
                if let Ok(data) = std::fs::read(&path) {
                    const MAX_MAP_BYTES: usize = 16 * 1024 * 1024;
                    const MAX_TOTAL_TRANSFER_BYTES: usize = 64 * 1024 * 1024;
                    if data.len() > MAX_MAP_BYTES {
                        tracing::warn!(
                            name,
                            bytes = data.len(),
                            "map request exceeds transfer limit"
                        );
                        return;
                    }
                    self.map_transfers
                        .retain(|transfer| transfer.babonet_id != babonet_id);
                    let buffered: usize = self
                        .map_transfers
                        .iter()
                        .map(|transfer| transfer.data.len().saturating_sub(transfer.offset))
                        .sum();
                    if buffered.saturating_add(data.len()) > MAX_TOTAL_TRANSFER_BYTES {
                        tracing::warn!(
                            bytes = data.len(),
                            buffered,
                            "total map transfer limit reached"
                        );
                        return;
                    }
                    self.map_transfers.push(MapTransfer {
                        babonet_id,
                        data,
                        offset: 0,
                    });
                }
            }
            ids::NET_CLSV_PLAYER_SHOOT => {
                let Some(pid) = wire_player_slot(payload) else {
                    return;
                };
                if self
                    .players
                    .get(pid)
                    .and_then(|p| p.as_ref())
                    .map(|p| p.babonet_id == babonet_id)
                    .unwrap_or(false)
                {
                    shoot::handle_player_shoot(self, pid, payload);
                }
            }
            ids::NET_CLSV_SVCL_PLAYER_PROJECTILE => {
                let Some(pid) = wire_player_slot(payload) else {
                    return;
                };
                if self
                    .players
                    .get(pid)
                    .and_then(|p| p.as_ref())
                    .map(|p| p.babonet_id == babonet_id)
                    .unwrap_or(false)
                {
                    projectile::handle_spawn_intent(self, pid, payload);
                } else {
                    tracing::debug!("projectile ignored: pid={pid} babonet mismatch");
                }
            }
            ids::NET_CLSV_SVCL_PLAYER_SHOOT_MELEE => {
                if payload.is_empty() {
                    return;
                }
                let Some(pid) = wire_player_slot(payload) else {
                    return;
                };
                if self
                    .players
                    .get(pid)
                    .and_then(|p| p.as_ref())
                    .map(|p| p.babonet_id == babonet_id)
                    .unwrap_or(false)
                {
                    shoot::handle_melee(self, pid, payload);
                }
            }
            ids::NET_CLSV_REPORT_EXPLOSION => {
                // Deprecated browser extension. Explosions are derived from
                // server-owned projectile state and client reports are ignored.
                tracing::debug!("ignored client explosion report from babonet={babonet_id}");
            }
            ids::NET_CLSV_REPORT_BURN => {
                tracing::debug!("ignored client burn report from babonet={babonet_id}");
            }
            _ => tracing::debug!("unhandled packet type {type_id}"),
        }
    }

    pub fn uptime_secs(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

pub(crate) fn player_spawn_bytes(slot: usize, p: &Player) -> Vec<u8> {
    let pos = p.cf.position;
    let mut body = vec![0u8; 26];
    body[0] = slot as u8;
    body[1] = p.weapon_id as u8;
    body[2] = p.melee_id as u8;
    body[4..6].copy_from_slice(&((pos[0] * 10.0) as i16).to_le_bytes());
    body[6..8].copy_from_slice(&((pos[1] * 10.0) as i16).to_le_bytes());
    body[8..10].copy_from_slice(&((pos[2] * 10.0) as i16).to_le_bytes());
    body[10..17].copy_from_slice(&p.skin);
    body[17..20].copy_from_slice(&p.red_decal);
    body[20..23].copy_from_slice(&p.green_decal);
    body[23..26].copy_from_slice(&p.blue_decal);
    body
}

fn build_coord_frame(player_id: i8, babonet_id: i32, cf: &CoordFrame) -> Vec<u8> {
    let mut p = vec![0u8; 28];
    p[0] = player_id as u8;
    p[4..8].copy_from_slice(&cf.frame_id.to_le_bytes());
    p[8..10].copy_from_slice(&((cf.position[0] * ids::WIRE_POS) as i16).to_le_bytes());
    p[10..12].copy_from_slice(&((cf.position[1] * ids::WIRE_POS) as i16).to_le_bytes());
    p[12..14].copy_from_slice(&((cf.position[2] * ids::WIRE_POS) as i16).to_le_bytes());
    p[14] = (cf.vel[0] * ids::WIRE_VEL) as i8 as u8;
    p[15] = (cf.vel[1] * ids::WIRE_VEL) as i8 as u8;
    p[16] = (cf.vel[2] * ids::WIRE_VEL) as i8 as u8;
    p[18..20].copy_from_slice(&((cf.mouse[0] * ids::WIRE_POS) as i16).to_le_bytes());
    p[20..22].copy_from_slice(&((cf.mouse[1] * ids::WIRE_POS) as i16).to_le_bytes());
    p[22..24].copy_from_slice(&((cf.mouse[2] * ids::WIRE_POS) as i16).to_le_bytes());
    p[24..28].copy_from_slice(&babonet_id.to_le_bytes());
    packet::encode_frame(ids::NET_CLSV_SVCL_PLAYER_COORD_FRAME, &p)
}

/// Correct client movement to the envelope produced by PlayerUpdate.cpp.
fn bound_client_movement(
    map: &MapGrid,
    previous: [f32; 3],
    previous_velocity: [f32; 3],
    requested: &mut [f32; 3],
    velocity: &mut [f32; 3],
    elapsed: f32,
) {
    const MAX_PLAYER_SPEED: f32 = 3.25;
    let allowed = MAX_PLAYER_SPEED * elapsed.max(TICK_MS as f32 / 1000.0) + 0.12;
    let dx = requested[0] - previous[0];
    let dy = requested[1] - previous[1];
    let distance = dx.hypot(dy);
    if distance > allowed {
        let scale = allowed / distance;
        requested[0] = previous[0] + dx * scale;
        requested[1] = previous[1] + dy * scale;
    }
    let planar_speed = velocity[0].hypot(velocity[1]);
    if planar_speed > MAX_PLAYER_SPEED {
        let scale = MAX_PLAYER_SPEED / planar_speed;
        velocity[0] *= scale;
        velocity[1] *= scale;
    }
    if map.contains_xy(requested[0], requested[1]) {
        requested[2] = 0.25;
        velocity[2] = 0.0;
    } else {
        let dt = elapsed.max(TICK_MS as f32 / 1000.0);
        requested[2] = previous[2] + previous_velocity[2] * dt;
        let old_speed = previous_velocity[0]
            .hypot(previous_velocity[1])
            .hypot(previous_velocity[2]);
        let friction_scale = if old_speed > 0.0 {
            (old_speed - 4.0 * dt).max(0.0) / old_speed
        } else {
            0.0
        };
        velocity[2] = previous_velocity[2] * friction_scale - 9.8 * dt;
        let speed = velocity[0].hypot(velocity[1]).hypot(velocity[2]);
        if speed > MAX_PLAYER_SPEED {
            let scale = MAX_PLAYER_SPEED / speed;
            velocity[0] *= scale;
            velocity[1] *= scale;
            velocity[2] *= scale;
        }
    }
}

/// Swept circle-vs-circle authority for player bodies. This prevents a large
/// but otherwise speed-valid movement sample from tunneling through a Babo.
pub(crate) fn clip_player_motion_xy(
    slot: usize,
    live_positions: &[Option<[f32; 3]>],
    previous: [f32; 3],
    requested: &mut [f32; 3],
    velocity: &mut [f32; 3],
) {
    const BODY_DIAMETER: f32 = 0.50;
    const CONTACT_GAP: f32 = 0.005;
    let movement = [requested[0] - previous[0], requested[1] - previous[1]];
    let movement_sq = movement[0] * movement[0] + movement[1] * movement[1];
    let mut earliest = 1.0f32;

    for (other_slot, other) in live_positions.iter().enumerate() {
        if other_slot == slot {
            continue;
        }
        let Some(other) = other else { continue };
        let offset = [previous[0] - other[0], previous[1] - other[1]];
        let start_distance = offset[0].hypot(offset[1]);
        if start_distance < BODY_DIAMETER {
            let (nx, ny) = if start_distance > 1e-5 {
                (offset[0] / start_distance, offset[1] / start_distance)
            } else if slot < other_slot {
                (-1.0, 0.0)
            } else {
                (1.0, 0.0)
            };
            requested[0] = other[0] + nx * (BODY_DIAMETER + CONTACT_GAP);
            requested[1] = other[1] + ny * (BODY_DIAMETER + CONTACT_GAP);
            velocity[0] = 0.0;
            velocity[1] = 0.0;
            return;
        }
        if movement_sq < 1e-8 {
            continue;
        }
        let b = 2.0 * (offset[0] * movement[0] + offset[1] * movement[1]);
        let c = offset[0] * offset[0] + offset[1] * offset[1] - BODY_DIAMETER * BODY_DIAMETER;
        let discriminant = b * b - 4.0 * movement_sq * c;
        if discriminant < 0.0 {
            continue;
        }
        let hit = (-b - discriminant.sqrt()) / (2.0 * movement_sq);
        if (0.0..=earliest).contains(&hit) {
            earliest = hit;
        }
    }

    if earliest < 1.0 {
        let movement_length = movement_sq.sqrt();
        let safe = (earliest - CONTACT_GAP / movement_length.max(1e-5)).max(0.0);
        requested[0] = previous[0] + movement[0] * safe;
        requested[1] = previous[1] + movement[1] * safe;
        velocity[0] = 0.0;
        velocity[1] = 0.0;
    }
}

pub(crate) fn write_name31(buf: &mut [u8], name: &str) {
    let safe = truncate_utf8_bytes(name, 31.min(buf.len().saturating_sub(1)));
    buf[..safe.len()].copy_from_slice(safe.as_bytes());
}

fn write_ip16(buf: &mut [u8], ip: &str) {
    let n = ip.len().min(15);
    buf[..n].copy_from_slice(&ip.as_bytes()[..n]);
}

fn sanitize_chat_text(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

fn sanitize_player_name(text: &str) -> String {
    // BV2 names intentionally use control characters 1..=9 as inline colour
    // markers and code points 128..=159 for the game's legacy glyph page.
    // Preserve those protocol values while rejecting unrelated controls.
    let clean = text
        .chars()
        .filter(|ch| {
            let code = *ch as u32;
            !ch.is_control() || (1..=9).contains(&code) || (128..=159).contains(&code)
        })
        .collect::<String>()
        .trim()
        .to_string();
    truncate_utf8_bytes(&clean, 31).to_string()
}

fn parse_bot_handshake(payload: &[u8]) -> Result<(BotIdentity, String), &'static str> {
    if payload.get(17..21) != Some(b"BOT1") {
        return Err("missing BOT1 identity block");
    }
    let mut offset = 21usize;
    let mut field = |maximum: usize, allow_name_codes: bool| -> Result<String, &'static str> {
        let length = *payload.get(offset).ok_or("truncated bot identity")? as usize;
        offset += 1;
        if length == 0 || length > maximum {
            return Err("invalid bot identity field length");
        }
        let bytes = payload
            .get(offset..offset + length)
            .ok_or("truncated bot identity field")?;
        offset += length;
        let value = std::str::from_utf8(bytes).map_err(|_| "bot identity is not UTF-8")?;
        if !allow_name_codes && value.chars().any(char::is_control) {
            return Err("bot identity contains control characters");
        }
        Ok(value.trim().to_string())
    };
    let name = field(64, false)?;
    let version = field(32, false)?;
    let author = field(64, false)?;
    let player_name = field(96, true)?;
    if name.is_empty() || version.is_empty() || author.is_empty() || player_name.is_empty() {
        return Err("bot handshake fields cannot be blank");
    }
    Ok((
        BotIdentity {
            name,
            version,
            author,
        },
        player_name,
    ))
}

fn normalize_bv2_player_name(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '^' {
            if let Some(next) = chars.peek().copied() {
                if ('1'..='9').contains(&next) {
                    out.push(char::from_u32(next.to_digit(10).unwrap()).unwrap());
                    chars.next();
                    continue;
                }
            }
        }
        let mapped = match ch as u32 {
            199 => 128,
            252 => 129,
            233 => 130,
            226 => 131,
            228 => 132,
            224 => 133,
            229 => 134,
            231 => 135,
            234 => 136,
            235 => 137,
            232 => 138,
            239 => 139,
            238 => 140,
            236 => 141,
            196 => 142,
            197 => 143,
            201 => 144,
            230 => 145,
            198 => 146,
            244 => 147,
            246 => 148,
            242 => 149,
            251 => 150,
            249 => 151,
            255 => 152,
            214 => 153,
            220 => 154,
            248 => 155,
            163 => 156,
            216 => 157,
            215 => 158,
            170 => 159,
            code => code,
        };
        if let Some(mapped) = char::from_u32(mapped) {
            out.push(mapped);
        }
    }
    sanitize_player_name(&out)
}

fn bot_display_name(text: &str) -> String {
    const SUFFIX: &str = " [bot]";
    let clean = sanitize_player_name(text);
    let base = clean.strip_suffix(SUFFIX).unwrap_or(&clean).trim_end();
    let base = truncate_utf8_bytes(base, 31 - SUFFIX.len());
    format!("{base}{SUFFIX}")
}

fn truncate_utf8_bytes(text: &str, maximum: usize) -> &str {
    let mut end = text.len().min(maximum);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn valid_movement_frame(previous: i32, next: i32) -> bool {
    if previous == 0 {
        return next != 0;
    }
    let delta = next.wrapping_sub(previous) as u32;
    (1..=600).contains(&delta)
}

fn normalize_skin_name(text: &str) -> [u8; 7] {
    let valid = text.strip_prefix("skin").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.len() <= 2 && suffix.bytes().all(|b| b.is_ascii_digit())
    });
    let selected = if valid { text } else { "skin10" };
    let mut out = [0u8; 7];
    out[..selected.len()].copy_from_slice(selected.as_bytes());
    out
}

fn coalescible_packet(frame: &[u8]) -> bool {
    if frame.len() < 4 {
        return false;
    }
    let type_id = u16::from_le_bytes([frame[2], frame[3]]);
    matches!(
        type_id,
        ids::NET_CLSV_SVCL_PLAYER_COORD_FRAME
            | ids::NET_SVCL_SYNCHRONIZE_TIMER
            | ids::NET_SVCL_PLAYER_PING
            | ids::NET_SVCL_PROJECTILE_COORD_FRAME
    )
}

#[cfg(test)]
mod movement_authority_tests {
    use super::{
        bot_display_name, bound_client_movement, build_coord_frame, clip_player_motion_xy,
        duration_ms_i16, is_advertised_map, map_allowed_for_game_type, normalize_bv2_player_name,
        parse_bot_handshake, player_spawn_bytes, sanitize_player_name, valid_movement_frame,
        valid_player_team, wire_player_slot, write_name31, CoordFrame, GameServer, Player,
        GAME_TYPE_CTF, GAME_TYPE_SND, MAX_PLAYER, PLAYER_STATUS_ALIVE, PLAYER_STATUS_DEAD,
        PLAYER_TEAM_BLUE, PLAYER_TEAM_RED, PLAYER_TEAM_SPECTATOR,
    };
    use crate::config::ServerConfig;
    use crate::map::MapGrid;
    use std::time::{Duration, Instant};

    fn map() -> MapGrid {
        MapGrid::fixture(10, &[])
    }

    #[test]
    fn malformed_wire_player_ids_are_rejected_before_array_access() {
        assert_eq!(wire_player_slot(&[]), None);
        assert_eq!(wire_player_slot(&[31]), Some(31));
        assert_eq!(wire_player_slot(&[32]), None);
        assert_eq!(wire_player_slot(&[255]), None);
    }

    #[test]
    fn player_names_truncate_on_utf8_boundaries_without_panicking() {
        let crossing = format!("{}é", "a".repeat(30));
        assert_eq!(sanitize_player_name(&crossing), "a".repeat(30));
        assert_eq!(sanitize_player_name("  Al\n ice  "), "Al ice");
        let mut wire = [0u8; 32];
        write_name31(&mut wire, &crossing);
        assert_eq!(&wire[..30], "a".repeat(30).as_bytes());
        assert_eq!(wire[30], 0);
    }

    #[test]
    fn player_names_preserve_colour_markers_and_legacy_alt_glyphs() {
        let name = "\u{3}Red \u{9b} King";
        assert_eq!(sanitize_player_name(name), name);
        assert_eq!(sanitize_player_name("A\nB\u{b}C"), "ABC");
    }

    #[test]
    fn external_bot_names_get_one_server_owned_suffix_within_wire_limit() {
        assert_eq!(bot_display_name("Ada"), "Ada [bot]");
        assert_eq!(bot_display_name("Ada [bot]"), "Ada [bot]");
        let name = bot_display_name("éééééééééééééééééééé");
        assert!(name.ends_with(" [bot]"));
        assert!(name.len() <= 31);
        assert!(name.is_char_boundary(name.len()));
    }

    #[test]
    fn external_bot_identity_is_distinct_from_server_control() {
        let mut player = Player::new(0, 42, "test".into());
        player.external_bot = true;
        player.name = bot_display_name("Player0");
        assert!(player.external_bot);
        assert!(!player.is_bot);
        assert_eq!(player.name, "Player0 [bot]");
    }

    #[test]
    fn bot_handshake_has_independent_colored_player_name() {
        let mut payload = vec![0u8; 21];
        payload[17..21].copy_from_slice(b"BOT1");
        for value in ["ArenaMind", "2.4.1", "Example Lab", "^4Rød ^1Runner"] {
            payload.push(value.len() as u8);
            payload.extend_from_slice(value.as_bytes());
        }
        let (identity, player_name) = parse_bot_handshake(&payload).unwrap();
        assert_eq!(identity.name, "ArenaMind");
        assert_eq!(identity.version, "2.4.1");
        assert_eq!(identity.author, "Example Lab");
        let display = bot_display_name(&normalize_bv2_player_name(&player_name));
        assert!(display.starts_with('\u{4}'));
        assert!(display.contains('\u{1}'));
        assert!(display.ends_with(" [bot]"));
        assert_ne!(identity.name, player_name);
    }

    #[test]
    fn movement_frame_jumps_are_bounded_and_wrap_safely() {
        assert!(valid_movement_frame(100, 101));
        assert!(valid_movement_frame(i32::MAX, i32::MIN));
        assert!(!valid_movement_frame(100, 100));
        assert!(!valid_movement_frame(100, 10_000));
        assert!(
            valid_movement_frame(100, 101),
            "a rejected jump must not wedge later input"
        );
    }

    #[test]
    fn player_spawn_and_coordinate_vectors_follow_packed_contract() {
        let mut player = Player::new(3, 0x11223344, "test".into());
        player.weapon_id = 2;
        player.melee_id = 8;
        player.cf.position = [1.2, -2.3, 0.4];
        player.skin = *b"skin10\0";
        player.red_decal = [1, 2, 3];
        player.green_decal = [4, 5, 6];
        player.blue_decal = [7, 8, 9];
        let spawn = player_spawn_bytes(3, &player);
        assert_eq!(spawn.len(), 26);
        assert_eq!(&spawn[0..4], &[3, 2, 8, 0]);
        assert_eq!(&spawn[4..10], &[12, 0, 233, 255, 4, 0]);
        assert_eq!(&spawn[10..17], b"skin10\0");
        assert_eq!(&spawn[17..26], &[1, 2, 3, 4, 5, 6, 7, 8, 9]);

        let cf = CoordFrame {
            frame_id: 0x01020304,
            position: [1.0, -2.0, 0.25],
            vel: [1.0, -1.0, 0.0],
            mouse: [3.0, 4.0, 0.25],
        };
        let encoded = build_coord_frame(3, 0x11223344, &cf);
        let frame = crate::net::packet::decode_frame(&encoded).unwrap().unwrap();
        assert_eq!(frame.payload.len(), 28);
        assert_eq!(&frame.payload[0..4], &[3, 0, 0, 0]);
        assert_eq!(&frame.payload[4..8], &[4, 3, 2, 1]);
        assert_eq!(&frame.payload[24..28], &[0x44, 0x33, 0x22, 0x11]);
    }

    #[test]
    fn profile_updates_are_authenticated_bounded_and_state_preserving() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        let mut first = Player::new(0, 7, "old".into());
        first.name = "old".into();
        first.handshake_done = true;
        first.life = 42.0;
        first.team_id = PLAYER_TEAM_BLUE;
        first.weapon_id = 5;
        let mut second = Player::new(1, 8, "other".into());
        second.name = "other".into();
        second.handshake_done = true;
        game.players[0] = Some(first);
        game.players[1] = Some(second);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
        game.register_session(8, tx);

        let mut spoof = [0u8; 33];
        spoof[0] = 0;
        spoof[1..6].copy_from_slice(b"owned");
        game.handle_frame(8, crate::net::ids::NET_CLSV_SVCL_PLAYER_CHANGE_NAME, &spoof);
        assert_eq!(game.players[0].as_ref().unwrap().name, "old");

        let mut name = [0u8; 33];
        name[0] = 0;
        name[1..16].copy_from_slice(b"  Al\n ice Hero ");
        game.handle_frame(7, crate::net::ids::NET_CLSV_SVCL_PLAYER_CHANGE_NAME, &name);
        let player = game.players[0].as_ref().unwrap();
        assert_eq!(player.name, "Al ice Hero");
        assert_eq!(
            (player.life, player.team_id, player.weapon_id),
            (42.0, PLAYER_TEAM_BLUE, 5)
        );

        let mut skin = [0u8; 17];
        skin[0] = 0;
        skin[1..7].copy_from_slice(b"skin23");
        skin[8..17].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        game.handle_frame(7, crate::net::ids::NET_CLSV_SVCL_PLAYER_UPDATE_SKIN, &skin);
        let player = game.players[0].as_ref().unwrap();
        assert_eq!(&player.skin, b"skin23\0");
        assert_eq!(player.red_decal, [1, 2, 3]);
        assert_eq!(
            (player.life, player.team_id, player.weapon_id),
            (42.0, PLAYER_TEAM_BLUE, 5)
        );
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());

        game.disconnect_babonet(7);
        let mut replacement = Player::new(0, 9, "replacement".into());
        replacement.name = "replacement".into();
        replacement.handshake_done = true;
        game.players[0] = Some(replacement);
        game.handle_frame(7, crate::net::ids::NET_CLSV_SVCL_PLAYER_CHANGE_NAME, &name);
        assert_eq!(game.players[0].as_ref().unwrap().name, "replacement");
    }

    #[test]
    fn map_download_only_accepts_advertised_basenames() {
        let maps = vec!["CTF-Alert".to_string()];
        assert!(is_advertised_map(&maps, "CTF-Alert"));
        assert!(!is_advertised_map(&maps, "../bv2.cfg"));
        assert!(!is_advertised_map(&maps, "/etc/passwd"));
        assert!(!is_advertised_map(&maps, "CTF-Alert.bvm"));
    }

    #[test]
    fn gameplay_before_handshake_releases_the_reserved_slot() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: "secret".into(),
            game_type: GAME_TYPE_CTF,
            max_players: 1,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        assert_eq!(game.connect_client(7, "test".into()), Some(0));
        game.handle_frame(7, crate::net::ids::NET_CLSV_SVCL_TEAM_REQUEST, &[0, 0]);
        assert!(game.players[0].is_none());
        assert_eq!(game.connect_client(8, "test".into()), Some(0));
    }

    #[test]
    fn repeated_handshake_cannot_reset_a_live_player_or_replay_snapshots() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 1,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        let mut player = Player::new(0, 7, "test".into());
        player.handshake_done = true;
        player.status = super::PLAYER_STATUS_ALIVE;
        player.spawned_once = true;
        game.players[0] = Some(player);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        game.register_session(7, tx);
        let mut handshake = [0u8; 17];
        handshake[0] = 0;
        for _ in 0..100 {
            game.handle_frame(
                7,
                crate::net::ids::NET_CLSV_GAMEVERSION_ACCEPTED,
                &handshake,
            );
        }
        let player = game.players[0].as_ref().unwrap();
        assert_eq!(player.status, super::PLAYER_STATUS_ALIVE);
        assert!(player.handshake_done);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn bounded_session_queue_sheds_a_non_reading_clients_packets() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 1,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
        game.register_session(7, tx);
        let snapshot =
            crate::net::packet::encode_frame(crate::net::ids::NET_SVCL_SYNCHRONIZE_TIMER, &[0; 12]);
        for _ in 0..10_000 {
            game.send_to(7, snapshot.clone());
        }
        assert_eq!(rx.try_recv().unwrap(), snapshot);
        assert_eq!(rx.try_recv().unwrap(), snapshot);
        assert!(rx.try_recv().is_err());
        game.tick(0.0);
        assert!(
            game.sessions.contains_key(&7),
            "shed snapshots must not disconnect"
        );

        game.send_to(7, snapshot.clone());
        game.send_to(7, snapshot);
        game.send_to(7, crate::net::packet::gameversion());
        game.tick(0.0);
        assert!(
            !game.sessions.contains_key(&7),
            "reliable backpressure must disconnect"
        );
    }

    #[test]
    fn disconnect_immediately_cancels_map_transfer() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 1,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        game.connect_client(7, "test".into());
        game.map_transfers.push(super::MapTransfer {
            babonet_id: 7,
            data: vec![0; 1024],
            offset: 0,
        });
        game.disconnect_babonet(7);
        assert!(game.map_transfers.is_empty());
    }

    #[test]
    fn chat_uses_authenticated_name_and_enforces_team_and_spectator_routing() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 3,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        let mut receivers = Vec::new();
        for (slot, team) in [
            PLAYER_TEAM_BLUE,
            PLAYER_TEAM_RED,
            super::PLAYER_TEAM_SPECTATOR,
        ]
        .into_iter()
        .enumerate()
        {
            let mut player = Player::new(slot, slot as u32 + 1, "test".into());
            player.name = if slot == 0 {
                "\u{3}Ali\u{9b}ce".into()
            } else {
                format!("P{slot}")
            };
            player.team_id = team;
            player.handshake_done = true;
            game.players[slot] = Some(player);
            let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
            game.register_session(slot as u32 + 1, tx);
            receivers.push(rx);
        }
        let mut team_message = vec![0u8; 131];
        team_message[0] = PLAYER_TEAM_BLUE as u8;
        team_message[1..15].copy_from_slice(b"\x04server: spoof");
        game.handle_frame(1, crate::net::ids::NET_CLSV_SVCL_CHAT, &team_message);
        assert_eq!(game.recent_chat.len(), 1);
        let frame = receivers[0].try_recv().unwrap();
        let decoded = crate::net::packet::decode_frame(&frame).unwrap().unwrap();
        assert_eq!(decoded.payload[0] as i8, PLAYER_TEAM_BLUE);
        assert_eq!(
            crate::net::packet::read_str(&decoded.payload, 1, 130),
            "\u{3}Ali\u{9b}ce: server: spoof"
        );
        assert_eq!(game.recent_chat[0].player_name, "\u{3}Ali\u{9b}ce");
        assert!(receivers[1].try_recv().is_err());
        assert!(receivers[2].try_recv().is_err());

        game.handle_frame(
            3,
            crate::net::ids::NET_CLSV_SVCL_CHAT,
            &[PLAYER_TEAM_BLUE as u8, b'x'],
        );
        assert!(receivers
            .iter_mut()
            .all(|receiver| receiver.try_recv().is_err()));
    }

    #[test]
    fn champion_maps_are_isolated_to_champion_rotation() {
        for game_type in [0, 1, GAME_TYPE_CTF] {
            assert!(!map_allowed_for_game_type(game_type, "CHP-Lava"));
            assert!(map_allowed_for_game_type(game_type, "DM-Field"));
        }
        assert!(map_allowed_for_game_type(GAME_TYPE_SND, "CHP-Lava"));
        assert!(!map_allowed_for_game_type(GAME_TYPE_SND, "DM-Field"));
        assert!(!map_allowed_for_game_type(GAME_TYPE_SND, "CTF-Alert"));
    }

    #[test]
    fn only_spectator_blue_and_red_are_valid_team_requests() {
        assert!([-1, 0, 1].into_iter().all(valid_player_team));
        assert!(!valid_player_team(-2));
        assert!(!valid_player_team(2));
        assert!(!valid_player_team(127));
    }

    #[test]
    fn scoreboard_ping_is_encoded_as_elapsed_milliseconds() {
        assert_eq!(duration_ms_i16(Duration::from_millis(42)), 42);
        assert_eq!(duration_ms_i16(Duration::from_secs(60)), i16::MAX);
    }

    #[test]
    fn authenticated_spectators_are_pinged_before_their_first_spawn() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 1,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        let mut spectator = Player::new(0, 7, "test".into());
        spectator.handshake_done = true;
        spectator.team_id = PLAYER_TEAM_SPECTATOR;
        spectator.status = PLAYER_STATUS_DEAD;
        spectator.spawned_once = false;
        spectator.ping_cycle_started = Instant::now() - Duration::from_secs(2);
        game.players[0] = Some(spectator);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
        game.register_session(7, tx);

        game.tick(0.0);

        let packets: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(packets.iter().any(|packet| {
            crate::net::packet::decode_frame(packet).is_ok_and(|frame| {
                frame.is_some_and(|f| f.type_id == crate::net::ids::NET_SVCL_PING)
            })
        }));
    }

    #[test]
    fn joining_is_rejected_when_it_would_create_a_two_player_team_gap() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 8,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        let mut blue = Player::new(0, 1, "blue".into());
        blue.team_id = PLAYER_TEAM_BLUE;
        game.players[0] = Some(blue);
        game.players[1] = Some(Player::new(1, 2, "joining".into()));
        assert!(!game.team_join_allowed(1, PLAYER_TEAM_BLUE));
        assert!(game.team_join_allowed(1, PLAYER_TEAM_RED));
    }

    #[test]
    fn idle_player_gets_countdown_then_moves_to_spectators() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        let mut player = Player::new(0, 7, "test".into());
        player.name = "Idle".into();
        player.team_id = PLAYER_TEAM_BLUE;
        player.status = PLAYER_STATUS_ALIVE;
        player.cf.position = [5.5, 5.5, 0.25];
        player.idle_position = player.cf.position;
        game.players[0] = Some(player);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
        game.register_session(7, tx);

        game.tick_idle_spectators(20.01);
        let warning = rx.try_recv().expect("idle countdown warning");
        let frame = crate::net::packet::decode_frame(&warning).unwrap().unwrap();
        assert_eq!(frame.type_id, crate::net::ids::NET_SVCL_MSG);
        assert!(String::from_utf8_lossy(&frame.payload).contains("10s"));

        game.tick_idle_spectators(10.0);
        let player = game.players[0].as_ref().unwrap();
        assert_eq!(player.team_id, PLAYER_TEAM_SPECTATOR);
        assert_eq!(player.status, PLAYER_STATUS_DEAD);
    }

    #[test]
    fn real_movement_resets_idle_countdown() {
        let mut player = Player::new(0, 7, "test".into());
        player.team_id = PLAYER_TEAM_BLUE;
        player.status = PLAYER_STATUS_ALIVE;
        player.cf.position = [5.5, 5.5, 0.25];
        player.idle_position = player.cf.position;
        player.idle_elapsed = 25.0;
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        game.players[0] = Some(player);
        game.players[0].as_mut().unwrap().cf.position[0] += 0.1;
        game.players[0].as_mut().unwrap().cf.vel[0] = 1.0;

        game.tick_idle_spectators(10.0);
        let player = game.players[0].as_ref().unwrap();
        assert_eq!(player.team_id, PLAYER_TEAM_BLUE);
        assert_eq!(player.idle_elapsed, 0.0);
    }

    #[test]
    fn native_style_autobalance_warns_then_moves_a_non_carrier() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 8,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        for slot in 0..3 {
            let mut player = Player::new(slot, slot as u32 + 1, "blue".into());
            player.team_id = PLAYER_TEAM_BLUE;
            game.players[slot] = Some(player);
        }
        game.ctf.flag_state[1] = 2;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        game.register_session(99, tx);

        game.tick_auto_balance(0.02);
        let warning = rx.try_recv().expect("autobalance warning");
        let frame = crate::net::packet::decode_frame(&warning).unwrap().unwrap();
        assert_eq!(frame.type_id, crate::net::ids::NET_SVCL_AUTOBALANCE);
        game.tick_auto_balance(4.1);

        let (blue, red) = game.team_counts_without(super::MAX_PLAYER);
        assert_eq!((blue, red), (2, 1));
        assert_eq!(game.players[2].as_ref().unwrap().team_id, PLAYER_TEAM_BLUE);
    }

    #[test]
    fn teleport_is_corrected_to_elapsed_speed_envelope() {
        let previous = [5.0, 5.0, 0.25];
        let mut requested = [50.0, 5.0, 8.0];
        let mut velocity = [12.0, 0.0, 4.0];
        bound_client_movement(
            &map(),
            previous,
            [0.0; 3],
            &mut requested,
            &mut velocity,
            0.1,
        );
        assert!((requested[0] - 5.445).abs() < 0.001);
        assert_eq!(requested[1], 5.0);
        assert_eq!(requested[2], 0.25);
        assert!((velocity[0] - 3.25).abs() < 0.001);
        assert_eq!(velocity[2], 0.0);
    }

    #[test]
    fn player_body_collision_blocks_endpoint_and_swept_tunneling() {
        let mut positions = [None; MAX_PLAYER];
        positions[0] = Some([1.0, 1.0, 0.25]);
        positions[1] = Some([2.0, 1.0, 0.25]);
        let mut requested = [3.0, 1.0, 0.25];
        let mut velocity = [3.0, 0.0, 0.0];
        clip_player_motion_xy(
            0,
            &positions,
            positions[0].unwrap(),
            &mut requested,
            &mut velocity,
        );
        assert!(requested[0] < 1.51, "player tunneled to x={}", requested[0]);
        assert_eq!(velocity[..2], [0.0, 0.0]);
    }

    #[test]
    fn overlapping_player_bodies_are_separated_deterministically() {
        let mut positions = [None; MAX_PLAYER];
        positions[0] = Some([2.0, 2.0, 0.25]);
        positions[1] = Some([2.0, 2.0, 0.25]);
        let mut requested = [2.1, 2.0, 0.25];
        let mut velocity = [1.0, 0.0, 0.0];
        clip_player_motion_xy(
            0,
            &positions,
            positions[0].unwrap(),
            &mut requested,
            &mut velocity,
        );
        assert!(requested[0] < 1.5);
        assert_eq!(velocity[..2], [0.0, 0.0]);
    }

    #[test]
    fn legitimate_diagonal_sample_is_unchanged() {
        let previous = [5.0, 5.0, 0.25];
        let mut requested = [5.1, 5.1, 0.25];
        let mut velocity = [2.0, 2.0, 0.0];
        bound_client_movement(
            &map(),
            previous,
            [0.0; 3],
            &mut requested,
            &mut velocity,
            0.05,
        );
        assert_eq!(requested, [5.1, 5.1, 0.25]);
        assert_eq!(velocity, [2.0, 2.0, 0.0]);
    }

    #[test]
    fn movement_beyond_open_edge_falls_under_server_gravity() {
        let previous = [0.05, 5.0, 0.25];
        let mut requested = [-0.05, 5.0, 0.25];
        let mut velocity = [-1.0, 0.0, 0.0];
        bound_client_movement(
            &map(),
            previous,
            [0.0; 3],
            &mut requested,
            &mut velocity,
            0.1,
        );
        assert!(requested[0] < 0.0);
        assert_eq!(requested[2], 0.25);
        assert!(velocity[2] < 0.0);
    }
}
