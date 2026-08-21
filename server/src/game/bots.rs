//! Server-side bots — tile-aware navigation and CTF objectives.

use crate::game::bot_roster::{bot_decals, bot_name, bot_skin};
use crate::game::ctf::{CtfState, FLAG_AT_POD, FLAG_DROPPED, GAME_TYPE_CTF};
use crate::game::{
    clip_player_motion_xy, player_spawn_bytes, BotIdentity, GameServer, Player, MAX_PLAYER,
    PLAYER_STATUS_ALIVE, PLAYER_STATUS_DEAD, PLAYER_TEAM_BLUE, PLAYER_TEAM_RED, SV_TIME_TO_SPAWN,
};
use crate::map::clip_entity_xy;
use crate::net::{ids, packet};
use serde::Serialize;

const BOT_SPEED: f32 = 2.8;
const BOT_FIRE_INTERVAL: f32 = 0.11;
pub const BOT_RESPAWN_DELAY: f32 = SV_TIME_TO_SPAWN;
const BOT_AIM_RANGE: f32 = 14.0;
const BOT_GOAL_REFRESH: f32 = 0.35;
const BOT_STUCK_THRESHOLD: f32 = 0.04;
const BOT_STUCK_TIME: f32 = 0.45;
const BOT_LOCAL_GOAL_STALE_TIME: f32 = 2.2;
const BOT_DEFEND_PRESSURE_RANGE: f32 = 6.8;
const BOT_ESCAPE_STEP: f32 = 1.8;
const BOT_NAV_NO_PROGRESS_TIME: f32 = 0.45;
const BOT_NAV_PROGRESS_EPSILON: f32 = 0.025;
const BOT_NAV_WAYPOINT_COMMIT_TIME: f32 = 0.75;
const PLAYER_RADIUS: f32 = 0.25;
const BOT_SEPARATION_RADIUS: f32 = 0.9;
const BOT_SEPARATION_WEIGHT: f32 = 1.35;
const BOT_AIM_ERROR: f32 = 0.055;
const BOT_MIN_GOAL_COMMITMENT: f32 = 0.8;
const BOT_TARGET_ACQUIRE_TIME: f32 = 0.12;
const BOT_ENEMY_MEMORY_TIME: f32 = 1.5;
const BOT_PERCEPTION_INTERVAL: f32 = 0.08;
const BOT_HEARING_RANGE: f32 = 8.0;
const BOT_AUDIBLE_SHOT_AGE: f32 = 0.2;
const UTILITY_CAPTURE: f32 = 1_000.0;
const UTILITY_RECOVER: f32 = 900.0;
const UTILITY_ESCORT: f32 = 70.0;
const UTILITY_ATTACK: f32 = 40.0;
const UTILITY_OBJECTIVE: f32 = 30.0;
const UTILITY_WANDER: f32 = 5.0;
const UTILITY_RETREAT_HEALTH: f32 = 180.0;
const BOT_CARRIER_DIRECT_CAPTURE_DISTANCE: f32 = 3.5;
const BOT_CTF_CARRIER_ESCORT_COUNT: usize = 2;
const BOT_CTF_MISSING_FLAG_RECOVER_COUNT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BotTacticalState {
    Wander,
    Attack,
    AttackObjective,
    Capture,
    Escort,
    Recover,
    Retreat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BotRole {
    Attacker,
    Defender,
    Escort,
    Recoverer,
    Interceptor,
    Carrier,
}

const TACTICAL_STATES: [BotTacticalState; 7] = [
    BotTacticalState::Wander,
    BotTacticalState::Attack,
    BotTacticalState::AttackObjective,
    BotTacticalState::Capture,
    BotTacticalState::Escort,
    BotTacticalState::Recover,
    BotTacticalState::Retreat,
];

#[derive(Debug, Clone)]
pub struct BotBrain {
    pub role: BotRole,
    pub state: BotTacticalState,
    pub reason: &'static str,
    pub committed_for: f32,
    pub state_changes: u32,
    pub shots_fired: u32,
    pub occluded_shots_rejected: u32,
    pub stuck_events: u32,
    pub perceived_target: Option<usize>,
    pub last_known_enemy: [f32; 2],
    pub enemy_memory_remaining: f32,
    pub target_visible_for: f32,
    pub perception_elapsed: f32,
    pub utilities: [f32; 7],
    pub aim_point: [f32; 2],
    pub aim_velocity: [f32; 2],
    pub recovery_dir: f32,
    pub recovery_timer: f32,
    pub recovery_tries: u8,
    pub burst_remaining: u8,
    pub burst_pause: f32,
    pub spawn_pressure: f32,
    pub role_committed_for: f32,
    pub goal_stale_time: f32,
    pub last_goal: [f32; 2],
    pub nav_goal: [f32; 2],
    pub nav_waypoint: [f32; 2],
    pub nav_from_tile: [i32; 2],
    pub nav_waypoint_time: f32,
    pub nav_best_distance: f32,
    pub nav_no_progress_time: f32,
}

impl Default for BotBrain {
    fn default() -> Self {
        Self {
            role: BotRole::Attacker,
            state: BotTacticalState::Wander,
            reason: "spawned",
            committed_for: 0.0,
            state_changes: 0,
            shots_fired: 0,
            occluded_shots_rejected: 0,
            stuck_events: 0,
            perceived_target: None,
            last_known_enemy: [0.0; 2],
            enemy_memory_remaining: 0.0,
            target_visible_for: 0.0,
            perception_elapsed: 0.0,
            utilities: [0.0; 7],
            aim_point: [0.0; 2],
            aim_velocity: [0.0; 2],
            recovery_dir: 1.0,
            recovery_timer: 0.0,
            recovery_tries: 0,
            burst_remaining: 0,
            burst_pause: 0.0,
            spawn_pressure: 0.0,
            role_committed_for: BOT_MIN_GOAL_COMMITMENT,
            goal_stale_time: 0.0,
            last_goal: [0.0; 2],
            nav_goal: [f32::INFINITY; 2],
            nav_waypoint: [0.0; 2],
            nav_from_tile: [i32::MIN; 2],
            nav_waypoint_time: 0.0,
            nav_best_distance: f32::INFINITY,
            nav_no_progress_time: 0.0,
        }
    }
}

fn update_perception(
    brain: &mut BotBrain,
    visible: Option<(usize, [f32; 3], f32)>,
    heard: Option<(usize, [f32; 3])>,
    dt: f32,
) -> Option<(usize, f32)> {
    if let Some((slot, position, distance)) = visible {
        if brain.perceived_target == Some(slot) {
            brain.target_visible_for += dt;
        } else {
            brain.perceived_target = Some(slot);
            brain.target_visible_for = 0.0;
        }
        brain.last_known_enemy = [position[0], position[1]];
        brain.enemy_memory_remaining = BOT_ENEMY_MEMORY_TIME;
        if brain.target_visible_for >= BOT_TARGET_ACQUIRE_TIME {
            return Some((slot, distance));
        }
        return None;
    }

    if let Some((slot, position)) = heard {
        brain.perceived_target = Some(slot);
        brain.last_known_enemy = [position[0], position[1]];
        brain.enemy_memory_remaining = BOT_ENEMY_MEMORY_TIME;
        brain.target_visible_for = 0.0;
        return None;
    }

    brain.target_visible_for = 0.0;
    brain.enemy_memory_remaining = (brain.enemy_memory_remaining - dt).max(0.0);
    if brain.enemy_memory_remaining == 0.0 {
        brain.perceived_target = None;
    }
    None
}

#[derive(Debug, Serialize)]
pub struct BotSimulationSummary {
    pub scenario: String,
    pub seed: u64,
    pub ticks: u64,
    pub bots: usize,
    pub state_changes: u32,
    pub shots_fired: u32,
    pub occluded_shots: u32,
    pub stuck_events: u32,
    pub states: std::collections::BTreeMap<String, usize>,
    pub roles: std::collections::BTreeMap<String, usize>,
    pub captures: i16,
    pub returns: i32,
    pub deaths: i32,
    pub accuracy: f32,
    pub baseline_pass: bool,
}

pub fn bot_scenario_summary(
    game: &GameServer,
    seed: u64,
    ticks: u64,
    scenario: &str,
) -> BotSimulationSummary {
    let mut summary = BotSimulationSummary {
        scenario: scenario.to_string(),
        seed,
        ticks,
        bots: 0,
        state_changes: 0,
        shots_fired: 0,
        occluded_shots: 0,
        stuck_events: 0,
        states: std::collections::BTreeMap::new(),
        roles: std::collections::BTreeMap::new(),
        captures: game.blue_score + game.red_score,
        returns: 0,
        deaths: 0,
        accuracy: 0.0,
        baseline_pass: true,
    };
    for player in game.players.iter().flatten().filter(|player| player.is_bot) {
        summary.bots += 1;
        summary.state_changes += player.bot_brain.state_changes;
        summary.shots_fired += player.bot_brain.shots_fired;
        summary.occluded_shots += player.bot_brain.occluded_shots_rejected;
        summary.stuck_events += player.bot_brain.stuck_events;
        summary.returns += i32::from(player.returns);
        summary.deaths += i32::from(player.deaths);
        *summary
            .states
            .entry(format!("{:?}", player.bot_brain.state).to_ascii_lowercase())
            .or_default() += 1;
        *summary
            .roles
            .entry(format!("{:?}", player.bot_brain.role).to_ascii_lowercase())
            .or_default() += 1;
    }
    let hits = game.players.iter().flatten().map(|p| p.dmg).sum::<f32>();
    summary.accuracy = if summary.shots_fired == 0 {
        0.0
    } else {
        (hits / summary.shots_fired as f32).clamp(0.0, 1.0)
    };
    summary.baseline_pass = summary.occluded_shots == 0
        && summary.stuck_events <= (ticks / 300).max(1) as u32
        && summary.roles.len() >= usize::from(summary.bots > 1);
    summary
}

pub fn configure_bot_scenario(game: &mut GameServer, scenario: &str, seed: u64) {
    let jitter = (seed % 5) as f32 * 0.05;
    match scenario {
        "carrier_escort" | "spawn_pressure" if game.ctf.supports_ctf() => {
            let carrier = game.players.iter().enumerate().find_map(|(slot, p)| {
                p.as_ref()
                    .filter(|p| p.team_id == PLAYER_TEAM_BLUE)
                    .map(|_| slot)
            });
            if let Some(carrier) = carrier {
                let home = game.ctf.flag_pod[0];
                let enemy = game.ctf.flag_pod[1];
                let lane_x = enemy[0] - home[0];
                let lane_y = enemy[1] - home[1];
                let lane_len = lane_x.hypot(lane_y).max(0.01);
                let near_home = game.map_grid.resolve_navigation_goal([
                    home[0] + lane_x / lane_len * (1.25 + jitter),
                    home[1] + lane_y / lane_len * 1.25,
                ]);
                let carrier_start = [near_home[0], near_home[1], 0.25];
                game.ctf.flag_state[1] = carrier as i8;
                game.ctf.flag_pos[1] = carrier_start;
                if let Some(player) = game.players[carrier].as_mut() {
                    player.cf.position = carrier_start;
                    player.cf.vel = [0.0; 3];
                }
                if scenario == "spawn_pressure" {
                    if let Some(enemy) = game
                        .players
                        .iter_mut()
                        .flatten()
                        .find(|p| p.team_id == PLAYER_TEAM_RED)
                    {
                        enemy.status = PLAYER_STATUS_DEAD;
                        enemy.time_to_spawn = SV_TIME_TO_SPAWN;
                    }
                }
            }
        }
        "separation" => {
            let position = game.players.iter().flatten().next().map(|p| p.cf.position);
            if let Some(position) = position {
                for bot in game
                    .players
                    .iter_mut()
                    .flatten()
                    .filter(|p| p.is_bot)
                    .take(3)
                {
                    bot.cf.position = position;
                }
            }
        }
        "cover_retreat" => {
            if let Some(bot) = game.players.iter_mut().flatten().find(|p| p.is_bot) {
                bot.life = 0.1;
            }
        }
        "wall_occlusion" | "full_ctf_round" => {}
        _ => {}
    }
}

fn assign_ctf_roles(
    team: i8,
    players: &[Option<Player>; MAX_PLAYER],
    ctf: &CtfState,
    should_defend: bool,
) -> [Option<BotRole>; MAX_PLAYER] {
    let mut roles = [None; MAX_PLAYER];
    let mut roster: Vec<usize> = players
        .iter()
        .enumerate()
        .filter_map(|(slot, p)| {
            p.as_ref()
                .filter(|p| p.is_bot && p.team_id == team)
                .map(|_| slot)
        })
        .collect();
    roster.sort_unstable();
    let mut bots: Vec<usize> = roster
        .iter()
        .copied()
        .filter(|slot| {
            players[*slot]
                .as_ref()
                .is_some_and(|p| p.status == PLAYER_STATUS_ALIVE)
        })
        .collect();
    bots.sort_unstable();
    if bots.is_empty() {
        return roles;
    }
    let own_flag = if team == PLAYER_TEAM_BLUE { 0 } else { 1 };
    let enemy_flag = 1 - own_flag;
    // The sorted roster is the team's shared plan: roughly one third are
    // stable defensive personalities and the rest are offense. Every bot sees
    // this same role array for the tick, so dynamic assignments cannot cause
    // the whole team to independently choose the same objective.
    let defender_count = if roster.len() <= 1 {
        0
    } else {
        roster.len().div_ceil(3).clamp(1, 3).min(roster.len() - 1)
    };
    let natural_defenders: Vec<_> = roster.iter().copied().take(defender_count).collect();
    for slot in natural_defenders
        .iter()
        .copied()
        .filter(|slot| bots.contains(slot))
    {
        roles[slot] = Some(BotRole::Defender);
    }
    if defender_count > 0
        && !bots
            .iter()
            .any(|slot| roles[*slot] == Some(BotRole::Defender))
    {
        roles[bots[0]] = Some(BotRole::Defender);
    }
    let enemy_flag_carrier = if ctf.flag_state[enemy_flag] >= 0 {
        Some(ctf.flag_state[enemy_flag] as usize)
    } else {
        None
    };
    if let Some(carrier) = enemy_flag_carrier {
        if bots.contains(&carrier) {
            roles[carrier] = Some(BotRole::Carrier);
        }
        let escort_count = if ctf.flag_state[own_flag] == FLAG_DROPPED {
            1
        } else {
            BOT_CTF_CARRIER_ESCORT_COUNT
        };
        let escorts: Vec<_> = bots
            .iter()
            .copied()
            .filter(|slot| *slot != carrier && !natural_defenders.contains(slot))
            .take(escort_count)
            .collect();
        for slot in escorts {
            roles[slot] = Some(BotRole::Escort);
        }
    }
    if ctf.flag_state[own_flag] == FLAG_DROPPED {
        let recoverers: Vec<_> = bots
            .iter()
            .copied()
            .filter(|slot| !natural_defenders.contains(slot) && roles[*slot].is_none())
            .take(BOT_CTF_MISSING_FLAG_RECOVER_COUNT)
            .collect();
        for slot in recoverers {
            roles[slot] = Some(BotRole::Recoverer);
        }
    } else if ctf.flag_state[own_flag] >= 0 {
        let interceptors: Vec<_> = bots
            .iter()
            .copied()
            .filter(|slot| !natural_defenders.contains(slot) && roles[*slot].is_none())
            .take(2)
            .collect();
        for slot in interceptors {
            roles[slot] = Some(BotRole::Interceptor);
        }
    }
    if should_defend {
        if let Some(slot) = bots
            .iter()
            .copied()
            .find(|slot| roles[*slot] == Some(BotRole::Attacker) || roles[*slot].is_none())
        {
            roles[slot] = Some(BotRole::Defender);
        }
    }
    for slot in bots {
        roles[slot].get_or_insert(BotRole::Attacker);
    }
    roles
}

fn enemy_spawn_points(team: i8, spawns: &crate::config::MapSpawns) -> &[[f32; 3]] {
    let points = if team == PLAYER_TEAM_BLUE {
        &spawns.red
    } else {
        &spawns.blue
    };
    if points.is_empty() {
        &spawns.dm
    } else {
        points
    }
}

/// Predict the spawn an enemy is most likely to receive. BV2 selects spawns
/// away from living opponents, so the defenders can make the same inference
/// from the positions shared by their team rather than waiting at the pole.
fn predicted_enemy_spawn(
    team: i8,
    spawns: &crate::config::MapSpawns,
    live_positions: &[Option<[f32; 3]>],
    live_teams: &[Option<i8>],
) -> Option<[f32; 2]> {
    enemy_spawn_points(team, spawns)
        .iter()
        .max_by(|a, b| {
            let nearest_friend = |spawn: &&[f32; 3]| {
                live_positions
                    .iter()
                    .enumerate()
                    .filter(|(slot, pos)| pos.is_some() && live_teams[*slot] == Some(team))
                    .map(|(_, pos)| {
                        let pos = pos.unwrap();
                        (spawn[0] - pos[0]).hypot(spawn[1] - pos[1])
                    })
                    .fold(f32::INFINITY, f32::min)
            };
            nearest_friend(a).total_cmp(&nearest_friend(b))
        })
        .map(|spawn| [spawn[0], spawn[1]])
}

fn spawn_danger(point: [f32; 2], spawns: &[[f32; 3]], pressure: f32) -> f32 {
    spawns
        .iter()
        .map(|spawn| pressure / (point[0] - spawn[0]).hypot(point[1] - spawn[1]).max(0.5))
        .sum()
}

fn flag_is_visible(grid: &crate::map::MapGrid, point: [f32; 3]) -> bool {
    grid.contains_xy(point[0], point[1])
        && grid.is_passable(point[0].floor() as i32, point[1].floor() as i32)
}

fn defender_pressure(
    team: i8,
    ctf: &CtfState,
    live_positions: &[Option<[f32; 3]>],
    live_teams: &[Option<i8>],
) -> bool {
    let own_flag = if team == PLAYER_TEAM_BLUE { 0 } else { 1 };
    let enemy_flag = 1 - own_flag;
    if ctf.flag_state[own_flag] == FLAG_DROPPED {
        return true;
    }

    if ctf.flag_state[enemy_flag] >= 0 {
        let carrier = ctf.flag_state[enemy_flag] as usize;
        if carrier < MAX_PLAYER {
            if let Some(carrier_pos) = live_positions[carrier] {
                let pod = ctf.flag_pod[own_flag];
                if (carrier_pos[0] - pod[0]).hypot(carrier_pos[1] - pod[1])
                    <= BOT_DEFEND_PRESSURE_RANGE
                {
                    return true;
                }
            }
        }
    }

    let pod = ctf.flag_pod[own_flag];
    let enemy_team = if team == PLAYER_TEAM_BLUE {
        PLAYER_TEAM_RED
    } else {
        PLAYER_TEAM_BLUE
    };
    live_positions.iter().enumerate().any(|(slot, pos)| {
        if live_teams[slot] != Some(enemy_team) {
            return false;
        }
        let Some(enemy_pos) = pos else {
            return false;
        };
        (enemy_pos[0] - pod[0]).hypot(enemy_pos[1] - pod[1]) <= BOT_DEFEND_PRESSURE_RANGE * 0.85
    })
}

fn pressure_spawn_goal(spawns: &[[f32; 3]], carrier: [f32; 3], pod: [f32; 3]) -> [f32; 2] {
    spawns
        .iter()
        .min_by(|a, b| {
            let route_distance = |spawn: &&[f32; 3]| {
                let route_mid = [(carrier[0] + pod[0]) * 0.5, (carrier[1] + pod[1]) * 0.5];
                (spawn[0] - route_mid[0]).hypot(spawn[1] - route_mid[1])
            };
            route_distance(a).total_cmp(&route_distance(b))
        })
        .map(|spawn| [spawn[0], spawn[1]])
        .unwrap_or([pod[0], pod[1]])
}

fn safer_carrier_goal(
    carrier: [f32; 3],
    pod: [f32; 3],
    spawns: &[[f32; 3]],
    pressure: f32,
) -> [f32; 2] {
    let dx = pod[0] - carrier[0];
    let dy = pod[1] - carrier[1];
    let len = dx.hypot(dy).max(0.01);
    // Once the pod is close, route safety no longer outweighs scoring. A
    // lateral detour here made carriers orbit just outside TAKE_RADIUS.
    if len <= BOT_CARRIER_DIRECT_CAPTURE_DISTANCE {
        return [pod[0], pod[1]];
    }
    let candidates = [
        [pod[0], pod[1]],
        [
            carrier[0] + dx * 0.5 - dy / len * 2.0,
            carrier[1] + dy * 0.5 + dx / len * 2.0,
        ],
        [
            carrier[0] + dx * 0.5 + dy / len * 2.0,
            carrier[1] + dy * 0.5 - dx / len * 2.0,
        ],
    ];
    candidates
        .into_iter()
        .min_by(|a, b| {
            let score = |p: [f32; 2]| {
                (p[0] - carrier[0]).hypot(p[1] - carrier[1])
                    + spawn_danger(p, spawns, pressure) * 4.0
            };
            score(*a).total_cmp(&score(*b))
        })
        .unwrap_or([pod[0], pod[1]])
}

fn desired_tactical_state(
    slot: usize,
    team: i8,
    life: f32,
    combat_target: bool,
    ctf: Option<&CtfState>,
    role: BotRole,
) -> (BotTacticalState, &'static str, bool, [f32; 7]) {
    let mut utilities = [0.0; 7];
    utilities[0] = if ctf.is_none() { UTILITY_WANDER } else { 0.0 };
    utilities[1] = if combat_target {
        UTILITY_ATTACK * (0.5 + life.clamp(0.0, 1.0))
    } else {
        0.0
    };
    utilities[2] = if ctf.is_some() && matches!(role, BotRole::Attacker | BotRole::Defender) {
        UTILITY_OBJECTIVE
    } else {
        0.0
    };
    utilities[6] = (0.55 - life).max(0.0) * UTILITY_RETREAT_HEALTH;

    if let Some(ctf) = ctf {
        let enemy_flag = if team == PLAYER_TEAM_BLUE { 1 } else { 0 };
        if role == BotRole::Carrier || ctf.flag_state[enemy_flag] == slot as i8 {
            utilities[3] = UTILITY_CAPTURE;
        }
        if matches!(role, BotRole::Recoverer | BotRole::Interceptor) {
            utilities[5] = UTILITY_RECOVER;
        }
        if role == BotRole::Escort {
            utilities[4] = UTILITY_ESCORT;
        }
    }

    let mut best = 0usize;
    for index in 1..utilities.len() {
        if utilities[index] > utilities[best] {
            best = index;
        }
    }
    let state = TACTICAL_STATES[best];
    let reason = match state {
        BotTacticalState::Wander => "searching",
        BotTacticalState::Attack => "visible enemy has highest utility",
        BotTacticalState::AttackObjective => "advance on enemy flag",
        BotTacticalState::Capture => "carrying enemy flag",
        BotTacticalState::Escort => "screening allied carrier",
        BotTacticalState::Recover => "own flag is threatened",
        BotTacticalState::Retreat => "survival utility",
    };
    let urgent = matches!(state, BotTacticalState::Capture | BotTacticalState::Recover)
        || (state == BotTacticalState::Retreat && life < 0.3);
    (state, reason, urgent, utilities)
}

fn update_tactical_state(
    brain: &mut BotBrain,
    desired: BotTacticalState,
    reason: &'static str,
    urgent: bool,
    dt: f32,
) {
    brain.committed_for += dt;
    if desired == brain.state {
        brain.reason = reason;
        return;
    }
    if urgent || brain.committed_for >= BOT_MIN_GOAL_COMMITMENT {
        brain.state = desired;
        brain.reason = reason;
        brain.committed_for = 0.0;
        brain.state_changes += 1;
    }
}

fn update_role(brain: &mut BotBrain, desired: BotRole, urgent: bool, dt: f32) {
    brain.role_committed_for += dt;
    if desired == brain.role {
        return;
    }
    if urgent || brain.role_committed_for >= BOT_MIN_GOAL_COMMITMENT {
        brain.role = desired;
        brain.role_committed_for = 0.0;
    }
}

fn weapon_utility(weapon_id: i8, distance: f32, has_ammo: bool, reloading: bool) -> f32 {
    if !has_ammo {
        return 0.0;
    }
    let distance = distance.max(0.0);
    let range_score = match weapon_id {
        1 => (1.0 - distance / 5.0).clamp(0.05, 1.0), // shotgun
        2 => (distance / 10.0).clamp(0.15, 1.0),      // sniper
        5 => (1.0 - (distance - 7.0).abs() / 8.0).clamp(0.1, 1.0), // bazooka
        7 => (1.0 - distance / 3.0).clamp(0.05, 1.0), // old-school flamethrower
        _ => (1.0 - (distance - 5.0).abs() / 14.0).clamp(0.25, 1.0),
    };
    range_score * if reloading { 0.2 } else { 1.0 }
}

fn find_cover_goal(
    grid: &crate::map::MapGrid,
    from: [f32; 3],
    threat: [f32; 3],
) -> Option<[f32; 2]> {
    let origin_x = from[0].floor() as i32;
    let origin_y = from[1].floor() as i32;
    let mut best: Option<([f32; 2], f32)> = None;
    for radius in 1..=4 {
        for y in origin_y - radius..=origin_y + radius {
            for x in origin_x - radius..=origin_x + radius {
                if x != origin_x - radius
                    && x != origin_x + radius
                    && y != origin_y - radius
                    && y != origin_y + radius
                {
                    continue;
                }
                if !grid.is_passable(x, y) {
                    continue;
                }
                let candidate = [x as f32 + 0.5, y as f32 + 0.5, 0.25];
                let (wall, _) = grid.ray_test(threat, candidate);
                let threat_distance = (candidate[0] - threat[0]).hypot(candidate[1] - threat[1]);
                let wall_distance = (wall[0] - threat[0]).hypot(wall[1] - threat[1]);
                if wall_distance + 0.1 >= threat_distance {
                    continue;
                }
                let travel = (candidate[0] - from[0]).hypot(candidate[1] - from[1]);
                let score = travel - threat_distance * 0.15;
                if best
                    .as_ref()
                    .is_none_or(|(_, best_score)| score < *best_score)
                {
                    best = Some(([candidate[0], candidate[1]], score));
                }
            }
        }
        if best.is_some() {
            break;
        }
    }
    best.map(|(goal, _)| goal)
}

fn flank_goal(slot: usize, from: [f32; 3], target: [f32; 3]) -> [f32; 2] {
    let dx = target[0] - from[0];
    let dy = target[1] - from[1];
    let distance = dx.hypot(dy).max(0.01);
    let nx = dx / distance;
    let ny = dy / distance;
    // Team slots alternate globally (blue even, red odd), so raw parity puts
    // an entire team on one flank. Alternate by the within-team slot index.
    let side = if (slot / 2) & 1 == 0 { 1.0 } else { -1.0 };
    let lateral = (1.1 + (slot % 3) as f32 * 0.35) * side;
    [
        target[0] - nx * 2.5 - ny * lateral,
        target[1] - ny * 2.5 + nx * lateral,
    ]
}

fn intercept_goal(position: [f32; 3], velocity: [f32; 3], horizon: f32) -> [f32; 2] {
    [
        position[0] + velocity[0] * horizon,
        position[1] + velocity[1] * horizon,
    ]
}

fn update_human_aim(
    brain: &mut BotBrain,
    current_look: [f32; 2],
    desired: [f32; 2],
    dt: f32,
    personality: usize,
) -> [f32; 2] {
    if brain.aim_point == [0.0; 2] {
        brain.aim_point = current_look;
    }
    let responsiveness = 5.5 + (personality % 5) as f32 * 0.45;
    let max_speed = 12.0 + (personality % 3) as f32 * 1.25;
    let max_accel = 32.0 + (personality % 4) as f32 * 3.0;
    for (axis, desired_axis) in desired.iter().enumerate().take(2) {
        let wanted_velocity =
            ((*desired_axis - brain.aim_point[axis]) * responsiveness).clamp(-max_speed, max_speed);
        let change =
            (wanted_velocity - brain.aim_velocity[axis]).clamp(-max_accel * dt, max_accel * dt);
        brain.aim_velocity[axis] += change;
        brain.aim_point[axis] += brain.aim_velocity[axis] * dt;
    }
    brain.aim_point
}

fn recovery_heading(
    grid: &crate::map::MapGrid,
    from: [f32; 3],
    preferred: [f32; 2],
    forward: [f32; 2],
    side: f32,
    _tries: u8,
) -> [f32; 2] {
    let forward_len = (forward[0] * forward[0] + forward[1] * forward[1]).sqrt();
    let side_sign = if side >= 0.0 { 1.0 } else { -1.0 };
    if forward_len <= 0.001 {
        return grid.unstuck_heading([from[0], from[1]], preferred);
    }

    let nx = -forward[1] / forward_len * side_sign;
    let ny = forward[0] / forward_len * side_sign;
    let sidestep = [from[0] + nx * 1.5, from[1] + ny * 1.5];
    let forward_step = [
        from[0] + (forward[0] / forward_len) * 1.5,
        from[1] + (forward[1] / forward_len) * 1.5,
    ];
    let rx = -forward[0] / forward_len;
    let ry = -forward[1] / forward_len;
    let reverse_step = [
        from[0] + rx * BOT_ESCAPE_STEP,
        from[1] + ry * BOT_ESCAPE_STEP,
    ];
    let sidestep2 = [from[0] - nx * 1.5, from[1] - ny * 1.5];
    let forward_to_wall = [
        from[0] + (forward[0] / forward_len) * 0.6,
        from[1] + (forward[1] / forward_len) * 0.6,
    ];
    let (wall_hit, wall_normal) = grid.ray_test(
        [from[0], from[1], from[2]],
        [forward_to_wall[0], forward_to_wall[1], from[2]],
    );
    let wall_t = crate::map::segment_hit_t(
        [from[0], from[1], from[2]],
        [forward_to_wall[0], forward_to_wall[1], from[2]],
        wall_hit,
    );
    let wall_ahead = wall_t < 0.98 && wall_normal[2].abs() < 0.2;
    let candidates = if wall_ahead {
        [sidestep, reverse_step, sidestep2, forward_step]
    } else {
        [forward_step, sidestep, sidestep2, reverse_step]
    };
    let preferred_heading = candidates
        .into_iter()
        .map(|point| grid.unstuck_heading([from[0], from[1]], point))
        .find(|point| {
            let dx = point[0] - from[0];
            let dy = point[1] - from[1];
            dx.hypot(dy) > 0.03
        })
        .unwrap_or_else(|| grid.unstuck_heading([from[0], from[1]], preferred));
    // Keep the chosen recovery side for the whole recovery window. The old
    // try-number jitter replaced this result with an arbitrary compass heading,
    // producing visible spins and left/right oscillation at corners.
    preferred_heading
}

fn resolve_stuck_bots(brain: &mut BotBrain, carrying_enemy_flag: bool) {
    if carrying_enemy_flag {
        brain.recovery_tries = brain.recovery_tries.saturating_add(1);
        brain.recovery_timer = 0.45;
        brain.recovery_dir *= -1.0;
        return;
    }

    brain.recovery_tries = brain.recovery_tries.saturating_add(1);
    brain.recovery_timer = 0.30;
    if brain.recovery_tries > 3 {
        brain.recovery_dir *= -1.0;
        brain.recovery_tries = 1;
    }
}

fn consume_burst_shot(brain: &mut BotBrain, slot: usize) -> bool {
    if brain.burst_pause > 0.0 {
        return false;
    }
    if brain.burst_remaining == 0 {
        brain.burst_remaining = 3 + (slot % 4) as u8;
    }
    brain.burst_remaining -= 1;
    if brain.burst_remaining == 0 {
        brain.burst_pause = 0.18 + (slot % 5) as f32 * 0.035;
    }
    true
}

pub fn spawn_initial_bots(game: &mut GameServer) {
    let count = game.config.bot_count.min(MAX_PLAYER);
    if count == 0 {
        return;
    }
    tracing::info!("spawning {count} debug bot(s) (BV2_BOTS)");
    let mut bot_index = 0usize;
    for i in 0..count {
        if game.players[i].is_some() {
            continue;
        }
        let team = if i % 2 == 0 {
            PLAYER_TEAM_BLUE
        } else {
            PLAYER_TEAM_RED
        };
        game.add_bot(i, team, bot_index);
        bot_index += 1;
    }
}

pub fn tick_bots(game: &mut GameServer, dt: f32) {
    if game.config.bot_count == 0 {
        return;
    }

    let mut respawn_slots = Vec::new();
    let mut fire_targets: Vec<(usize, [f32; 2])> = Vec::new();
    let mut rocket_targets: Vec<(usize, [f32; 2])> = Vec::new();
    let mut grenade_targets: Vec<(usize, [f32; 2])> = Vec::new();
    let mut molotov_targets: Vec<(usize, [f32; 2])> = Vec::new();
    let mut melee_slots: Vec<usize> = Vec::new();
    type BotMove = (usize, [f32; 3], [f32; 3], [f32; 3], i32);
    let mut moves: Vec<BotMove> = Vec::new();

    let live_positions: Vec<Option<[f32; 3]>> = game
        .players
        .iter()
        .map(|p| {
            p.as_ref()
                .filter(|pl| pl.status == PLAYER_STATUS_ALIVE)
                .map(|pl| pl.cf.position)
        })
        .collect();
    let teams: Vec<Option<i8>> = game
        .players
        .iter()
        .map(|p| p.as_ref().map(|pl| pl.team_id))
        .collect();
    let live_velocities: Vec<Option<[f32; 3]>> = game
        .players
        .iter()
        .map(|player| {
            player
                .as_ref()
                .filter(|player| player.status == PLAYER_STATUS_ALIVE)
                .map(|player| player.cf.vel)
        })
        .collect();
    let shot_ages: Vec<Option<f32>> = game
        .players
        .iter()
        .map(|player| player.as_ref().map(|player| player.elapsed_since_last_shot))
        .collect();
    let game_type = game.config.game_type;
    let ctf_active = game_type == GAME_TYPE_CTF && game.ctf.supports_ctf();
    let ctf_snapshot = if ctf_active {
        Some(game.ctf.clone())
    } else {
        None
    };
    let blue_defend_pressure = ctf_snapshot
        .as_ref()
        .is_some_and(|ctf| defender_pressure(PLAYER_TEAM_BLUE, ctf, &live_positions, &teams));
    let red_defend_pressure = ctf_snapshot
        .as_ref()
        .is_some_and(|ctf| defender_pressure(PLAYER_TEAM_RED, ctf, &live_positions, &teams));
    let blue_roles = ctf_snapshot
        .as_ref()
        .map(|ctf| assign_ctf_roles(PLAYER_TEAM_BLUE, &game.players, ctf, blue_defend_pressure));
    let red_roles = ctf_snapshot
        .as_ref()
        .map(|ctf| assign_ctf_roles(PLAYER_TEAM_RED, &game.players, ctf, red_defend_pressure));
    let blue_predicted_enemy_spawn =
        predicted_enemy_spawn(PLAYER_TEAM_BLUE, &game.map_spawns, &live_positions, &teams);
    let red_predicted_enemy_spawn =
        predicted_enemy_spawn(PLAYER_TEAM_RED, &game.map_spawns, &live_positions, &teams);
    let blue_enemy_recently_dead = game.players.iter().flatten().any(|other| {
        other.team_id == PLAYER_TEAM_RED
            && other.status == PLAYER_STATUS_DEAD
            && other.time_to_spawn > 0.0
    });
    let red_enemy_recently_dead = game.players.iter().flatten().any(|other| {
        other.team_id == PLAYER_TEAM_BLUE
            && other.status == PLAYER_STATUS_DEAD
            && other.time_to_spawn > 0.0
    });

    for slot in 0..MAX_PLAYER {
        let Some(p) = game.players[slot].as_mut() else {
            continue;
        };
        if !p.is_bot {
            continue;
        }

        if p.status == PLAYER_STATUS_DEAD {
            p.bot_respawn_timer -= dt;
            if p.bot_respawn_timer <= 0.0 {
                respawn_slots.push(slot);
            }
            continue;
        }

        p.bot_fire_cooldown -= dt;
        p.bot_brain.burst_pause = (p.bot_brain.burst_pause - dt).max(0.0);
        p.bot_goal_refresh -= dt;
        p.bot_wander_angle += dt * (1.1 + slot as f32 * 0.013);
        let my_pos = p.cf.position;
        let my_team = p.team_id;
        let assigned_role = if my_team == PLAYER_TEAM_BLUE {
            blue_roles.as_ref().and_then(|roles| roles[slot])
        } else {
            red_roles.as_ref().and_then(|roles| roles[slot])
        }
        .unwrap_or(BotRole::Attacker);
        let coordinated_role_change = matches!(
            assigned_role,
            BotRole::Carrier | BotRole::Escort | BotRole::Recoverer | BotRole::Interceptor
        ) || matches!(
            p.bot_brain.role,
            BotRole::Carrier | BotRole::Escort | BotRole::Recoverer | BotRole::Interceptor
        );
        update_role(&mut p.bot_brain, assigned_role, coordinated_role_change, dt);
        let enemy_recently_dead = if my_team == PLAYER_TEAM_BLUE {
            blue_enemy_recently_dead
        } else {
            red_enemy_recently_dead
        };
        p.bot_brain.spawn_pressure = if enemy_recently_dead {
            1.0
        } else {
            (p.bot_brain.spawn_pressure - dt * 0.35).max(0.0)
        };
        let carrying_enemy_flag = ctf_snapshot
            .as_ref()
            .is_some_and(|ctf| bot_is_flag_carrier(slot, my_team, ctf));
        let escorting_flag_carrier = p.bot_brain.role == BotRole::Escort
            && ctf_snapshot
                .as_ref()
                .is_some_and(|ctf| bot_should_escort_carrier(slot, my_team, ctf));
        let defend_pressure = ctf_snapshot
            .as_ref()
            .is_some_and(|ctf| defender_pressure(my_team, ctf, &live_positions, &teams));
        let predicted_spawn = if my_team == PLAYER_TEAM_BLUE {
            blue_predicted_enemy_spawn
        } else {
            red_predicted_enemy_spawn
        };
        let prev_xy = [my_pos[0], my_pos[1]];

        let mut visible_target: Option<(usize, [f32; 3], f32)> = None;
        let mut heard_target: Option<(usize, [f32; 3], f32)> = None;
        for other in 0..MAX_PLAYER {
            if other == slot {
                continue;
            }
            let Some(pos) = live_positions[other] else {
                continue;
            };
            let Some(tt) = teams[other] else { continue };
            if !bot_can_damage(game_type, my_team, tt) {
                continue;
            }
            let dx = pos[0] - my_pos[0];
            let dy = pos[1] - my_pos[1];
            let d = (dx * dx + dy * dy).sqrt();
            if d > BOT_AIM_RANGE {
                continue;
            }
            if d <= BOT_HEARING_RANGE
                && shot_ages[other].is_some_and(|age| age <= BOT_AUDIBLE_SHOT_AGE)
                && heard_target.map(|(_, _, best)| d < best).unwrap_or(true)
            {
                heard_target = Some((other, pos, d));
            }
            if !bot_can_see(&game.map_grid, my_pos, pos) {
                continue;
            }
            // Keep a visible acquired target unless another enemy is
            // materially closer. This prevents two crossing enemies from
            // resetting reaction/aim every other tick.
            let score = d * if p.bot_brain.perceived_target == Some(other) {
                0.72
            } else {
                1.0
            };
            let beats_current = visible_target
                .map(|(current, _, best_distance)| {
                    let best_score = best_distance
                        * if p.bot_brain.perceived_target == Some(current) {
                            0.72
                        } else {
                            1.0
                        };
                    score < best_score
                })
                .unwrap_or(true);
            if beats_current {
                visible_target = Some((other, pos, d));
            }
        }
        p.bot_brain.perception_elapsed += dt;
        if p.bot_brain.perception_elapsed >= BOT_PERCEPTION_INTERVAL {
            let perception_dt = p.bot_brain.perception_elapsed;
            p.bot_brain.perception_elapsed = 0.0;
            update_perception(
                &mut p.bot_brain,
                visible_target,
                heard_target.map(|(target, position, _)| (target, position)),
                perception_dt,
            );
        }
        // Perception memory may guide investigation, but a firing target must
        // still be visible in this exact simulation tick.
        let combat_target = visible_target.and_then(|(target, _, distance)| {
            (p.bot_brain.perceived_target == Some(target)
                && p.bot_brain.target_visible_for >= BOT_TARGET_ACQUIRE_TIME)
                .then_some((target, distance))
        });
        // Outside CTF objectives, pursue the nearest living enemy even when
        // they are not yet visible. Local wandering can strand opposing teams
        // in separate rooms indefinitely on partitioned maps such as DM-Fort.
        let hunt_target = (0..MAX_PLAYER)
            .filter(|&other| other != slot)
            .filter_map(|other| {
                let position = live_positions[other]?;
                let team = teams[other]?;
                bot_can_damage(game_type, my_team, team).then_some((other, position))
            })
            .min_by(|(_, a), (_, b)| {
                (a[0] - my_pos[0])
                    .hypot(a[1] - my_pos[1])
                    .total_cmp(&(b[0] - my_pos[0]).hypot(b[1] - my_pos[1]))
            });

        let (desired_state, state_reason, urgent, utilities) = desired_tactical_state(
            slot,
            my_team,
            p.life,
            combat_target.is_some(),
            ctf_snapshot.as_ref(),
            p.bot_brain.role,
        );
        p.bot_brain.utilities = utilities;
        update_tactical_state(&mut p.bot_brain, desired_state, state_reason, urgent, dt);

        let stale_goal =
            p.bot_brain.goal_stale_time > BOT_LOCAL_GOAL_STALE_TIME && p.bot_stuck_time > 0.0;
        if p.bot_goal_refresh <= 0.0
            || stale_goal
            || carrying_enemy_flag
            || escorting_flag_carrier
            || p.bot_brain.role == BotRole::Defender
        {
            p.bot_goal = if p.bot_brain.state == BotTacticalState::Retreat {
                if let Some((target_slot, _)) = combat_target {
                    let target = live_positions[target_slot].unwrap();
                    find_cover_goal(&game.map_grid, my_pos, target).unwrap_or([
                        my_pos[0] + my_pos[0] - target[0],
                        my_pos[1] + my_pos[1] - target[1],
                    ])
                } else {
                    wander_goal(p.bot_wander_angle + std::f32::consts::PI, my_pos)
                }
            } else if p.bot_brain.state == BotTacticalState::Attack {
                if p.bot_brain.role == BotRole::Defender {
                    ctf_snapshot
                        .as_ref()
                        .map_or(p.bot_brain.last_known_enemy, |ctf| {
                            let own_flag = if my_team == PLAYER_TEAM_BLUE { 0 } else { 1 };
                            let guard_post = bot_defender_goal(
                                ctf,
                                my_team,
                                slot,
                                p.bot_wander_angle,
                                predicted_spawn,
                                &game.map_grid,
                            );
                            combat_target.map_or(guard_post, |(target_slot, _)| {
                                let target = live_positions[target_slot].unwrap();
                                let pod = ctf.flag_pod[own_flag];
                                let team_plan = if my_team == PLAYER_TEAM_BLUE {
                                    blue_roles.as_ref()
                                } else {
                                    red_roles.as_ref()
                                };
                                let responder = team_plan.and_then(|roles| {
                                    roles
                                        .iter()
                                        .enumerate()
                                        .filter(|(candidate, role)| {
                                            **role == Some(BotRole::Defender)
                                                && live_positions[*candidate].is_some()
                                        })
                                        .min_by(|(a, _), (b, _)| {
                                            let pa = live_positions[*a].unwrap();
                                            let pb = live_positions[*b].unwrap();
                                            (pa[0] - target[0]).hypot(pa[1] - target[1]).total_cmp(
                                                &(pb[0] - target[0]).hypot(pb[1] - target[1]),
                                            )
                                        })
                                        .map(|(candidate, _)| candidate)
                                });
                                if responder == Some(slot)
                                    && (target[0] - pod[0]).hypot(target[1] - pod[1])
                                        <= BOT_DEFEND_PRESSURE_RANGE
                                {
                                    flank_goal(slot, my_pos, target)
                                } else {
                                    guard_post
                                }
                            })
                        })
                } else if let Some((target_slot, _)) = combat_target {
                    flank_goal(slot, my_pos, live_positions[target_slot].unwrap())
                } else {
                    p.bot_brain.last_known_enemy
                }
            } else if p.bot_brain.state == BotTacticalState::Recover {
                let own_flag = if my_team == PLAYER_TEAM_BLUE { 0 } else { 1 };
                let carrier = ctf_snapshot
                    .as_ref()
                    .map(|ctf| ctf.flag_state[own_flag])
                    .unwrap_or(-1);
                if carrier >= 0 && (carrier as usize) < MAX_PLAYER {
                    let carrier = carrier as usize;
                    if let (Some(position), Some(velocity)) =
                        (live_positions[carrier], live_velocities[carrier])
                    {
                        intercept_goal(position, velocity, 1.25)
                    } else {
                        ctf_snapshot.as_ref().map_or(p.bot_goal, |ctf| {
                            [ctf.flag_pos[own_flag][0], ctf.flag_pos[own_flag][1]]
                        })
                    }
                } else {
                    ctf_snapshot.as_ref().map_or(p.bot_goal, |ctf| {
                        [ctf.flag_pos[own_flag][0], ctf.flag_pos[own_flag][1]]
                    })
                }
            } else if let Some(ref ctf) = ctf_snapshot {
                let own_flag = if my_team == PLAYER_TEAM_BLUE { 0 } else { 1 };
                let enemy_flag = 1 - own_flag;
                let spawn_points = enemy_spawn_points(my_team, &game.map_spawns);
                match p.bot_brain.role {
                    BotRole::Carrier => safer_carrier_goal(
                        my_pos,
                        ctf.flag_pod[own_flag],
                        spawn_points,
                        p.bot_brain.spawn_pressure.max(0.25),
                    ),
                    BotRole::Defender => bot_defender_goal(
                        ctf,
                        my_team,
                        slot,
                        p.bot_wander_angle,
                        predicted_spawn,
                        &game.map_grid,
                    ),
                    BotRole::Escort => {
                        let enemy_carrier = ctf.flag_state[enemy_flag];
                        if enemy_carrier >= 0 {
                            escort_carrier_path_goal(
                                slot,
                                ctf.flag_pos[enemy_flag],
                                ctf.flag_pod[own_flag],
                            )
                        } else if ctf.flag_state[own_flag] == FLAG_DROPPED {
                            guard_objective_safe(
                                ctf.flag_pos[own_flag],
                                ctf.flag_pod[own_flag],
                                &game.map_grid,
                            )
                        } else {
                            bot_ctf_goal(
                                slot,
                                my_team,
                                ctf,
                                spawn_points,
                                &game.map_grid,
                                defend_pressure,
                            )
                        }
                    }
                    BotRole::Attacker => [ctf.flag_pod[enemy_flag][0], ctf.flag_pod[enemy_flag][1]],
                    _ => bot_ctf_goal(
                        slot,
                        my_team,
                        ctf,
                        spawn_points,
                        &game.map_grid,
                        defend_pressure,
                    ),
                }
            } else if let Some((ts, _)) = combat_target {
                let tp = live_positions[ts].unwrap();
                [tp[0], tp[1]]
            } else if p.bot_brain.enemy_memory_remaining > 0.0 {
                p.bot_brain.last_known_enemy
            } else if let Some((_, target)) = hunt_target {
                [target[0], target[1]]
            } else {
                wander_goal(p.bot_wander_angle, my_pos)
            };
            p.bot_goal_refresh = BOT_GOAL_REFRESH;
            p.bot_brain.goal_stale_time = 0.0;
            p.bot_brain.last_goal = p.bot_goal;
        } else {
            p.bot_brain.goal_stale_time += dt;
        }

        let navigation_goal = game.map_grid.resolve_navigation_goal(p.bot_goal);
        p.bot_brain.nav_waypoint_time = (p.bot_brain.nav_waypoint_time - dt).max(0.0);
        let nav_goal_change_distance = if p.bot_brain.role == BotRole::Defender {
            0.02
        } else {
            0.75
        };
        let nav_goal_changed = (navigation_goal[0] - p.bot_brain.nav_goal[0])
            .hypot(navigation_goal[1] - p.bot_brain.nav_goal[1])
            > nav_goal_change_distance;
        let reached_waypoint = (p.bot_brain.nav_waypoint[0] - my_pos[0])
            .hypot(p.bot_brain.nav_waypoint[1] - my_pos[1])
            < 0.30;
        let current_nav_tile = [my_pos[0].floor() as i32, my_pos[1].floor() as i32];
        let left_nav_area = current_nav_tile != p.bot_brain.nav_from_tile;
        if nav_goal_changed
            || reached_waypoint
            || left_nav_area
            || p.bot_brain.nav_waypoint_time <= 0.0
        {
            if nav_goal_changed || reached_waypoint || left_nav_area {
                p.bot_stuck_time = 0.0;
            }
            p.bot_brain.nav_goal = navigation_goal;
            p.bot_brain.nav_from_tile = current_nav_tile;
            p.bot_brain.nav_waypoint = game
                .map_grid
                .navigate_toward([my_pos[0], my_pos[1]], navigation_goal);
            p.bot_brain.nav_waypoint_time = BOT_NAV_WAYPOINT_COMMIT_TIME;
            p.bot_brain.nav_best_distance = (p.bot_brain.nav_waypoint[0] - my_pos[0])
                .hypot(p.bot_brain.nav_waypoint[1] - my_pos[1]);
            p.bot_brain.nav_no_progress_time = 0.0;
        }
        let waypoint = p.bot_brain.nav_waypoint;
        let waypoint_distance = (waypoint[0] - my_pos[0]).hypot(waypoint[1] - my_pos[1]);
        let goal_arrival_radius = bot_goal_arrival_radius(p.bot_brain.role, p.bot_brain.state);
        let at_navigation_goal = (p.bot_brain.nav_goal[0] - my_pos[0])
            .hypot(p.bot_brain.nav_goal[1] - my_pos[1])
            < goal_arrival_radius;
        let made_nav_progress = at_navigation_goal
            || waypoint_distance + BOT_NAV_PROGRESS_EPSILON < p.bot_brain.nav_best_distance;
        if made_nav_progress {
            p.bot_brain.nav_best_distance = waypoint_distance;
            p.bot_brain.nav_no_progress_time = 0.0;
        } else {
            p.bot_brain.nav_no_progress_time += dt;
        }
        let nav_stuck = p.bot_brain.nav_no_progress_time >= BOT_NAV_NO_PROGRESS_TIME;
        let mut look_at = navigation_goal;
        if let Some((target_slot, dist)) = combat_target {
            let tpos = live_positions[target_slot].unwrap();
            // Stable, continuously changing error avoids both perfect aim and packet-to-packet jitter.
            let personality_error = BOT_AIM_ERROR * (0.75 + (slot % 7) as f32 * 0.08);
            let error = p.bot_wander_angle.sin() * personality_error * dist;
            let inv = 1.0 / dist.max(0.01);
            let imperfect_target = [
                tpos[0] - (tpos[1] - my_pos[1]) * inv * error,
                tpos[1] + (tpos[0] - my_pos[0]) * inv * error,
            ];
            let aim_target = update_human_aim(
                &mut p.bot_brain,
                [p.cf.mouse[0], p.cf.mouse[1]],
                imperfect_target,
                dt,
                slot,
            );
            let weapon_ready = weapon_utility(
                p.weapon_id,
                dist,
                true,
                p.elapsed_since_last_shot < BOT_FIRE_INTERVAL,
            );
            if dist < 1.0 && p.melee_elapsed >= 1.0 {
                melee_slots.push(slot);
            } else if (1.5..=5.0).contains(&dist)
                && p.molotovs_left > 0
                && p.molotov_elapsed >= 1.0
                && (p.bot_brain.shots_fired + slot as u32) % 6 == 1
            {
                molotov_targets.push((slot, aim_target));
            } else if (2.0..=6.5).contains(&dist)
                && p.grenades_left > 0
                && p.grenade_elapsed >= 1.0
                && (p.bot_brain.shots_fired + slot as u32).is_multiple_of(4)
            {
                grenade_targets.push((slot, aim_target));
            } else if p.weapon_id == 7 && p.bot_fire_cooldown <= 0.0 && weapon_ready > 0.05 {
                let p1 = my_pos;
                let len = dist.max(0.01);
                let p2 = [
                    p1[0] + (tpos[0] - my_pos[0]) / len * 40.0,
                    p1[1] + (tpos[1] - my_pos[1]) / len * 40.0,
                    p1[2],
                ];
                let (wall_end, _) = game.map_grid.ray_test(p1, p2);
                let wall_t = crate::map::segment_hit_t(p1, p2, wall_end);
                let los = crate::map::segment_sphere_hit_t(p1, p2, tpos, PLAYER_RADIUS)
                    .map(|t| t <= wall_t + 1e-3)
                    .unwrap_or(false);
                if los {
                    fire_targets.push((slot, aim_target));
                    p.bot_fire_cooldown = BOT_FIRE_INTERVAL;
                    p.bot_brain.shots_fired += 1;
                } else {
                    p.bot_brain.occluded_shots_rejected += 1;
                }
            } else if p.bot_fire_cooldown <= 0.0
                && weapon_ready > 0.05
                && consume_burst_shot(&mut p.bot_brain, slot)
            {
                let p1 = my_pos;
                let len = dist.max(0.01);
                let p2 = [
                    p1[0] + (tpos[0] - p1[0]) / len * 40.0,
                    p1[1] + (tpos[1] - p1[1]) / len * 40.0,
                    p1[2],
                ];
                let (wall_end, _) = game.map_grid.ray_test(p1, p2);
                let wall_t = crate::map::segment_hit_t(p1, p2, wall_end);
                let los = crate::map::segment_sphere_hit_t(p1, p2, tpos, PLAYER_RADIUS)
                    .map(|t| t <= wall_t + 1e-3)
                    .unwrap_or(false);
                if los {
                    if p.weapon_id == 5 {
                        rocket_targets.push((slot, aim_target));
                    } else {
                        fire_targets.push((slot, aim_target));
                    }
                    p.bot_fire_cooldown = BOT_FIRE_INTERVAL;
                    p.bot_brain.shots_fired += 1;
                } else {
                    p.bot_brain.occluded_shots_rejected += 1;
                }
            }
            look_at = aim_target;
        }

        let mut steer = if at_navigation_goal {
            [my_pos[0], my_pos[1]]
        } else if waypoint_distance < 0.15 {
            game.map_grid
                .navigate_toward([my_pos[0], my_pos[1]], navigation_goal)
        } else {
            waypoint
        };

        if p.bot_brain.recovery_timer > 0.0 {
            p.bot_brain.recovery_timer -= dt;
            let goal_forward = [
                navigation_goal[0] - my_pos[0],
                navigation_goal[1] - my_pos[1],
            ];
            steer = recovery_heading(
                &game.map_grid,
                [my_pos[0], my_pos[1], my_pos[2]],
                navigation_goal,
                goal_forward,
                p.bot_brain.recovery_dir,
                p.bot_brain.recovery_tries,
            );
        }

        if nav_stuck && p.bot_brain.recovery_timer <= 0.0 {
            // Quake III keeps the current reachability until it fails, then
            // temporarily avoids/replans it. Treat lack of waypoint progress
            // as that failure instead of mistaking wall-sliding for success.
            resolve_stuck_bots(&mut p.bot_brain, carrying_enemy_flag);
            p.bot_goal_refresh = 0.0;
            p.bot_brain.goal_stale_time = 0.0;
            p.bot_brain.last_goal = [my_pos[0], my_pos[1]];
            p.bot_brain.nav_waypoint_time = 0.0;
            p.bot_brain.nav_no_progress_time = 0.0;
            p.bot_brain.nav_best_distance = f32::INFINITY;
            p.bot_brain.stuck_events += 1;
            tracing::debug!(
                "bot route stalled: slot={slot} pos=({:.2},{:.2}) waypoint=({:.2},{:.2}) goal=({:.2},{:.2}) try={}",
                my_pos[0],
                my_pos[1],
                waypoint[0],
                waypoint[1],
                navigation_goal[0],
                navigation_goal[1],
                p.bot_brain.recovery_tries,
            );
            let goal_forward = [
                navigation_goal[0] - my_pos[0],
                navigation_goal[1] - my_pos[1],
            ];
            steer = recovery_heading(
                &game.map_grid,
                [my_pos[0], my_pos[1], my_pos[2]],
                navigation_goal,
                goal_forward,
                p.bot_brain.recovery_dir,
                p.bot_brain.recovery_tries,
            );
        }

        if p.bot_stuck_time > BOT_STUCK_TIME && !at_navigation_goal {
            resolve_stuck_bots(&mut p.bot_brain, carrying_enemy_flag);
            p.bot_goal_refresh = 0.0;
            let goal_forward = [
                navigation_goal[0] - my_pos[0],
                navigation_goal[1] - my_pos[1],
            ];
            steer = recovery_heading(
                &game.map_grid,
                [my_pos[0], my_pos[1], my_pos[2]],
                navigation_goal,
                goal_forward,
                p.bot_brain.recovery_dir,
                p.bot_brain.recovery_tries,
            );
            p.bot_stuck_time = 0.0;
            p.bot_brain.stuck_events += 1;
            tracing::debug!(
                "bot movement stalled: slot={slot} pos=({:.2},{:.2}) waypoint=({:.2},{:.2}) goal=({:.2},{:.2}) try={}",
                my_pos[0],
                my_pos[1],
                waypoint[0],
                waypoint[1],
                navigation_goal[0],
                navigation_goal[1],
                p.bot_brain.recovery_tries,
            );
        }

        let mut dx = steer[0] - my_pos[0];
        let mut dy = steer[1] - my_pos[1];
        let separation = separation_force(slot, my_pos, &live_positions);
        let mut separation_weight = BOT_SEPARATION_WEIGHT;
        let closing_on_flag =
            matches!(
                p.bot_brain.state,
                BotTacticalState::AttackObjective | BotTacticalState::Recover
            ) && (my_pos[0] - navigation_goal[0]).hypot(my_pos[1] - navigation_goal[1]) <= 1.0;
        if closing_on_flag {
            // Player bodies do not block movement. Once an objective runner
            // reaches the flag tile, teammate separation only makes them
            // hesitate around the 0.25-unit take/return trigger.
            separation_weight = 0.0;
        }
        if carrying_enemy_flag {
            let own_flag = if my_team == PLAYER_TEAM_BLUE { 0 } else { 1 };
            let near_scoring_pod = ctf_snapshot.as_ref().is_some_and(|ctf| {
                let pod = ctf.flag_pod[own_flag];
                (my_pos[0] - pod[0]).hypot(my_pos[1] - pod[1])
                    <= BOT_CARRIER_DIRECT_CAPTURE_DISTANCE
            });
            // Player bodies do not block server movement. Let the carrier own
            // the final scoring lane instead of steering away from teammates.
            separation_weight = if near_scoring_pod { 0.0 } else { 1.8 };
        }
        dx += separation[0] * separation_weight;
        dy += separation[1] * separation_weight;
        if combat_target.is_some() && !carrying_enemy_flag && !escorting_flag_carrier {
            // Orbit in alternating arcs instead of forming a straight charging line.
            let orbit = if ((p.bot_wander_angle / 3.0) as i32) & 1 == 0 {
                0.75
            } else {
                -0.75
            };
            let base_len = dx.hypot(dy).max(0.05);
            let (forward_x, forward_y) = (dx, dy);
            dx += -forward_y / base_len * orbit;
            dy += forward_x / base_len * orbit;
        }
        let len = (dx * dx + dy * dy).sqrt().max(0.05);
        let vel = [(dx / len) * BOT_SPEED, (dy / len) * BOT_SPEED, 0.0];
        let mut pos = [my_pos[0] + vel[0] * dt, my_pos[1] + vel[1] * dt, my_pos[2]];
        let mut vel = vel;
        clip_entity_xy(&game.map_grid, &mut pos, PLAYER_RADIUS);
        clip_player_motion_xy(slot, &live_positions, my_pos, &mut pos, &mut vel);

        let moved_dist = (pos[0] - prev_xy[0]).hypot(pos[1] - prev_xy[1]);
        if moved_dist < BOT_STUCK_THRESHOLD || !made_nav_progress {
            p.bot_stuck_time += dt;
        } else {
            p.bot_brain.recovery_tries = 0;
            p.bot_stuck_time = 0.0;
        }
        moves.push((
            slot,
            pos,
            vel,
            [look_at[0], look_at[1], 0.25],
            p.cf.frame_id + 1,
        ));
    }

    for (slot, pos, vel, mouse, frame_id) in moves {
        if let Some(p) = game.players[slot].as_mut() {
            p.cf.position = pos;
            p.cf.vel = vel;
            p.cf.mouse = mouse;
            p.cf.frame_id = frame_id;
        }
    }

    for slot in respawn_slots {
        game.respawn_bot(slot);
    }
    for (slot, target) in fire_targets {
        crate::game::shoot::bot_fire(game, slot, target);
    }
    for (slot, target) in rocket_targets {
        crate::game::projectile::bot_fire_rocket(game, slot, target);
    }
    for (slot, target) in grenade_targets {
        crate::game::projectile::bot_throw_grenade(game, slot, target);
    }
    for (slot, target) in molotov_targets {
        crate::game::projectile::bot_throw_molotov(game, slot, target);
    }
    for slot in melee_slots {
        crate::game::shoot::bot_melee(game, slot);
    }
}

fn bot_goal_arrival_radius(role: BotRole, state: BotTacticalState) -> f32 {
    // All flag interactions use a 0.25-unit center radius. Stopping at the
    // generic 0.40 navigation tolerance made attackers and recoverers wait
    // just outside take/return range. Defenders also use a tight tolerance so
    // their patrol never becomes a static post.
    if role == BotRole::Defender
        || matches!(
            state,
            BotTacticalState::AttackObjective
                | BotTacticalState::Capture
                | BotTacticalState::Recover
        )
    {
        0.12
    } else {
        0.40
    }
}

fn bot_is_flag_carrier(slot: usize, team: i8, ctf: &CtfState) -> bool {
    let enemy_flag = if team == PLAYER_TEAM_BLUE { 1 } else { 0 };
    ctf.flag_state[enemy_flag] == slot as i8
}

fn bot_should_escort_carrier(slot: usize, team: i8, ctf: &CtfState) -> bool {
    let enemy_flag = if team == PLAYER_TEAM_BLUE { 1 } else { 0 };
    let own_flag = if team == PLAYER_TEAM_BLUE { 0 } else { 1 };
    ctf.flag_state[own_flag] == FLAG_AT_POD
        && ctf.flag_state[enemy_flag] >= 0
        && ctf.flag_state[enemy_flag] != slot as i8
}

fn bot_can_see(grid: &crate::map::MapGrid, from: [f32; 3], target: [f32; 3]) -> bool {
    let tx = target[0] - from[0];
    let ty = target[1] - from[1];
    let distance = tx.hypot(ty);
    if distance <= 0.001 || distance > BOT_AIM_RANGE {
        return false;
    }
    // BV2 is top-down: facing controls the gun, not the player's camera.
    // Anything within the screen-scale range and unobstructed by a wall can
    // be noticed, including an enemy behind a retreating bot.
    let (wall, _) = grid.ray_test(from, target);
    let wall_distance = (wall[0] - from[0]).hypot(wall[1] - from[1]);
    wall_distance + 1e-3 >= distance
}

fn bot_defender_goal(
    ctf: &CtfState,
    team: i8,
    slot: usize,
    angle: f32,
    predicted_spawn: Option<[f32; 2]>,
    grid: &crate::map::MapGrid,
) -> [f32; 2] {
    let own_flag = if team == PLAYER_TEAM_BLUE { 0 } else { 1 };
    let enemy_flag = 1 - own_flag;
    let own_pod = ctf.flag_pod[own_flag];
    let enemy_pod = ctf.flag_pod[enemy_flag];
    let approach = predicted_spawn.unwrap_or([enemy_pod[0], enemy_pod[1]]);
    let lane_x = approach[0] - own_pod[0];
    let lane_y = approach[1] - own_pod[1];
    let lane_len = (lane_x * lane_x + lane_y * lane_y).sqrt().max(0.01);
    let forward = [lane_x / lane_len, lane_y / lane_len];
    let lateral = [-forward[1], forward[0]];

    // Defenders guard both shoulders of the predicted spawn approach. There
    // is intentionally no center post: that is the carrier's scoring lane.
    let team_rank = slot / 2;
    let side = if team_rank & 1 == 0 { 1.0 } else { -1.0 };
    let lateral_lane = side * (1.35 + (team_rank / 2) as f32 * 0.3);
    let advance = 1.25 + (team_rank % 2) as f32 * 0.35;
    let station = [
        own_pod[0] + forward[0] * advance + lateral[0] * lateral_lane,
        own_pod[1] + forward[1] * advance + lateral[1] * lateral_lane,
    ];
    // A small elliptical loop keeps defenders alive and scanning without
    // letting them drift into midfield or camp motionless on the flag pole.
    let phase = angle * 0.85 + team_rank as f32 * 1.7;
    let patrol_forward = phase.cos() * 0.48;
    let patrol_lateral = phase.sin() * 0.68;
    grid.resolve_navigation_goal([
        station[0] + forward[0] * patrol_forward + lateral[0] * patrol_lateral,
        station[1] + forward[1] * patrol_forward + lateral[1] * patrol_lateral,
    ])
}

fn guard_objective_safe(
    point: [f32; 3],
    fallback: [f32; 3],
    grid: &crate::map::MapGrid,
) -> [f32; 2] {
    if point[0].is_finite() && point[1].is_finite() && flag_is_visible(grid, point) {
        [point[0], point[1]]
    } else {
        [fallback[0], fallback[1]]
    }
}

fn separation_force(
    slot: usize,
    position: [f32; 3],
    live_positions: &[Option<[f32; 3]>],
) -> [f32; 2] {
    let mut force = [0.0, 0.0];
    for (other, candidate) in live_positions.iter().enumerate() {
        if other == slot {
            continue;
        }
        let Some(other_pos) = candidate else { continue };
        let dx = position[0] - other_pos[0];
        let dy = position[1] - other_pos[1];
        let distance = dx.hypot(dy);
        if distance >= BOT_SEPARATION_RADIUS {
            continue;
        }
        let strength = (BOT_SEPARATION_RADIUS - distance) / BOT_SEPARATION_RADIUS;
        if distance > 0.001 {
            force[0] += dx / distance * strength;
            force[1] += dy / distance * strength;
        } else {
            // Stable opposite headings break perfectly coincident bot spawns.
            let angle = ((slot * 37 + other * 17) as f32).to_radians();
            force[0] += angle.cos() * strength;
            force[1] += angle.sin() * strength;
        }
    }
    force
}

fn wander_goal(angle: f32, my_pos: [f32; 3]) -> [f32; 2] {
    [my_pos[0] + angle.cos() * 6.0, my_pos[1] + angle.sin() * 6.0]
}

/// CTF objective priority: cap > return own flag > pick up enemy flag > attack carrier.
fn bot_ctf_goal(
    slot: usize,
    team: i8,
    ctf: &CtfState,
    spawn_points: &[[f32; 3]],
    grid: &crate::map::MapGrid,
    defend_pressure: bool,
) -> [f32; 2] {
    let enemy_flag = if team == PLAYER_TEAM_BLUE { 1 } else { 0 };
    let own_flag = if team == PLAYER_TEAM_BLUE { 0 } else { 1 };
    let own_pod = ctf.flag_pod[own_flag];
    let enemy_pod = ctf.flag_pod[enemy_flag];

    if ctf.flag_state[enemy_flag] == slot as i8 {
        return guard_objective_safe(own_pod, enemy_pod, grid);
    }

    if defend_pressure {
        return pressure_spawn_goal(
            spawn_points,
            ctf.flag_pos[enemy_flag],
            ctf.flag_pod[own_flag],
        );
    }

    if ctf.flag_state[own_flag] == FLAG_AT_POD && ctf.flag_state[enemy_flag] >= 0 {
        return escort_carrier_path_goal(slot, ctf.flag_pos[enemy_flag], own_pod);
    }

    if ctf.flag_state[own_flag] == FLAG_DROPPED {
        return guard_objective_safe(ctf.flag_pos[own_flag], own_pod, grid);
    }

    if ctf.flag_state[enemy_flag] == FLAG_DROPPED {
        return guard_objective_safe(ctf.flag_pos[enemy_flag], enemy_pod, grid);
    }

    if ctf.flag_state[enemy_flag] == FLAG_AT_POD {
        return guard_objective_safe(enemy_pod, own_pod, grid);
    }

    if ctf.flag_state[own_flag] >= 0 {
        let carrier = ctf.flag_state[own_flag] as usize;
        if carrier < MAX_PLAYER {
            return guard_objective_safe(ctf.flag_pos[own_flag], enemy_pod, grid);
        }
    }

    [enemy_pod[0], enemy_pod[1]]
}

#[cfg(test)]
fn escort_carrier_goal(slot: usize, carrier: [f32; 3], scoring_pod: [f32; 3]) -> [f32; 2] {
    let dx = scoring_pod[0] - carrier[0];
    let dy = scoring_pod[1] - carrier[1];
    let distance = dx.hypot(dy);
    if distance <= 0.25 {
        return [scoring_pod[0], scoring_pod[1]];
    }
    let formation = slot % 6;
    let advance = distance.min(1.5 + (formation / 2) as f32 * 1.25);
    let nx = dx / distance;
    let ny = dy / distance;
    let lateral =
        if formation & 1 == 0 { 1.0 } else { -1.0 } * (1.2 + (formation / 2) as f32 * 0.65);
    [
        carrier[0] + nx * advance - ny * lateral,
        carrier[1] + ny * advance + nx * lateral,
    ]
}

fn escort_carrier_path_goal(slot: usize, carrier: [f32; 3], scoring_pod: [f32; 3]) -> [f32; 2] {
    let dx = scoring_pod[0] - carrier[0];
    let dy = scoring_pod[1] - carrier[1];
    let distance = dx.hypot(dy).max(0.01);
    let nx = dx / distance;
    let ny = dy / distance;
    let formation = slot % 4;
    // Escorts clear the shoulders ahead of the carrier and never occupy the
    // central scoring point. Stop short of the pod as the carrier arrives.
    let lane_offset = 0.9 + (formation / 2) as f32 * 0.45;
    let sign = if formation.is_multiple_of(2) {
        1.0
    } else {
        -1.0
    };
    let lead = (distance * 0.70).min(2.8).min((distance - 0.55).max(0.0));
    [
        carrier[0] + nx * lead - ny * sign * lane_offset,
        carrier[1] + ny * lead + nx * sign * lane_offset,
    ]
}

fn bot_can_damage(game_type: i8, shooter_team: i8, target_team: i8) -> bool {
    if matches!(game_type, 0 | crate::game::GAME_TYPE_SND) {
        return true;
    }
    shooter_team != target_team
        && (shooter_team == PLAYER_TEAM_BLUE || shooter_team == PLAYER_TEAM_RED)
        && (target_team == PLAYER_TEAM_BLUE || target_team == PLAYER_TEAM_RED)
}

fn bot_primary_weapon(roster_index: usize) -> i8 {
    (roster_index % 8) as i8
}

impl GameServer {
    pub fn remove_bot(&mut self, slot: usize) {
        if !self
            .players
            .get(slot)
            .and_then(Option::as_ref)
            .is_some_and(|player| player.is_bot)
        {
            return;
        }
        crate::game::ctf::drop_carrier_flags(self, slot);
        self.players[slot] = None;
        self.broadcast(packet::player_disconnect(slot as i8));
    }

    pub fn add_bot(&mut self, slot: usize, team_id: i8, roster_index: usize) {
        let babonet_id = u32::MAX - slot as u32;
        let mut p = Player::new(slot, babonet_id, "127.0.0.1".into());
        p.is_bot = true;
        p.bot_identity = Some(BotIdentity {
            name: "bv2-server AI".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            author: "BaboViolent community".into(),
        });
        p.name = bot_name(roster_index);
        p.handshake_done = true;
        p.team_id = team_id;
        p.weapon_id = bot_primary_weapon(roster_index);
        p.weapon_current_imp = crate::game::shoot::weapon_start_spread(p.weapon_id);
        p.melee_id = 10;
        p.bot_respawn_timer = BOT_RESPAWN_DELAY;
        p.skin = bot_skin(roster_index);
        let (rd, gd, bd) = bot_decals(roster_index);
        p.red_decal = rd;
        p.green_decal = gd;
        p.blue_decal = bd;

        // Bots are server-owned players, so use the same native GameSpawn.cpp
        // maximin rule as humans: maximize distance from the nearest live enemy.
        let pos = self.pick_spawn_for(team_id, Some(slot));
        p.cf.position = pos;
        p.cf.mouse = [pos[0] + 1.0, pos[1], 0.25];
        p.bot_goal = self
            .map_grid
            .unstuck_heading([pos[0], pos[1]], [pos[0] + 2.0, pos[1] + 0.1]);
        p.bot_brain.nav_goal = [f32::INFINITY; 2];
        p.bot_brain.nav_from_tile = [i32::MIN; 2];
        p.bot_brain.nav_waypoint_time = 0.0;
        p.bot_brain.nav_best_distance = f32::INFINITY;
        p.bot_brain.nav_no_progress_time = 0.0;
        p.status = PLAYER_STATUS_ALIVE;
        p.life = 1.0;
        p.spawned_once = true;
        p.spawn_immunity_remaining = crate::game::SPAWN_IMMUNITY_TIME;

        self.players[slot] = Some(p);

        let bp = self.players[slot].as_ref().unwrap();
        tracing::debug!(
            "bot spawned: slot={slot} name={} team={team_id} skin={} pos=({:.1},{:.1})",
            bp.name,
            std::str::from_utf8(&bp.skin)
                .unwrap_or("?")
                .trim_end_matches('\0'),
            pos[0],
            pos[1]
        );

        self.broadcast(packet::newplayer(slot as i8, babonet_id as i32));

        let spawn_body = player_spawn_bytes(slot, bp);
        let enum_body = bp.enum_state_bytes();
        self.broadcast(packet::encode_frame(
            ids::NET_SVCL_PLAYER_SPAWN,
            &spawn_body,
        ));
        self.broadcast(packet::encode_frame(
            ids::NET_SVCL_PLAYER_ENUM_STATE,
            &enum_body,
        ));

        let mut info = vec![0u8; 48];
        info[0] = slot as u8;
        crate::game::write_name31(&mut info[1..33], &bp.name);
        self.broadcast(packet::encode_frame(ids::NET_CLSV_SVCL_PLAYER_INFO, &info));
    }

    pub fn respawn_bot(&mut self, slot: usize) {
        let team = self
            .players
            .get(slot)
            .and_then(|p| p.as_ref())
            .map(|p| p.team_id)
            .unwrap_or(PLAYER_TEAM_BLUE);
        if !self.players[slot].as_ref().is_some_and(|p| p.is_bot) {
            return;
        }
        let pos = self.pick_spawn_for(team, Some(slot));

        let Some(p) = self.players[slot].as_mut() else {
            return;
        };
        p.cf.position = pos;
        p.cf.vel = [0.0, 0.0, 0.0];
        p.cf.mouse = [pos[0] + 1.0, pos[1], 0.25];
        p.bot_goal = self
            .map_grid
            .unstuck_heading([pos[0], pos[1]], [pos[0] + 2.0, pos[1] + 0.1]);
        p.bot_brain.nav_goal = [f32::INFINITY; 2];
        p.bot_brain.nav_from_tile = [i32::MIN; 2];
        p.bot_brain.nav_waypoint_time = 0.0;
        p.bot_brain.nav_best_distance = f32::INFINITY;
        p.bot_brain.nav_no_progress_time = 0.0;
        p.bot_stuck_time = 0.0;
        p.status = PLAYER_STATUS_ALIVE;
        p.life = 1.0;
        p.bot_respawn_timer = BOT_RESPAWN_DELAY;
        p.spawned_once = true;
        p.grenades_left = 2;
        p.molotovs_left = 1;
        p.grenade_elapsed = f32::INFINITY;
        p.molotov_elapsed = f32::INFINITY;
        p.melee_elapsed = f32::INFINITY;
        p.elapsed_since_last_shot = f32::INFINITY;
        p.weapon_current_imp = crate::game::shoot::weapon_start_spread(p.weapon_id);
        p.spawn_immunity_remaining = crate::game::SPAWN_IMMUNITY_TIME;

        tracing::debug!("bot respawn: slot={slot} pos=({:.1},{:.1})", pos[0], pos[1]);

        let spawn_body = player_spawn_bytes(slot, self.players[slot].as_ref().unwrap());
        let enum_body = self.players[slot].as_ref().unwrap().enum_state_bytes();
        self.broadcast(packet::encode_frame(
            ids::NET_SVCL_PLAYER_SPAWN,
            &spawn_body,
        ));
        self.broadcast(packet::encode_frame(
            ids::NET_SVCL_PLAYER_ENUM_STATE,
            &enum_body,
        ));
    }
}

/// Treat BV2_BOTS as a target population: every connected human replaces one
/// bot until no bots remain. Only the necessary bots leave or join so clients
/// receive the same single-player roster events they would for humans.
pub fn reconcile_bot_roster(game: &mut GameServer) {
    if game.config.bot_count == 0 {
        return;
    }
    let max = game.config.max_players.min(MAX_PLAYER);
    let (human_blue, human_red, human_count) = game
        .players
        .iter()
        .flatten()
        .filter(|player| !player.is_bot)
        .fold((0usize, 0usize, 0usize), |(blue, red, count), player| {
            (
                blue + usize::from(player.team_id == PLAYER_TEAM_BLUE),
                red + usize::from(player.team_id == PLAYER_TEAM_RED),
                count + 1,
            )
        });
    let capacity = game
        .config
        .bot_count
        .saturating_sub(human_count)
        .min(max.saturating_sub(human_count));
    let (mut desired_blue, mut desired_red) = (0usize, 0usize);
    while desired_blue + desired_red < capacity {
        let blue_total = human_blue + desired_blue;
        let red_total = human_red + desired_red;
        if blue_total <= red_total {
            desired_blue += 1;
        } else {
            desired_red += 1;
        }
    }

    for (team, desired) in [
        (PLAYER_TEAM_BLUE, desired_blue),
        (PLAYER_TEAM_RED, desired_red),
    ] {
        let slots: Vec<_> = (0..max)
            .filter(|&slot| {
                game.players[slot]
                    .as_ref()
                    .is_some_and(|player| player.is_bot && player.team_id == team)
            })
            .collect();
        for slot in slots.into_iter().skip(desired) {
            game.remove_bot(slot);
        }
    }

    let current_blue = game
        .players
        .iter()
        .flatten()
        .filter(|player| player.is_bot && player.team_id == PLAYER_TEAM_BLUE)
        .count();
    let current_red = game
        .players
        .iter()
        .flatten()
        .filter(|player| player.is_bot && player.team_id == PLAYER_TEAM_RED)
        .count();
    let teams = std::iter::repeat_n(PLAYER_TEAM_BLUE, desired_blue - current_blue).chain(
        std::iter::repeat_n(PLAYER_TEAM_RED, desired_red - current_red),
    );
    for team in teams {
        let Some(slot) = (0..max).find(|&slot| game.players[slot].is_none()) else {
            break;
        };
        game.add_bot(slot, team, slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_replaces_one_bot_and_keeps_six_vs_six() {
        let mut game = GameServer::new(crate::config::ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 16,
            bot_count: 12,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        for slot in 0..MAX_PLAYER {
            if game.players[slot]
                .as_ref()
                .is_some_and(|player| player.is_bot)
            {
                game.players[slot] = None;
            }
        }
        let mut human = Player::new(0, 10, "human".into());
        human.team_id = PLAYER_TEAM_BLUE;
        game.players[0] = Some(human);

        reconcile_bot_roster(&mut game);

        let blue = game
            .players
            .iter()
            .flatten()
            .filter(|p| p.team_id == PLAYER_TEAM_BLUE)
            .count();
        let red = game
            .players
            .iter()
            .flatten()
            .filter(|p| p.team_id == PLAYER_TEAM_RED)
            .count();
        let bots = game.players.iter().flatten().filter(|p| p.is_bot).count();
        let occupied = game.players.iter().filter(|p| p.is_some()).count();
        assert_eq!(blue, red);
        assert_eq!(blue, 6);
        assert_eq!(red, 6);
        assert_eq!(bots, 11);
        assert_eq!(occupied, 12);
        assert!(game
            .players
            .iter()
            .flatten()
            .filter(|p| p.is_bot)
            .all(|p| p.name.ends_with(" [bot]")));
    }

    #[test]
    fn sixteen_humans_displace_every_bot() {
        let mut game = GameServer::new(crate::config::ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 16,
            bot_count: 12,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        game.players = std::array::from_fn(|slot| {
            (slot < 16).then(|| {
                let mut human = Player::new(slot, slot as u32 + 1, "human".into());
                human.team_id = if slot % 2 == 0 {
                    PLAYER_TEAM_BLUE
                } else {
                    PLAYER_TEAM_RED
                };
                human
            })
        });

        reconcile_bot_roster(&mut game);

        assert_eq!(
            game.players.iter().flatten().filter(|p| p.is_bot).count(),
            0
        );
        assert_eq!(game.players.iter().flatten().count(), 16);
    }

    #[test]
    fn departing_human_is_replaced_until_target_population_is_restored() {
        let mut game = GameServer::new(crate::config::ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 16,
            bot_count: 12,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        game.players[0] = None;
        let mut human = Player::new(0, 99, "human".into());
        human.team_id = PLAYER_TEAM_BLUE;
        game.players[0] = Some(human);
        reconcile_bot_roster(&mut game);
        assert_eq!(
            game.players.iter().flatten().filter(|p| p.is_bot).count(),
            11
        );

        game.players[0] = None;
        reconcile_bot_roster(&mut game);

        assert_eq!(
            game.players.iter().flatten().filter(|p| p.is_bot).count(),
            12
        );
        assert_eq!(game.players.iter().flatten().count(), 12);
    }

    #[test]
    fn roster_changes_preserve_unaffected_bots() {
        let mut game = GameServer::new(crate::config::ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 16,
            bot_count: 12,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        let before: Vec<_> = game
            .players
            .iter()
            .enumerate()
            .filter_map(|(slot, player)| player.as_ref().map(|p| (slot, p.babonet_id)))
            .collect();
        game.players[12] = Some(Player::new(12, 42, "human".into()));

        reconcile_bot_roster(&mut game);

        let retained = before
            .iter()
            .filter(|(slot, babonet)| {
                game.players[*slot]
                    .as_ref()
                    .is_some_and(|player| player.babonet_id == *babonet)
            })
            .count();
        assert_eq!(retained, 11, "exactly one bot should leave for one human");
        assert_eq!(
            game.players.iter().flatten().filter(|p| p.is_bot).count(),
            11
        );
    }

    fn role_players(blue: usize, red: usize) -> [Option<Player>; MAX_PLAYER] {
        let mut players: [Option<Player>; MAX_PLAYER] = std::array::from_fn(|_| None);
        for (slot, entry) in players.iter_mut().enumerate().take(blue + red) {
            let mut player = Player::new(slot, slot as u32 + 1, "bot".into());
            player.is_bot = true;
            player.status = PLAYER_STATUS_ALIVE;
            player.team_id = if slot < blue {
                PLAYER_TEAM_BLUE
            } else {
                PLAYER_TEAM_RED
            };
            *entry = Some(player);
        }
        players
    }

    #[test]
    fn bots_use_the_human_respawn_delay() {
        assert_eq!(BOT_RESPAWN_DELAY, SV_TIME_TO_SPAWN);
    }

    #[test]
    fn bot_respawn_maximizes_distance_from_the_nearest_live_enemy() {
        let mut game = GameServer::new(crate::config::ServerConfig {
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
        game.players = std::array::from_fn(|_| None);
        game.map_spawns.dm = vec![[0.0, 0.0, 0.25], [10.0, 0.0, 0.25]];

        let mut bot = Player::new(0, u32::MAX, "bot".into());
        bot.is_bot = true;
        bot.team_id = PLAYER_TEAM_RED;
        bot.status = PLAYER_STATUS_DEAD;
        game.players[0] = Some(bot);

        for (slot, x) in [(1, 0.1), (2, 100.0)] {
            let mut enemy = Player::new(slot, slot as u32, "enemy".into());
            enemy.team_id = PLAYER_TEAM_BLUE;
            enemy.status = PLAYER_STATUS_ALIVE;
            enemy.cf.position = [x, 0.0, 0.25];
            game.players[slot] = Some(enemy);
        }

        game.respawn_bot(0);

        assert_eq!(
            game.players[0].as_ref().unwrap().cf.position,
            [10.0, 0.0, 0.25]
        );
    }

    #[test]
    fn varied_bot_loadouts_start_at_their_native_bloom() {
        for roster in 0..8 {
            let weapon = bot_primary_weapon(roster);
            let mut player = Player::new(roster, roster as u32, "bot".into());
            player.weapon_id = weapon;
            player.weapon_current_imp = crate::game::shoot::weapon_start_spread(weapon);
            assert_eq!(
                player.weapon_current_imp,
                crate::game::shoot::weapon_start_spread(player.weapon_id)
            );
        }
    }

    #[test]
    fn nearby_bots_steer_apart() {
        let positions = vec![Some([2.0, 2.0, 0.25]), Some([2.4, 2.0, 0.25])];
        assert!(separation_force(0, positions[0].unwrap(), &positions)[0] < 0.0);
        assert!(separation_force(1, positions[1].unwrap(), &positions)[0] > 0.0);
    }

    #[test]
    fn coincident_bots_receive_nonzero_deterministic_separation() {
        let positions = vec![Some([2.0, 2.0, 0.25]), Some([2.0, 2.0, 0.25])];
        let force = separation_force(0, positions[0].unwrap(), &positions);
        assert!(force[0].hypot(force[1]) > 0.5);
        assert_eq!(
            force,
            separation_force(0, positions[0].unwrap(), &positions)
        );
    }

    #[test]
    fn flag_carrier_prioritizes_own_pod() {
        let mut ctf = CtfState::new([[2.0, 3.0, 0.25], [18.0, 3.0, 0.25]]);
        ctf.flag_state[1] = 4;
        let grid = crate::map::MapGrid::fixture(32, &[]);
        assert!(bot_is_flag_carrier(4, PLAYER_TEAM_BLUE, &ctf));
        assert_eq!(
            bot_ctf_goal(
                4,
                PLAYER_TEAM_BLUE,
                &ctf,
                &[[0.0, 0.0, 0.25], [32.0, 0.0, 0.25],],
                &grid,
                true
            ),
            [2.0, 3.0]
        );
        assert!(!bot_is_flag_carrier(3, PLAYER_TEAM_BLUE, &ctf));
    }

    #[test]
    fn teammates_screen_a_carrier_toward_the_scoring_pod() {
        let mut ctf = CtfState::new([[2.0, 3.0, 0.25], [18.0, 3.0, 0.25]]);
        let grid = crate::map::MapGrid::fixture(32, &[]);
        ctf.flag_state[1] = 4;
        ctf.flag_pos[1] = [8.0, 3.0, 0.25];
        let goal = bot_ctf_goal(
            2,
            PLAYER_TEAM_BLUE,
            &ctf,
            &[[0.0, 0.0, 0.25], [32.0, 0.0, 0.25]],
            &grid,
            false,
        );
        assert!(
            goal[0] < ctf.flag_pos[1][0],
            "escort screens ahead toward home"
        );
        assert!(
            goal[0] > ctf.flag_pod[0][0],
            "escort stays with the carrier route"
        );
    }

    #[test]
    fn carrier_escorts_get_distinct_lanes_and_depths() {
        let carrier = [8.0, 3.0, 0.25];
        let pod = [2.0, 3.0, 0.25];
        let goals: Vec<_> = (0..6)
            .map(|slot| escort_carrier_goal(slot, carrier, pod))
            .collect();
        for a in 0..goals.len() {
            for b in a + 1..goals.len() {
                assert!(goals[a][0] != goals[b][0] || goals[a][1] != goals[b][1]);
            }
        }
        assert!(goals.iter().any(|goal| goal[1] < carrier[1]));
        assert!(goals.iter().any(|goal| goal[1] > carrier[1]));
    }

    #[test]
    fn coordinator_covers_home_allied_carry_and_imminent_capture() {
        let players = role_players(5, 3);
        let mut ctf = CtfState::new([[2.0, 3.0, 0.25], [18.0, 3.0, 0.25]]);
        let home = assign_ctf_roles(PLAYER_TEAM_BLUE, &players, &ctf, false);
        assert_eq!(
            home.iter()
                .filter(|role| **role == Some(BotRole::Defender))
                .count(),
            2
        );
        assert_eq!(
            home.iter()
                .filter(|role| **role == Some(BotRole::Attacker))
                .count(),
            3
        );

        ctf.flag_state[1] = 4;
        ctf.flag_pos[1] = [2.2, 3.0, 0.25];
        let carry = assign_ctf_roles(PLAYER_TEAM_BLUE, &players, &ctf, false);
        assert_eq!(carry[4], Some(BotRole::Carrier));
        assert_eq!(
            carry
                .iter()
                .filter(|role| **role == Some(BotRole::Escort))
                .count(),
            2
        );
        assert_eq!(
            carry
                .iter()
                .filter(|role| **role == Some(BotRole::Defender))
                .count(),
            2
        );
        assert_eq!(
            escort_carrier_goal(0, ctf.flag_pos[1], ctf.flag_pod[0]),
            [2.0, 3.0]
        );
    }

    #[test]
    fn coordinator_assigns_backup_when_natural_defenders_are_dead() {
        let mut players = role_players(6, 2);
        players[0].as_mut().unwrap().status = PLAYER_STATUS_DEAD;
        players[1].as_mut().unwrap().status = PLAYER_STATUS_DEAD;
        let ctf = CtfState::new([[2.0, 3.0, 0.25], [18.0, 3.0, 0.25]]);
        let roles = assign_ctf_roles(PLAYER_TEAM_BLUE, &players, &ctf, false);
        assert_eq!(roles[2], Some(BotRole::Defender));
        assert!(roles[3..6].contains(&Some(BotRole::Attacker)));
    }

    #[test]
    fn defensive_personalities_patrol_their_own_base_not_midfield() {
        let ctf = CtfState::new([[2.0, 3.0, 0.25], [18.0, 3.0, 0.25]]);
        let grid = crate::map::MapGrid::fixture(24, &[]);
        for slot in 0..4 {
            let goal = bot_defender_goal(
                &ctf,
                PLAYER_TEAM_BLUE,
                slot,
                slot as f32,
                Some([18.0, 3.0]),
                &grid,
            );
            let home_distance = (goal[0] - 2.0).hypot(goal[1] - 3.0);
            let enemy_distance = (goal[0] - 18.0).hypot(goal[1] - 3.0);
            assert!(home_distance < 4.0, "defender {slot} wandered to {goal:?}");
            assert!(home_distance < enemy_distance);
        }
    }

    #[test]
    fn coordinator_covers_enemy_carry_both_flags_out_and_dropped_flag() {
        let players = role_players(6, 4);
        let mut ctf = CtfState::new([[2.0, 3.0, 0.25], [18.0, 3.0, 0.25]]);
        ctf.flag_state[0] = 7;
        let stolen = assign_ctf_roles(PLAYER_TEAM_BLUE, &players, &ctf, false);
        assert_eq!(
            stolen
                .iter()
                .filter(|role| **role == Some(BotRole::Interceptor))
                .count(),
            2
        );

        ctf.flag_state[1] = 5;
        let both_out = assign_ctf_roles(PLAYER_TEAM_BLUE, &players, &ctf, true);
        assert_eq!(both_out[5], Some(BotRole::Carrier));
        assert!(both_out.contains(&Some(BotRole::Escort)));
        assert!(both_out.contains(&Some(BotRole::Interceptor)));
        assert!(both_out.contains(&Some(BotRole::Defender)));

        ctf.flag_state[0] = FLAG_DROPPED;
        ctf.flag_state[1] = FLAG_AT_POD;
        let dropped = assign_ctf_roles(PLAYER_TEAM_BLUE, &players, &ctf, true);
        assert_eq!(
            dropped
                .iter()
                .filter(|role| **role == Some(BotRole::Recoverer))
                .count(),
            2
        );
    }

    #[test]
    fn carrier_pushes_split_between_escorts_and_recovering_flag() {
        let players = role_players(5, 3);
        let mut ctf = CtfState::new([[2.0, 3.0, 0.25], [18.0, 3.0, 0.25]]);
        ctf.flag_state[1] = 2;
        ctf.flag_state[0] = FLAG_DROPPED;
        let roles = assign_ctf_roles(PLAYER_TEAM_BLUE, &players, &ctf, false);
        assert_eq!(roles[2], Some(BotRole::Carrier));
        assert!(roles.contains(&Some(BotRole::Escort)));
        assert!(
            roles
                .iter()
                .filter(|role| **role == Some(BotRole::Recoverer))
                .count()
                >= 1
        );
    }

    #[test]
    fn role_hysteresis_waits_but_material_flag_events_interrupt() {
        let mut brain = BotBrain {
            role_committed_for: 0.0,
            ..Default::default()
        };
        update_role(&mut brain, BotRole::Defender, false, 0.2);
        assert_eq!(brain.role, BotRole::Attacker);
        update_role(&mut brain, BotRole::Interceptor, true, 0.02);
        assert_eq!(brain.role, BotRole::Interceptor);
    }

    #[test]
    fn spawn_pressure_uses_legal_sets_and_safer_route_with_fallback() {
        let spawns = crate::config::MapSpawns {
            dm: vec![[9.0, 9.0, 0.25]],
            red: vec![[5.0, 0.0, 0.25], [5.0, 4.0, 0.25]],
            ..Default::default()
        };
        assert_eq!(
            enemy_spawn_points(PLAYER_TEAM_BLUE, &spawns),
            spawns.red.as_slice()
        );
        let safe = safer_carrier_goal(
            [10.0, 0.0, 0.25],
            [0.0, 0.0, 0.25],
            enemy_spawn_points(PLAYER_TEAM_BLUE, &spawns),
            1.0,
        );
        assert_ne!(safe, [0.0, 0.0]);
        assert_eq!(
            safer_carrier_goal([1.0, 1.0, 0.25], [0.0, 0.0, 0.25], &[], 1.0),
            [0.0, 0.0]
        );
        assert_eq!(
            safer_carrier_goal([3.4, 0.0, 0.25], [0.0, 0.0, 0.25], &spawns.red, 10.0,),
            [0.0, 0.0]
        );
    }

    #[test]
    fn defenders_predict_the_spawn_farthest_from_living_friends() {
        let spawns = crate::config::MapSpawns {
            red: vec![[2.0, 2.0, 0.25], [18.0, 2.0, 0.25]],
            ..Default::default()
        };
        let mut positions = vec![None; MAX_PLAYER];
        let mut teams = vec![None; MAX_PLAYER];
        positions[0] = Some([3.0, 2.0, 0.25]);
        teams[0] = Some(PLAYER_TEAM_BLUE);
        positions[1] = Some([17.5, 2.0, 0.25]);
        teams[1] = Some(PLAYER_TEAM_RED);
        assert_eq!(
            predicted_enemy_spawn(PLAYER_TEAM_BLUE, &spawns, &positions, &teams),
            Some([18.0, 2.0])
        );
    }

    #[test]
    fn escort_goal_converges_on_pod_when_capture_is_imminent() {
        let goal = escort_carrier_goal(2, [2.1, 3.0, 0.25], [2.0, 3.0, 0.25]);
        assert_eq!(goal, [2.0, 3.0]);
    }

    #[test]
    fn tactical_state_commitment_prevents_tick_to_tick_oscillation() {
        let mut brain = BotBrain::default();
        update_tactical_state(
            &mut brain,
            BotTacticalState::Attack,
            "visible enemy",
            false,
            0.4,
        );
        assert_eq!(brain.state, BotTacticalState::Wander);
        update_tactical_state(
            &mut brain,
            BotTacticalState::Attack,
            "visible enemy",
            false,
            0.4,
        );
        assert_eq!(brain.state, BotTacticalState::Attack);
        update_tactical_state(
            &mut brain,
            BotTacticalState::Wander,
            "lost enemy",
            false,
            0.1,
        );
        assert_eq!(brain.state, BotTacticalState::Attack);
    }

    #[test]
    fn urgent_survival_and_flag_states_interrupt_immediately() {
        let mut brain = BotBrain {
            state: BotTacticalState::Attack,
            ..Default::default()
        };
        update_tactical_state(
            &mut brain,
            BotTacticalState::Retreat,
            "critical health",
            true,
            0.02,
        );
        assert_eq!(brain.state, BotTacticalState::Retreat);

        let mut ctf = CtfState::new([[2.0, 3.0, 0.25], [18.0, 3.0, 0.25]]);
        ctf.flag_state[1] = 4;
        assert_eq!(
            desired_tactical_state(4, PLAYER_TEAM_BLUE, 1.0, true, Some(&ctf), BotRole::Carrier,).0,
            BotTacticalState::Capture
        );
    }

    #[test]
    fn simulation_summary_is_machine_readable_and_deterministic() {
        let mut game = GameServer::new(crate::config::ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 2,
            bot_count: 2,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        });
        spawn_initial_bots(&mut game);
        for _ in 0..10 {
            tick_bots(&mut game, 0.02);
        }
        let json =
            serde_json::to_string(&bot_scenario_summary(&game, 7, 10, "full_ctf_round")).unwrap();
        assert!(json.contains("\"seed\":7"));
        assert!(json.contains("\"bots\":2"));
        assert_eq!(
            json,
            serde_json::to_string(&bot_scenario_summary(&game, 7, 10, "full_ctf_round",)).unwrap()
        );
    }

    #[test]
    fn carrier_near_home_completes_an_authoritative_capture() {
        let mut game = GameServer::new(crate::config::ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: std::path::PathBuf::from("../Content/main"),
            map_name: "CTF-Crazy".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 1,
            bot_count: 1,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        });
        spawn_initial_bots(&mut game);
        configure_bot_scenario(&mut game, "carrier_escort", 1);
        for _ in 0..500 {
            game.tick(0.02);
        }
        let carrier = game.players[0].as_ref().unwrap();
        assert!(
            game.blue_score > 0,
            "carrier at {:?}, goal {:?}, nav {:?}, blue pod {:?}, resolved pod {:?}, flag states {:?}",
            carrier.cf.position,
            carrier.bot_goal,
            carrier.bot_brain.nav_goal,
            game.ctf.flag_pod[0],
            game.map_grid.resolve_navigation_goal([
                game.ctf.flag_pod[0][0],
                game.ctf.flag_pod[0][1],
            ]),
            game.ctf.flag_state,
        );
    }

    #[test]
    fn perception_requires_acquisition_time_and_forgets_stale_targets() {
        let mut brain = BotBrain::default();
        let visible = Some((3, [5.0, 4.0, 0.25], 2.0));
        assert_eq!(update_perception(&mut brain, visible, None, 0.02), None);
        for _ in 0..6 {
            update_perception(&mut brain, visible, None, 0.02);
        }
        assert_eq!(
            update_perception(&mut brain, visible, None, 0.02),
            Some((3, 2.0))
        );
        assert_eq!(brain.last_known_enemy, [5.0, 4.0]);

        assert_eq!(update_perception(&mut brain, None, None, 0.5), None);
        assert_eq!(brain.perceived_target, Some(3));
        update_perception(&mut brain, None, None, BOT_ENEMY_MEMORY_TIME);
        assert_eq!(brain.perceived_target, None);
    }

    #[test]
    fn reacquiring_a_different_enemy_restarts_reaction_delay() {
        let mut brain = BotBrain::default();
        for _ in 0..8 {
            update_perception(&mut brain, Some((2, [3.0, 3.0, 0.25], 1.0)), None, 0.02);
        }
        assert!(brain.target_visible_for >= BOT_TARGET_ACQUIRE_TIME);
        assert_eq!(
            update_perception(&mut brain, Some((4, [4.0, 3.0, 0.25], 2.0)), None, 0.02,),
            None
        );
        assert_eq!(brain.perceived_target, Some(4));
        assert_eq!(brain.target_visible_for, 0.0);
    }

    #[test]
    fn audible_enemy_updates_memory_without_authorizing_fire() {
        let mut brain = BotBrain::default();
        assert_eq!(
            update_perception(&mut brain, None, Some((5, [7.0, 8.0, 0.25])), 0.08),
            None
        );
        assert_eq!(brain.perceived_target, Some(5));
        assert_eq!(brain.last_known_enemy, [7.0, 8.0]);
        assert_eq!(brain.target_visible_for, 0.0);
    }

    #[test]
    fn perception_rejects_wall_occlusion_and_reverse_diagonal_corners() {
        let grid = crate::map::MapGrid::fixture(8, &[(3, 3)]);
        assert!(!bot_can_see(&grid, [1.5, 1.5, 0.25], [5.5, 5.5, 0.25]));
        assert!(!bot_can_see(&grid, [5.5, 5.5, 0.25], [1.5, 1.5, 0.25]));
    }

    #[test]
    fn top_down_perception_notices_visible_enemy_behind_current_aim() {
        let grid = crate::map::MapGrid::fixture(8, &[]);
        assert!(bot_can_see(&grid, [4.0, 4.0, 0.25], [2.0, 4.0, 0.25]));
    }

    #[test]
    fn utility_prefers_survival_over_a_low_value_fight() {
        let healthy =
            desired_tactical_state(0, PLAYER_TEAM_BLUE, 1.0, true, None, BotRole::Attacker);
        assert_eq!(healthy.0, BotTacticalState::Attack);
        let critical =
            desired_tactical_state(0, PLAYER_TEAM_BLUE, 0.1, true, None, BotRole::Attacker);
        assert_eq!(critical.0, BotTacticalState::Retreat);
        assert!(critical.3[6] > critical.3[1]);
    }

    #[test]
    fn capture_utility_outweighs_combat_and_low_health() {
        let mut ctf = CtfState::new([[2.0, 3.0, 0.25], [18.0, 3.0, 0.25]]);
        ctf.flag_state[1] = 4;
        let choice =
            desired_tactical_state(4, PLAYER_TEAM_BLUE, 0.1, true, Some(&ctf), BotRole::Carrier);
        assert_eq!(choice.0, BotTacticalState::Capture);
        assert!(choice.3[3] > choice.3[6]);
    }

    #[test]
    fn weapon_utility_accounts_for_range_ammo_and_reload_state() {
        assert!(weapon_utility(1, 1.0, true, false) > weapon_utility(2, 1.0, true, false));
        assert!(weapon_utility(2, 12.0, true, false) > weapon_utility(1, 12.0, true, false));
        assert_eq!(weapon_utility(0, 4.0, false, false), 0.0);
        assert!(weapon_utility(0, 4.0, true, true) < weapon_utility(0, 4.0, true, false));
    }

    #[test]
    fn tactical_navigation_finds_cover_hidden_from_threat() {
        let grid = crate::map::MapGrid::fixture(10, &[(4, 3), (4, 4), (4, 5), (4, 6)]);
        let threat = [7.5, 4.5, 0.25];
        let goal = find_cover_goal(&grid, [5.5, 4.5, 0.25], threat).unwrap();
        let candidate = [goal[0], goal[1], 0.25];
        let (wall, _) = grid.ray_test(threat, candidate);
        assert!(
            (wall[0] - threat[0]).hypot(wall[1] - threat[1])
                < (candidate[0] - threat[0]).hypot(candidate[1] - threat[1])
        );
    }

    #[test]
    fn flank_slots_choose_opposite_sides_without_stacking() {
        let from = [2.0, 2.0, 0.25];
        let target = [8.0, 2.0, 0.25];
        let left = flank_goal(0, from, target);
        let right = flank_goal(2, from, target);
        assert!(left[1] > target[1]);
        assert!(right[1] < target[1]);
        assert_eq!(left[0], right[0]);
    }

    #[test]
    fn intercept_goal_leads_a_moving_carrier() {
        assert_eq!(
            intercept_goal([4.0, 5.0, 0.25], [2.0, -1.0, 0.0], 1.5),
            [7.0, 3.5]
        );
    }

    #[test]
    fn flag_runners_enter_the_server_interaction_radius() {
        assert_eq!(
            bot_goal_arrival_radius(BotRole::Attacker, BotTacticalState::AttackObjective),
            0.12
        );
        assert_eq!(
            bot_goal_arrival_radius(BotRole::Recoverer, BotTacticalState::Recover),
            0.12
        );
        assert_eq!(
            bot_goal_arrival_radius(BotRole::Carrier, BotTacticalState::Capture),
            0.12
        );
    }

    #[test]
    fn non_objective_bots_keep_the_general_arrival_tolerance() {
        assert_eq!(
            bot_goal_arrival_radius(BotRole::Attacker, BotTacticalState::Attack),
            0.40
        );
    }

    #[test]
    fn human_aim_accelerates_instead_of_snapping_to_target() {
        let mut brain = BotBrain::default();
        let first = update_human_aim(&mut brain, [1.0, 1.0], [9.0, 1.0], 0.02, 0);
        assert!(first[0] > 1.0 && first[0] < 9.0);
        let first_step = first[0] - 1.0;
        let second = update_human_aim(&mut brain, first, [9.0, 1.0], 0.02, 0);
        assert!(second[0] - first[0] > first_step);
        for _ in 0..200 {
            let current = brain.aim_point;
            update_human_aim(&mut brain, current, [9.0, 1.0], 0.02, 0);
        }
        assert!((brain.aim_point[0] - 9.0).abs() < 0.1);
    }

    #[test]
    fn automatic_fire_uses_deterministic_bursts_and_pauses() {
        let mut brain = BotBrain::default();
        assert!(consume_burst_shot(&mut brain, 0));
        assert!(consume_burst_shot(&mut brain, 0));
        assert!(consume_burst_shot(&mut brain, 0));
        assert!(brain.burst_pause > 0.0);
        assert!(!consume_burst_shot(&mut brain, 0));
        brain.burst_pause = 0.0;
        assert!(consume_burst_shot(&mut brain, 0));
    }

    #[test]
    fn bot_stuck_recovery_reinforces_side_and_timer() {
        let mut brain = BotBrain::default();
        let mut bot_dir = brain.recovery_dir;
        resolve_stuck_bots(&mut brain, true);
        assert_eq!(brain.recovery_tries, 1);
        assert!(brain.recovery_timer > 0.0);
        assert!(brain.recovery_dir < 0.0 && bot_dir > 0.0);
        bot_dir = brain.recovery_dir;

        resolve_stuck_bots(&mut brain, true);
        assert_eq!(brain.recovery_tries, 2);
        assert!(brain.recovery_dir > 0.0 && bot_dir < 0.0);
    }

    #[test]
    fn bot_recovery_heading_stays_stable_across_retry_counts() {
        let grid = crate::map::MapGrid::fixture(10, &[(4, 4)]);
        let heading = recovery_heading(&grid, [3.0, 3.0, 0.25], [8.0, 3.0], [1.0, 0.0], -1.0, 3);
        assert!(heading[0] != 3.0 || heading[1] != 3.0);
        let retry = recovery_heading(&grid, [3.0, 3.0, 0.25], [8.0, 3.0], [1.0, 0.0], -1.0, 4);
        assert_eq!(heading, retry);
    }
}
