//! Authoritative hitscan — simplified port of Game::shootSV (Game.cpp).

use crate::game::{
    GameServer, GAME_PLAYING, MAX_PLAYER, PLAYER_STATUS_ALIVE, PLAYER_STATUS_DEAD,
    PLAYER_TEAM_BLUE, PLAYER_TEAM_RED, SV_TIME_TO_SPAWN,
};
use crate::map::{segment_hit_t, segment_sphere_hit_t};
use crate::net::{ids, packet};

const PLAYER_RADIUS: f32 = 0.25;
const GAME_TYPE_DM: i8 = 0;

#[cfg(test)]
const WEAPON_SMG: i8 = 0;
const WEAPON_SHOTGUN: i8 = 1;
const WEAPON_PHOTON_RIFLE: i8 = 6;
const WEAPON_SNIPER: i8 = 2;
const WEAPON_CHAIN_GUN: i8 = 4;
const WEAPON_BAZOOKA: i8 = 5;
const WEAPON_FLAME_THROWER: i8 = 7;
// The original non-Pro/"old school" Game::shoot path used a fixed three-unit
// stream. Later Pro code added configurable range decay and exploding FT.
const FLAME_MAX_RANGE: f32 = 3.0;
const FLAME_DAMAGE: f32 = 0.08;
const WEAPON_KNIVES: i8 = 10;
const WEAPON_NUCLEAR: i8 = 11;
const WEAPON_SHIELD: i8 = 12;
const WEAPON_MOLOTOV: i8 = 9;
const FLAME_STACK_MAX: u8 = 3;
const FLAME_STACK_TIME: f32 = 1.75;
const FLAME_STACK_DAMAGE_BONUS: f32 = 0.25;
const FLAME_KILL_STREAK_MAX: u8 = 3;
const FLAME_KILL_STREAK_TIME: f32 = 4.0;
const FLAME_KILL_STREAK_DAMAGE_BONUS: f32 = 0.20;

/// Maximum plausible distance from the authoritative player centre to a weapon
/// dummy. The shipped models place every muzzle well inside this bound.
const MAX_MUZZLE_DISTANCE: f32 = 1.0;

const FIRE_DELAYS: [f32; 14] = [
    0.1, 0.85, 2.0, 0.1, 0.1, 1.75, 1.5, 0.1, 1.0, 1.0, 1.0, 12.0, 3.0, 1.0,
];

// GameVar.cpp:278-304 — start bloom and maximum bloom in degrees.
const START_SPREAD: [f32; 8] = [1.0, 12.0, 0.0, 2.0, 5.0, 0.0, 0.0, 10.0];
const MAX_SPREAD: [f32; 8] = [8.0, 20.0, 0.0, 10.0, 15.0, 0.0, 0.0, 10.0];

pub(crate) fn weapon_start_spread(weapon_id: i8) -> f32 {
    START_SPREAD.get(weapon_id as usize).copied().unwrap_or(0.0)
}

fn weapon_max_spread(weapon_id: i8) -> f32 {
    MAX_SPREAD.get(weapon_id as usize).copied().unwrap_or(0.0)
}

pub(crate) fn recover_weapon_spread(weapon_id: i8, current: f32, dt: f32) -> f32 {
    (current - dt * 10.0).max(weapon_start_spread(weapon_id))
}

fn spread_random(sequence: u32, salt: u32) -> f32 {
    let mut x = sequence.wrapping_mul(0x9e37_79b9) ^ salt;
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    (x as f32) / (u32::MAX as f32)
}

fn rotate_axis(v: [f32; 3], axis: [f32; 3], angle: f32) -> [f32; 3] {
    let c = angle.cos();
    let s = angle.sin();
    let dot = v[0] * axis[0] + v[1] * axis[1] + v[2] * axis[2];
    [
        v[0] * c + (axis[1] * v[2] - axis[2] * v[1]) * s + axis[0] * dot * (1.0 - c),
        v[1] * c + (axis[2] * v[0] - axis[0] * v[2]) * s + axis[1] * dot * (1.0 - c),
        v[2] * c + (axis[0] * v[1] - axis[1] * v[0]) * s + axis[2] * dot * (1.0 - c),
    ]
}

fn spread_endpoint(from: [f32; 3], aim: [f32; 3], range: f32, imp: f32, sequence: u32) -> [f32; 3] {
    let length = (aim[0] * aim[0] + aim[1] * aim[1] + aim[2] * aim[2])
        .sqrt()
        .max(1e-6);
    let direction = [aim[0] / length, aim[1] / length, aim[2] / length];
    let yaw = (spread_random(sequence, 0xa511_e9b3) * 2.0 - 1.0) * imp.to_radians();
    let roll = spread_random(sequence, 0x63d8_35b7) * std::f32::consts::TAU;
    let mut ray = rotate_axis(
        [
            direction[0] * range,
            direction[1] * range,
            direction[2] * range,
        ],
        [0.0, 0.0, 1.0],
        yaw,
    );
    ray = rotate_axis(ray, direction, roll);
    ray[2] *= 0.5;
    [from[0] + ray[0], from[1] + ray[1], from[2] + ray[2]]
}

pub fn is_primary_weapon(weapon_id: i8) -> bool {
    matches!(weapon_id, 0..=7)
}

pub fn is_melee_weapon(weapon_id: i8) -> bool {
    matches!(weapon_id, WEAPON_KNIVES | WEAPON_NUCLEAR | WEAPON_SHIELD)
}

fn is_direct_weapon(weapon_id: i8) -> bool {
    matches!(weapon_id, 0 | 1 | 2 | 3 | 4 | 6 | 7)
}

fn fire_delay(weapon_id: i8) -> Option<f32> {
    FIRE_DELAYS.get(weapon_id as usize).copied()
}

/// Mirrors `ServerRecv.cpp:916-975`. Shotgun/sniper send one packet per
/// pellet in a single render frame, so only the first packet consumes the
/// cooldown; the remainder consume the bounded burst allowance.
fn authorize_direct_fire(player: &mut crate::game::Player, weapon_id: i8) -> Option<u8> {
    if player.weapon_id != weapon_id || !is_direct_weapon(weapon_id) {
        return None;
    }
    let delay = fire_delay(weapon_id)?;

    if weapon_id == WEAPON_SHOTGUN || weapon_id == WEAPON_SNIPER {
        if player.elapsed_since_last_shot + 0.061 > delay {
            player.elapsed_since_last_shot = 0.0;
            player.burst_shot_count = 1;
            return Some(0);
        }
        let burst_max = weapon_nb_shot(weapon_id) as u8;
        if player.burst_shot_count < burst_max {
            player.burst_shot_count += 1;
            return Some(player.burst_shot_count - 1);
        }
        return None;
    }

    let tolerance = if weapon_id == WEAPON_CHAIN_GUN {
        0.04
    } else {
        0.051
    };
    if player.elapsed_since_last_shot + tolerance > delay {
        if weapon_id == WEAPON_FLAME_THROWER {
            if player.elapsed_since_last_shot <= delay + 0.075 {
                player.flame_seconds_fired += delay;
            } else {
                player.flame_seconds_fired = 0.0;
            }
        } else {
            player.flame_seconds_fired = 0.0;
        }
        player.elapsed_since_last_shot = 0.0;
        player.burst_shot_count = 0;
        Some(0)
    } else {
        None
    }
}

pub(super) fn authorize_projectile_fire(
    player: &mut crate::game::Player,
    weapon_id: i8,
    projectile_type: i8,
) -> bool {
    let valid = match projectile_type {
        2 => weapon_id == 5 && player.weapon_id == 5,
        3 => weapon_id == 8 && player.grenades_left > 0,
        7 => weapon_id == 9 && player.molotovs_left > 0,
        _ => false,
    };
    if !valid {
        return false;
    }
    match projectile_type {
        3 => {
            if player.grenade_elapsed < 1.0 {
                return false;
            }
            player.grenade_elapsed = 0.0;
            player.grenades_left -= 1;
        }
        7 => {
            if player.molotov_elapsed < 1.0 {
                return false;
            }
            player.molotov_elapsed = 0.0;
            player.molotovs_left -= 1;
        }
        _ => {
            let Some(delay) = fire_delay(weapon_id) else {
                return false;
            };
            if player.elapsed_since_last_shot + 0.1 <= delay {
                return false;
            }
            player.elapsed_since_last_shot = 0.0;
        }
    }
    true
}

pub(super) fn apply_server_burn(
    game: &mut GameServer,
    position: [f32; 3],
    radius: f32,
    owner: usize,
) {
    let weapon_id = 9;
    let base_dmg = weapon_damage(weapon_id);
    let attacker_streak = game
        .players
        .get(owner)
        .and_then(Option::as_ref)
        .map(|p| p.molotov_kill_streak)
        .unwrap_or(0);
    let attacker_multiplier = 1.0 + (attacker_streak as f32) * FLAME_KILL_STREAK_DAMAGE_BONUS;
    for slot in 0..MAX_PLAYER {
        let Some(target) = game.players[slot].as_ref() else {
            continue;
        };
        // Native radiusHit lets Molotov fire burn its thrower even when
        // ordinary team damage is disabled.
        if target.status != PLAYER_STATUS_ALIVE
            || (slot != owner && !can_damage(game.config.game_type, owner, slot, game))
        {
            continue;
        }
        let tp = target.cf.position;
        let d = ((tp[0] - position[0]).powi(2)
            + (tp[1] - position[1]).powi(2)
            + (tp[2] - position[2]).powi(2))
        .sqrt();
        if d >= radius {
            continue;
        }
        let (wall_end, _) = game.map_grid.ray_test(position, tp);
        let wall_d = ((wall_end[0] - position[0]).powi(2)
            + (wall_end[1] - position[1]).powi(2)
            + (wall_end[2] - position[2]).powi(2))
        .sqrt();
        if wall_d + 0.01 >= d {
            if let Some(victim) = game.players[slot].as_mut() {
                let stack_scale =
                    1.0 + (victim.flame_burn_stacks as f32) * FLAME_STACK_DAMAGE_BONUS;
                victim.flame_burn_stacks = (victim.flame_burn_stacks + 1).min(FLAME_STACK_MAX);
                victim.flame_stack_decay = FLAME_STACK_TIME;
                apply_damage(
                    game,
                    slot,
                    owner,
                    weapon_id,
                    base_dmg * radial_falloff(d, radius) * stack_scale * attacker_multiplier,
                );
            }
        }
    }
}

/// Weapon damage table — mirrors jsclient `constants.js` WEAPONS[].damage.
fn weapon_damage(weapon_id: i8) -> f32 {
    const DMG: [f32; 14] = [
        0.1, 0.21, 0.3, 0.13, 0.19, 0.85, 0.24, 0.08, 1.5, 0.15, 0.6, 8.0, 0.0, 0.05,
    ];
    let i = weapon_id as usize;
    if i < DMG.len() {
        DMG[i]
    } else {
        0.1
    }
}

fn weapon_nb_shot(weapon_id: i8) -> i32 {
    match weapon_id {
        1 => 5, // shotgun
        2 => 2, // sniper
        _ => 1,
    }
}

fn radial_falloff(distance: f32, radius: f32) -> f32 {
    if radius <= 0.0 {
        return 0.0;
    }
    (1.0 - distance / radius).clamp(0.0, 1.0)
}

fn shield_damage_multiplier(protection_remaining: f32) -> f32 {
    if protection_remaining > 0.6 {
        0.5
    } else {
        1.0
    }
}

fn protected_damage(dmg: f32, shield_remaining: f32, spawn_immunity_remaining: f32) -> f32 {
    if spawn_immunity_remaining > 0.3 {
        0.0
    } else {
        dmg * shield_damage_multiplier(shield_remaining)
    }
}

fn authorize_melee(player: &mut crate::game::Player) -> Option<i8> {
    let weapon_id = player.melee_id;
    let delay = match weapon_id {
        WEAPON_KNIVES => 1.0,
        WEAPON_NUCLEAR => 12.0,
        WEAPON_SHIELD => 3.0,
        _ => return None,
    };
    if player.status != PLAYER_STATUS_ALIVE || player.melee_elapsed + 0.05 < delay {
        return None;
    }
    player.melee_elapsed = 0.0;
    Some(weapon_id)
}

pub fn handle_melee(game: &mut GameServer, shooter: usize, payload: &[u8]) {
    if game.round_state != GAME_PLAYING {
        return;
    }
    let weapon_id = {
        let Some(player) = game.players[shooter].as_mut() else {
            return;
        };
        let Some(weapon_id) = authorize_melee(player) else {
            tracing::debug!("melee rejected: shooter={shooter} equipment/cooldown");
            return;
        };
        weapon_id
    };

    match weapon_id {
        WEAPON_KNIVES => {
            let origin = game.players[shooter].as_ref().unwrap().cf.position;
            let mut victims = Vec::new();
            for slot in 0..MAX_PLAYER {
                if slot == shooter || !can_damage(game.config.game_type, shooter, slot, game) {
                    continue;
                }
                let Some(target) = game.players[slot].as_ref() else {
                    continue;
                };
                if target.status != PLAYER_STATUS_ALIVE {
                    continue;
                }
                let distance = ((target.cf.position[0] - origin[0]).powi(2)
                    + (target.cf.position[1] - origin[1]).powi(2)
                    + (target.cf.position[2] - origin[2]).powi(2))
                .sqrt();
                if distance >= 1.0 {
                    continue;
                }
                let (wall, _) = game.map_grid.ray_test(origin, target.cf.position);
                if segment_hit_t(origin, target.cf.position, wall) >= 0.999 {
                    victims.push(slot);
                }
            }
            for victim in victims {
                apply_hit(game, victim, shooter, WEAPON_KNIVES);
            }
        }
        WEAPON_SHIELD => {
            if let Some(player) = game.players[shooter].as_mut() {
                player.protection_remaining = 2.0;
            }
        }
        WEAPON_NUCLEAR => {
            if let Some(player) = game.players[shooter].as_mut() {
                player.nuke_remaining = 3.0;
            }
        }
        _ => return,
    }

    game.broadcast(packet::encode_frame(
        ids::NET_CLSV_SVCL_PLAYER_SHOOT_MELEE,
        payload,
    ));
}

pub fn bot_melee(game: &mut GameServer, shooter: usize) {
    let melee_id = game.players[shooter]
        .as_ref()
        .map(|p| p.melee_id)
        .unwrap_or(WEAPON_KNIVES);
    let payload = [shooter as u8, melee_id as u8];
    handle_melee(game, shooter, &payload);
}

pub fn tick_special_weapons(game: &mut GameServer, dt: f32) {
    let mut detonations = Vec::new();
    for slot in 0..MAX_PLAYER {
        let Some(player) = game.players[slot].as_mut() else {
            continue;
        };
        if player.nuke_remaining <= 0.0 {
            continue;
        }
        player.nuke_remaining -= dt;
        if player.nuke_remaining <= 0.0 {
            detonations.push((slot, player.cf.position));
        }
    }
    for (owner, position) in detonations {
        handle_explosion(
            game,
            position,
            [0.0, 0.0, 1.0],
            6.0,
            owner as i8,
            WEAPON_NUCLEAR,
        );
    }
}

pub fn bot_fire(game: &mut GameServer, shooter: usize, target: [f32; 2]) {
    let Some(p) = game.players[shooter].as_ref() else {
        return;
    };
    let p1 = p.cf.position;
    let dx = target[0] - p1[0];
    let dy = target[1] - p1[1];
    let len = (dx * dx + dy * dy).sqrt().max(0.01);
    let aim = [dx / len, dy / len, 0.0];
    let mut payload = vec![0u8; 16];
    payload[0] = shooter as u8;
    payload[1] = p.weapon_id as u8;
    payload[2] = 0;
    payload[4..6].copy_from_slice(&((p1[0] * ids::WIRE_POS) as i16).to_le_bytes());
    payload[6..8].copy_from_slice(&((p1[1] * ids::WIRE_POS) as i16).to_le_bytes());
    payload[8..10].copy_from_slice(&((p1[2] * ids::WIRE_POS) as i16).to_le_bytes());
    payload[10..12].copy_from_slice(&((aim[0] * ids::WIRE_POS) as i16).to_le_bytes());
    payload[12..14].copy_from_slice(&((aim[1] * ids::WIRE_POS) as i16).to_le_bytes());
    payload[14..16].copy_from_slice(&((aim[2] * ids::WIRE_POS) as i16).to_le_bytes());
    tracing::debug!(
        "bot fire: shooter={shooter} toward ({:.1},{:.1})",
        target[0],
        target[1]
    );
    handle_player_shoot(game, shooter, &payload);
}

pub fn handle_player_shoot(game: &mut GameServer, shooter: usize, payload: &[u8]) {
    if game.round_state != GAME_PLAYING {
        return;
    }
    if payload.len() < 16 {
        tracing::debug!("shoot rejected: payload too short ({})", payload.len());
        return;
    }
    let Some(shooter_p) = game.players[shooter].as_ref() else {
        tracing::debug!("shoot rejected: unknown shooter slot {shooter}");
        return;
    };
    if shooter_p.status != PLAYER_STATUS_ALIVE {
        tracing::debug!(
            "shoot rejected: shooter {shooter} status {} (not alive)",
            shooter_p.status
        );
        return;
    }

    let weapon_id = payload[1] as i8;
    let equipped = shooter_p.weapon_id;
    if equipped != weapon_id || !is_direct_weapon(weapon_id) {
        tracing::warn!(
            "shoot rejected: shooter={shooter} equipped={equipped} packet_weapon={weapon_id}"
        );
        return;
    }

    let p1 = [
        packet::read_i16(payload, 4).unwrap_or(0) as f32 / ids::WIRE_POS,
        packet::read_i16(payload, 6).unwrap_or(0) as f32 / ids::WIRE_POS,
        packet::read_i16(payload, 8).unwrap_or(0) as f32 / ids::WIRE_POS,
    ];
    let mut p2 = [
        packet::read_i16(payload, 10).unwrap_or(0) as f32 / ids::WIRE_POS,
        packet::read_i16(payload, 12).unwrap_or(0) as f32 / ids::WIRE_POS,
        packet::read_i16(payload, 14).unwrap_or(0) as f32 / ids::WIRE_POS,
    ];
    let nuzzle_id = payload[2] as i8;

    let player_pos = shooter_p.cf.position;
    let muzzle_distance = ((p1[0] - player_pos[0]).powi(2)
        + (p1[1] - player_pos[1]).powi(2)
        + (p1[2] - player_pos[2]).powi(2))
    .sqrt();
    let (muzzle_clip, _) = game.map_grid.ray_test(player_pos, p1);
    let muzzle_t = segment_hit_t(player_pos, p1, muzzle_clip);
    if !muzzle_distance.is_finite()
        || muzzle_distance > MAX_MUZZLE_DISTANCE
        || (muzzle_distance > 1e-3 && muzzle_t < 1.0 - 1e-3)
    {
        tracing::warn!(
            "shoot rejected: shooter={shooter} invalid/obstructed muzzle distance={muzzle_distance:.3} t={muzzle_t:.3}"
        );
        return;
    }

    let burst_index = {
        let Some(player) = game.players[shooter].as_mut() else {
            return;
        };
        authorize_direct_fire(player, weapon_id)
    };
    let Some(burst_index) = burst_index else {
        tracing::debug!("shoot rejected: shooter={shooter} weapon={weapon_id} cooldown/burst");
        return;
    };
    let (imp, sequence) = {
        let player = game.players[shooter]
            .as_mut()
            .expect("authorized player exists");
        player.weapon_current_imp =
            (player.weapon_current_imp + 3.0).min(weapon_max_spread(weapon_id));
        player.weapon_shot_sequence = player.weapon_shot_sequence.wrapping_add(1);
        (player.weapon_current_imp, player.weapon_shot_sequence)
    };
    let aim = p2;
    if weapon_id == WEAPON_FLAME_THROWER {
        p2 = spread_endpoint(p1, aim, FLAME_MAX_RANGE, imp, sequence);
    } else if weapon_id == WEAPON_SHOTGUN {
        let base = spread_endpoint(p1, aim, 128.0, 0.0, sequence);
        p2 = shotgun_pellet_endpoint(p1, base, burst_index);
    } else {
        p2 = spread_endpoint(p1, aim, 128.0, imp, sequence);
    }

    let (wall_end, normal) = game.map_grid.ray_test(p1, p2);
    let wall_t = segment_hit_t(p1, p2, wall_end);
    let mut hit_id: i8 = -1;
    let mut blocker_id: i8 = -1;
    let mut best_t = f32::MAX;
    let mut flame_hits = Vec::new();

    for slot in 0..MAX_PLAYER {
        if slot == shooter {
            continue;
        }
        let Some(target) = game.players[slot].as_ref() else {
            continue;
        };
        if target.status != PLAYER_STATUS_ALIVE {
            continue;
        }
        if weapon_id == WEAPON_FLAME_THROWER
            && !can_damage(game.config.game_type, shooter, slot, game)
        {
            continue;
        }
        let pos = target.cf.position;
        let hit_r = if weapon_id == WEAPON_FLAME_THROWER {
            0.5
        } else {
            PLAYER_RADIUS
        };
        if let Some(t) = segment_sphere_hit_t(p1, p2, pos, hit_r) {
            if weapon_id == WEAPON_FLAME_THROWER && t <= wall_t + 1e-3 {
                // The oldest server loop applies one damage tick to every
                // babo intersected by the wide flame segment.
                flame_hits.push(slot);
            } else if t <= wall_t + 1e-3 && t < best_t {
                best_t = t;
                blocker_id = slot as i8;
                hit_id = if can_damage(game.config.game_type, shooter, slot, game) {
                    slot as i8
                } else {
                    -1
                };
            }
        }
    }

    if weapon_id == WEAPON_FLAME_THROWER {
        for victim in flame_hits.iter().copied() {
            tracing::debug!("flame hit: shooter={shooter} victim={victim}");
            apply_damage(game, victim, shooter, weapon_id, FLAME_DAMAGE);
        }
    } else if hit_id >= 0 {
        tracing::debug!("shoot hit: shooter={shooter} victim={hit_id} weapon={weapon_id}");
        apply_hit(game, hit_id as usize, shooter, weapon_id);
    }

    if weapon_id == WEAPON_PHOTON_RIFLE {
        if let Some(player) = game.players[shooter].as_mut() {
            player.photon_frames = 30;
            player.photon_from = p1;
            player.photon_to = wall_end;
        }
    }

    let end = visual_shot_endpoint(weapon_id, p1, p2, wall_end, blocker_id, best_t);

    tracing::debug!("shoot broadcast: shooter={shooter} weapon={weapon_id} hit_player={hit_id}");
    let mut out = vec![0u8; 20];
    out[0] = shooter as u8;
    out[1] = hit_id as u8;
    out[2] = nuzzle_id as u8;
    out[3] = weapon_id as u8;
    out[4..6].copy_from_slice(&((p1[0] * ids::WIRE_POS) as i16).to_le_bytes());
    out[6..8].copy_from_slice(&((p1[1] * ids::WIRE_POS) as i16).to_le_bytes());
    out[8..10].copy_from_slice(&((p1[2] * ids::WIRE_POS) as i16).to_le_bytes());
    out[10..12].copy_from_slice(&((end[0] * ids::WIRE_POS) as i16).to_le_bytes());
    out[12..14].copy_from_slice(&((end[1] * ids::WIRE_POS) as i16).to_le_bytes());
    out[14..16].copy_from_slice(&((end[2] * ids::WIRE_POS) as i16).to_le_bytes());
    out[16] = (normal[0] * 120.0) as i8 as u8;
    out[17] = (normal[1] * 120.0) as i8 as u8;
    out[18] = (normal[2] * 120.0) as i8 as u8;
    game.broadcast(packet::encode_frame(ids::NET_SVCL_PLAYER_SHOOT, &out));
}

fn visual_shot_endpoint(
    weapon_id: i8,
    p1: [f32; 3],
    p2: [f32; 3],
    wall_end: [f32; 3],
    blocker_id: i8,
    best_t: f32,
) -> [f32; 3] {
    if weapon_id != WEAPON_FLAME_THROWER
        && weapon_id != WEAPON_PHOTON_RIFLE
        && blocker_id >= 0
        && best_t < f32::MAX
    {
        return [
            p1[0] + (p2[0] - p1[0]) * best_t,
            p1[1] + (p2[1] - p1[1]) * best_t,
            p1[2] + (p2[2] - p1[2]) * best_t,
        ];
    }
    wall_end
}

fn can_damage(game_type: i8, shooter: usize, target: usize, game: &GameServer) -> bool {
    let st = game.players[shooter].as_ref().map(|p| p.team_id);
    let tt = game.players[target].as_ref().map(|p| p.team_id);
    can_damage_teams(game_type, st, tt)
}

fn can_damage_teams(game_type: i8, shooter_team: Option<i8>, target_team: Option<i8>) -> bool {
    if matches!(game_type, GAME_TYPE_DM | crate::game::GAME_TYPE_SND) {
        return true;
    }
    match (shooter_team, target_team) {
        (Some(a), Some(b)) => {
            a != b
                && (a == PLAYER_TEAM_BLUE || a == PLAYER_TEAM_RED)
                && (b == PLAYER_TEAM_BLUE || b == PLAYER_TEAM_RED)
        }
        _ => false,
    }
}

fn apply_damage(game: &mut GameServer, victim: usize, attacker: usize, weapon_id: i8, dmg: f32) {
    apply_damage_from(game, victim, Some(attacker), weapon_id, dmg);
}

pub(crate) fn apply_world_damage(game: &mut GameServer, victim: usize, weapon_id: i8, dmg: f32) {
    // The map floor is an unconditional world kill, not combat damage. Spawn
    // immunity and shields must not leave a live player stranded below z=0.
    if let Some(player) = game.players.get_mut(victim).and_then(Option::as_mut) {
        player.spawn_immunity_remaining = 0.0;
        player.protection_remaining = 0.0;
    }
    apply_damage_from(game, victim, None, weapon_id, dmg);
}

/// `attacker` is `None` for world damage (an explosion with no live owner).
fn apply_damage_from(
    game: &mut GameServer,
    victim: usize,
    attacker: Option<usize>,
    weapon_id: i8,
    dmg: f32,
) {
    if dmg <= 0.0 {
        return;
    }
    let attacker_wire = attacker.map(|a| a as i8).unwrap_or(-1);
    let (life_before, life_left, credited_damage) = {
        let Some(v) = game.players[victim].as_mut() else {
            return;
        };
        let before = v.life;
        let actual_dmg = protected_damage(dmg, v.protection_remaining, v.spawn_immunity_remaining);
        v.life = (v.life - actual_dmg).max(0.0);
        (before, v.life, actual_dmg.min(before))
    };
    if let Some(attacker) = attacker.filter(|&slot| slot != victim) {
        if let Some(source) = game.players[attacker].as_mut() {
            source.dmg += credited_damage;
        }
        broadcast_enum(game, attacker);
    }
    tracing::debug!(
        "hit: victim={victim} attacker={attacker_wire} weapon={weapon_id} dmg={dmg:.3} life {life_before:.3}->{life_left:.3}"
    );

    let mut hit = vec![0u8; 16];
    hit[0] = victim as u8;
    hit[1] = attacker_wire as u8;
    hit[2] = weapon_id as u8;
    hit[3..7].copy_from_slice(&life_left.to_le_bytes());
    hit[9] = 1;
    game.broadcast(packet::encode_frame(ids::NET_SVCL_PLAYER_HIT, &hit));

    if life_left <= f32::EPSILON {
        let position = game.players[victim]
            .as_ref()
            .map(|player| player.cf.position)
            .unwrap_or([0.0; 3]);
        tracing::info!(
            victim,
            attacker = attacker_wire,
            weapon = weapon_id,
            life_before,
            x = position[0],
            y = position[1],
            z = position[2],
            "player killed"
        );
        if let Some(victim_player) = game.players[victim].as_ref() {
            let position = victim_player.cf.position;
            let velocity = victim_player.cf.vel;
            let victim_weapon_id = victim_player.weapon_id;
            let grenades = victim_player.grenades_left;
            crate::game::projectile::spawn_death_pickups(
                game,
                victim,
                position,
                velocity,
                victim_weapon_id,
                grenades,
            );
        }
        if let Some(attacker) = attacker.filter(|&_| weapon_id == WEAPON_MOLOTOV) {
            if let Some(attacker_player) = game.players[attacker].as_mut() {
                attacker_player.molotov_kill_streak =
                    (attacker_player.molotov_kill_streak + 1).min(FLAME_KILL_STREAK_MAX);
                attacker_player.molotov_kill_streak_decay = FLAME_KILL_STREAK_TIME;
            }
        }
        crate::game::ctf::drop_carrier_flags(game, victim);
        if let Some(v) = game.players[victim].as_mut() {
            v.status = PLAYER_STATUS_DEAD;
            v.time_to_spawn = SV_TIME_TO_SPAWN;
            if v.is_bot {
                v.bot_respawn_timer = crate::game::bots::BOT_RESPAWN_DELAY;
            }
            v.deaths += 1;
        }
        if let Some(attacker) = attacker {
            // Player.cpp:1469 — CTF team score tracks captures, not frags.
            let team_frags = frags_count_for_team_score(game.config.game_type);
            if let Some(a) = game.players[attacker].as_mut() {
                a.kills += 1;
                if team_frags {
                    // In CTF this wire field is the player's successful capture
                    // count. Kills have their own field and must never inflate it.
                    a.score += 1;
                }
                let team = a.team_id;
                if team_frags {
                    if team == PLAYER_TEAM_BLUE {
                        game.blue_score += 1;
                    } else if team == PLAYER_TEAM_RED {
                        game.red_score += 1;
                    }
                }
            }
            broadcast_enum(game, attacker);
            game.broadcast(packet::server_info(
                &game.config.map_name,
                game.config.game_type,
                game.blue_score,
                game.red_score,
                game.blue_win,
                game.red_win,
            ));
        }
        broadcast_enum(game, victim);
    }
}

fn frags_count_for_team_score(game_type: i8) -> bool {
    game_type != crate::game::ctf::GAME_TYPE_CTF
}

fn shotgun_pellet_endpoint(from: [f32; 3], to: [f32; 3], pellet: u8) -> [f32; 3] {
    const ANGLES: [f32; 5] = [-10.0, -5.0, 0.0, 5.0, 10.0];
    let angle = ANGLES
        .get(pellet as usize)
        .copied()
        .unwrap_or(0.0)
        .to_radians();
    let dx = to[0] - from[0];
    let dy = to[1] - from[1];
    let cos = angle.cos();
    let sin = angle.sin();
    [
        from[0] + dx * cos - dy * sin,
        from[1] + dx * sin + dy * cos,
        to[2],
    ]
}

fn apply_hit(game: &mut GameServer, victim: usize, attacker: usize, weapon_id: i8) {
    apply_damage(game, victim, attacker, weapon_id, weapon_damage(weapon_id));
}

pub fn tick_photon_beams(game: &mut GameServer) {
    let mut beams = Vec::new();
    for shooter in 0..MAX_PLAYER {
        let Some(player) = game.players[shooter].as_mut() else {
            continue;
        };
        if player.photon_frames == 0 {
            continue;
        }
        player.photon_frames -= 1;
        if player.photon_frames % 3 == 0 {
            beams.push((shooter, player.photon_from, player.photon_to));
        }
    }
    for (shooter, from, to) in beams {
        let wall_t = segment_hit_t(from, to, game.map_grid.ray_test(from, to).0);
        let victims: Vec<_> = (0..MAX_PLAYER)
            .filter(|&slot| {
                if slot == shooter || !can_damage(game.config.game_type, shooter, slot, game) {
                    return false;
                }
                let Some(target) = game.players[slot].as_ref() else {
                    return false;
                };
                target.status == PLAYER_STATUS_ALIVE
                    && segment_sphere_hit_t(from, to, target.cf.position, 0.35)
                        .is_some_and(|t| t <= wall_t + 1e-3)
            })
            .collect();
        for victim in victims {
            apply_damage(
                game,
                victim,
                shooter,
                WEAPON_PHOTON_RIFLE,
                weapon_damage(WEAPON_PHOTON_RIFLE) * 0.5,
            );
        }
    }
}

pub(crate) fn broadcast_enum(game: &GameServer, slot: usize) {
    let Some(p) = game.players[slot].as_ref() else {
        return;
    };
    let body = p.enum_state_bytes();
    game.broadcast(packet::encode_frame(ids::NET_SVCL_PLAYER_ENUM_STATE, &body));
}

pub fn handle_explosion(
    game: &mut GameServer,
    position: [f32; 3],
    normal: [f32; 3],
    radius: f32,
    owner_id: i8,
    weapon_id: i8,
) {
    // net_svcl_explosion includes the owner after its seven floats.
    let mut out = vec![0u8; 29];
    out[0..4].copy_from_slice(&position[0].to_le_bytes());
    out[4..8].copy_from_slice(&position[1].to_le_bytes());
    out[8..12].copy_from_slice(&position[2].to_le_bytes());
    out[12..16].copy_from_slice(&normal[0].to_le_bytes());
    out[16..20].copy_from_slice(&normal[1].to_le_bytes());
    out[20..24].copy_from_slice(&normal[2].to_le_bytes());
    out[24..28].copy_from_slice(&radius.to_le_bytes());
    out[28] = owner_id as u8;
    game.broadcast(packet::encode_frame(ids::NET_SVCL_EXPLOSION, &out));

    let damage_radius = if weapon_id == WEAPON_NUCLEAR || weapon_id == WEAPON_BAZOOKA {
        radius
    } else {
        radius * 2.0
    };
    let base_dmg = weapon_damage(weapon_id);
    let owner = (owner_id >= 0).then_some(owner_id as usize);
    for slot in 0..MAX_PLAYER {
        let Some(p) = game.players[slot].as_ref() else {
            continue;
        };
        if p.status != PLAYER_STATUS_ALIVE {
            continue;
        }
        let tp = p.cf.position;
        let dx = tp[0] - position[0];
        let dy = tp[1] - position[1];
        let dz = tp[2] - position[2];
        let d = (dx * dx + dy * dy + dz * dz).sqrt();
        if d > damage_radius {
            continue;
        }
        if let Some(owner) = owner {
            // Native radiusHit damages the explosive owner even when team
            // damage is disabled; only other teammates remain protected.
            if slot != owner && !can_damage(game.config.game_type, owner, slot, game) {
                continue;
            }
        }
        // Game.cpp:1618 — blast damage does not travel through walls.
        let (wall_end, _) = game.map_grid.ray_test(position, tp);
        let wall_d = ((wall_end[0] - position[0]).powi(2)
            + (wall_end[1] - position[1]).powi(2)
            + (wall_end[2] - position[2]).powi(2))
        .sqrt();
        if wall_d + 0.01 < d {
            continue;
        }
        let falloff = radial_falloff(d, damage_radius);
        apply_damage_from(game, slot, owner, weapon_id, base_dmg * falloff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::game::Player;
    use std::path::PathBuf;

    fn armed(weapon_id: i8) -> Player {
        let mut p = Player::new(0, 1, "127.0.0.1".into());
        p.weapon_id = weapon_id;
        p.elapsed_since_last_shot = f32::INFINITY;
        p
    }

    #[test]
    fn rejects_weapon_different_from_equipped() {
        let mut p = armed(0);
        assert!(authorize_direct_fire(&mut p, 6).is_none());
    }

    #[test]
    fn ctf_team_score_never_counts_frags() {
        assert!(!frags_count_for_team_score(crate::game::ctf::GAME_TYPE_CTF));
        assert!(frags_count_for_team_score(1));
    }

    #[test]
    fn champion_mode_treats_every_other_player_as_an_enemy() {
        assert!(can_damage_teams(
            crate::game::GAME_TYPE_SND,
            Some(PLAYER_TEAM_BLUE),
            Some(PLAYER_TEAM_BLUE)
        ));
    }

    #[test]
    fn ctf_kills_do_not_increment_personal_capture_score() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: crate::game::ctf::GAME_TYPE_CTF,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        });
        for slot in 0..2 {
            let mut player = armed(WEAPON_SMG);
            player.slot = slot;
            player.status = PLAYER_STATUS_ALIVE;
            player.team_id = if slot == 0 {
                PLAYER_TEAM_BLUE
            } else {
                PLAYER_TEAM_RED
            };
            game.players[slot] = Some(player);
        }

        apply_damage(&mut game, 1, 0, WEAPON_SMG, 2.0);

        let killer = game.players[0].as_ref().unwrap();
        assert_eq!(killer.kills, 1);
        assert_eq!(killer.score, 0);
        assert_eq!(game.blue_score, 0);
    }

    #[test]
    fn enforces_automatic_weapon_delay_with_legacy_tolerance() {
        let mut p = armed(0);
        assert!(authorize_direct_fire(&mut p, 0).is_some());
        assert!(authorize_direct_fire(&mut p, 0).is_none());
        p.elapsed_since_last_shot = 0.05;
        assert!(authorize_direct_fire(&mut p, 0).is_some());
    }

    #[test]
    fn bounds_shotgun_packets_to_one_five_pellet_burst() {
        let mut p = armed(WEAPON_SHOTGUN);
        for pellet in 0..5 {
            assert_eq!(authorize_direct_fire(&mut p, WEAPON_SHOTGUN), Some(pellet));
        }
        assert!(authorize_direct_fire(&mut p, WEAPON_SHOTGUN).is_none());
        p.elapsed_since_last_shot = 0.79;
        assert_eq!(authorize_direct_fire(&mut p, WEAPON_SHOTGUN), Some(0));
    }

    #[test]
    fn rejects_projectile_and_melee_ids_on_hitscan_path() {
        for weapon in [5, 8, 9, WEAPON_KNIVES, WEAPON_NUCLEAR, WEAPON_SHIELD, 13] {
            let mut p = armed(weapon);
            assert!(authorize_direct_fire(&mut p, weapon).is_none());
        }
    }

    #[test]
    fn shotgun_spread_is_deterministic_and_symmetric() {
        let from = [0.0, 0.0, 0.25];
        let to = [0.0, 10.0, 0.25];
        let pellets: Vec<_> = (0..5)
            .map(|i| shotgun_pellet_endpoint(from, to, i))
            .collect();
        assert!(pellets[0][0] > pellets[1][0]);
        assert!((pellets[2][0]).abs() < 0.0001);
        assert!((pellets[0][0] + pellets[4][0]).abs() < 0.0001);
        assert!((pellets[1][0] + pellets[3][0]).abs() < 0.0001);
        assert!((pellets[0][0] - 10.0_f32.to_radians().sin() * 10.0).abs() < 0.0001);
    }

    #[test]
    fn native_automatic_accuracy_order_keeps_smg_tightest() {
        assert_eq!(weapon_start_spread(WEAPON_SMG), 1.0);
        assert_eq!(weapon_max_spread(WEAPON_SMG), 8.0);
        assert!(weapon_start_spread(WEAPON_SMG) < weapon_start_spread(3));
        assert!(weapon_start_spread(3) < weapon_start_spread(WEAPON_CHAIN_GUN));
        assert_eq!(weapon_start_spread(WEAPON_SNIPER), 0.0);
    }

    #[test]
    fn automatic_bloom_increases_by_three_and_recovers_ten_degrees_per_second() {
        let mut player = armed(WEAPON_SMG);
        player.weapon_current_imp = weapon_start_spread(WEAPON_SMG);
        player.weapon_current_imp =
            (player.weapon_current_imp + 3.0).min(weapon_max_spread(WEAPON_SMG));
        assert_eq!(player.weapon_current_imp, 4.0);
        assert_eq!(
            recover_weapon_spread(WEAPON_SMG, player.weapon_current_imp, 0.1),
            3.0
        );
        assert_eq!(recover_weapon_spread(WEAPON_SMG, 2.0, 1.0), 1.0);
    }

    #[test]
    fn point_blank_shotgun_lands_lethal_five_pellet_damage() {
        let from = [1.0, 1.0, 0.25];
        let full_ray = [41.0, 1.0, 0.25];
        let target = [2.0, 1.0, 0.25];
        let hits = (0..5)
            .filter(|&pellet| {
                segment_sphere_hit_t(
                    from,
                    shotgun_pellet_endpoint(from, full_ray, pellet),
                    target,
                    PLAYER_RADIUS,
                )
                .is_some()
            })
            .count();
        assert_eq!(hits, 5);
        assert!(hits as f32 * weapon_damage(WEAPON_SHOTGUN) >= 1.0);
    }

    #[test]
    fn sniper_contract_is_exactly_two_traces() {
        assert_eq!(weapon_nb_shot(WEAPON_SNIPER), 2);
        let mut p = armed(WEAPON_SNIPER);
        assert_eq!(authorize_direct_fire(&mut p, WEAPON_SNIPER), Some(0));
        assert_eq!(authorize_direct_fire(&mut p, WEAPON_SNIPER), Some(1));
        assert!(authorize_direct_fire(&mut p, WEAPON_SNIPER).is_none());
    }

    #[test]
    fn validates_spawn_weapon_classes() {
        assert!((0..=7).all(is_primary_weapon));
        assert!(!is_primary_weapon(8));
        assert!([WEAPON_KNIVES, WEAPON_NUCLEAR, WEAPON_SHIELD]
            .into_iter()
            .all(is_melee_weapon));
        assert!(!is_melee_weapon(13));
    }

    #[test]
    fn projectile_authority_checks_equipment_cooldown_and_ammo() {
        let mut rocket = armed(5);
        assert!(authorize_projectile_fire(&mut rocket, 5, 2));
        assert!(!authorize_projectile_fire(&mut rocket, 5, 2));

        let mut thrower = armed(0);
        assert!(authorize_projectile_fire(&mut thrower, 8, 3));
        assert_eq!(thrower.grenades_left, 1);
        assert!(!authorize_projectile_fire(&mut thrower, 8, 3));
        thrower.grenade_elapsed = 1.0;
        assert!(authorize_projectile_fire(&mut thrower, 8, 3));
        thrower.grenade_elapsed = 1.0;
        assert!(!authorize_projectile_fire(&mut thrower, 8, 3));
    }

    #[test]
    fn explosion_damage_falls_off_linearly_and_clamps() {
        assert_eq!(radial_falloff(0.0, 3.0), 1.0);
        assert!((radial_falloff(1.5, 3.0) - 0.5).abs() < f32::EPSILON);
        assert_eq!(radial_falloff(3.0, 3.0), 0.0);
        assert_eq!(radial_falloff(4.0, 3.0), 0.0);
        assert_eq!(radial_falloff(0.0, 0.0), 0.0);
    }

    #[test]
    fn team_bazooka_splash_hurts_owner_but_not_teammate() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: 1,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        });
        for slot in 0..2 {
            let mut player = Player::new(slot, slot as u32 + 1, "127.0.0.1".into());
            player.status = PLAYER_STATUS_ALIVE;
            player.team_id = PLAYER_TEAM_BLUE;
            player.cf.position = [1.5, 1.5, 0.25];
            game.players[slot] = Some(player);
        }

        handle_explosion(
            &mut game,
            [1.5, 1.5, 0.25],
            [0.0, 0.0, 1.0],
            3.0,
            0,
            WEAPON_BAZOOKA,
        );

        assert!(game.players[0].as_ref().unwrap().life < 1.0);
        assert_eq!(game.players[1].as_ref().unwrap().life, 1.0);
    }

    #[test]
    fn damage_stat_is_credited_to_attacker_not_victim_or_self() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: 0,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        });
        for slot in 0..2 {
            let mut player = armed(WEAPON_SMG);
            player.slot = slot;
            player.status = PLAYER_STATUS_ALIVE;
            game.players[slot] = Some(player);
        }

        apply_damage(&mut game, 1, 0, WEAPON_SMG, 0.4);
        assert!((game.players[0].as_ref().unwrap().dmg - 0.4).abs() < f32::EPSILON);
        assert_eq!(game.players[1].as_ref().unwrap().dmg, 0.0);

        apply_damage(&mut game, 0, 0, WEAPON_SMG, 0.2);
        assert!((game.players[0].as_ref().unwrap().dmg - 0.4).abs() < f32::EPSILON);
    }

    #[test]
    fn bot_smg_cannot_damage_through_wall_on_reverse_diagonal() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: 0,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        });
        game.map_grid = crate::map::MapGrid::fixture(8, &[(3, 3)]);

        let mut bot = armed(WEAPON_SMG);
        bot.status = PLAYER_STATUS_ALIVE;
        bot.is_bot = true;
        bot.cf.position = [5.5, 5.5, 0.25];
        game.players[0] = Some(bot);

        let mut victim = armed(WEAPON_SMG);
        victim.status = PLAYER_STATUS_ALIVE;
        victim.cf.position = [1.5, 1.5, 0.25];
        game.players[1] = Some(victim);

        bot_fire(&mut game, 0, [1.5, 1.5]);

        assert_eq!(game.players[1].as_ref().unwrap().life, 1.0);
    }

    #[test]
    fn melee_cooldown_and_equipment_are_server_owned() {
        let mut knives = armed(0);
        knives.status = PLAYER_STATUS_ALIVE;
        knives.melee_id = WEAPON_KNIVES;
        assert_eq!(authorize_melee(&mut knives), Some(WEAPON_KNIVES));
        assert_eq!(authorize_melee(&mut knives), None);
        knives.melee_elapsed = 0.95;
        assert_eq!(authorize_melee(&mut knives), Some(WEAPON_KNIVES));

        let mut invalid = armed(0);
        invalid.status = PLAYER_STATUS_ALIVE;
        invalid.melee_id = 13;
        assert_eq!(authorize_melee(&mut invalid), None);
    }

    #[test]
    fn team_modes_reject_friendly_melee_damage() {
        assert!(can_damage_teams(
            0,
            Some(PLAYER_TEAM_BLUE),
            Some(PLAYER_TEAM_BLUE)
        ));
        assert!(!can_damage_teams(
            1,
            Some(PLAYER_TEAM_BLUE),
            Some(PLAYER_TEAM_BLUE)
        ));
        assert!(can_damage_teams(
            1,
            Some(PLAYER_TEAM_BLUE),
            Some(PLAYER_TEAM_RED)
        ));
        assert!(!can_damage_teams(2, Some(PLAYER_TEAM_RED), None));
    }

    #[test]
    fn team_mode_molotov_burns_owner_and_enemy_but_not_teammate() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: 2,
            max_players: 3,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        });
        for (slot, team, x) in [
            (0, PLAYER_TEAM_BLUE, 5.5),
            (1, PLAYER_TEAM_BLUE, 5.6),
            (2, PLAYER_TEAM_RED, 5.7),
        ] {
            let mut player = armed(WEAPON_SMG);
            player.slot = slot;
            player.team_id = team;
            player.status = PLAYER_STATUS_ALIVE;
            player.cf.position = [x, 5.5, 0.25];
            game.players[slot] = Some(player);
        }
        apply_server_burn(&mut game, [5.5, 5.5, 0.0], 0.5, 0);
        assert!(game.players[0].as_ref().unwrap().life < 1.0);
        assert_eq!(game.players[1].as_ref().unwrap().life, 1.0);
        assert!(game.players[2].as_ref().unwrap().life < 1.0);
    }

    #[test]
    fn teammate_blocks_direct_fire_without_taking_damage() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: crate::game::ctf::GAME_TYPE_CTF,
            max_players: 3,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        });
        game.map_grid = crate::map::MapGrid::fixture(8, &[]);
        let from = [2.5, 2.5, 0.25];
        let endpoint = spread_endpoint(from, [1.0, 0.0, 0.0], 128.0, 3.0, 1);
        let direction = [
            endpoint[0] - from[0],
            endpoint[1] - from[1],
            endpoint[2] - from[2],
        ];
        let length = direction[0].hypot(direction[1]).hypot(direction[2]);

        for (slot, team, distance) in [
            (0, PLAYER_TEAM_BLUE, 0.0),
            (1, PLAYER_TEAM_BLUE, 0.6),
            (2, PLAYER_TEAM_RED, 1.2),
        ] {
            let mut player = armed(WEAPON_SMG);
            player.slot = slot;
            player.team_id = team;
            player.status = PLAYER_STATUS_ALIVE;
            player.spawn_immunity_remaining = 0.0;
            player.cf.position = [
                from[0] + direction[0] / length * distance,
                from[1] + direction[1] / length * distance,
                from[2] + direction[2] / length * distance,
            ];
            game.players[slot] = Some(player);
        }

        let mut payload = [0u8; 16];
        payload[1] = WEAPON_SMG as u8;
        payload[4..6].copy_from_slice(&((from[0] * ids::WIRE_POS) as i16).to_le_bytes());
        payload[6..8].copy_from_slice(&((from[1] * ids::WIRE_POS) as i16).to_le_bytes());
        payload[8..10].copy_from_slice(&((from[2] * ids::WIRE_POS) as i16).to_le_bytes());
        payload[10..12].copy_from_slice(&(ids::WIRE_POS as i16).to_le_bytes());
        handle_player_shoot(&mut game, 0, &payload);

        assert_eq!(game.players[1].as_ref().unwrap().life, 1.0);
        assert_eq!(game.players[2].as_ref().unwrap().life, 1.0);
    }

    #[test]
    fn photon_visual_continues_through_player_to_wall() {
        let from = [1.0, 2.0, 0.25];
        let aimed = [9.0, 2.0, 0.25];
        let wall = [7.0, 2.0, 0.25];

        assert_eq!(
            visual_shot_endpoint(WEAPON_PHOTON_RIFLE, from, aimed, wall, 1, 0.25),
            wall
        );
        assert_eq!(
            visual_shot_endpoint(WEAPON_SMG, from, aimed, wall, 1, 0.25),
            [3.0, 2.0, 0.25]
        );
    }

    #[test]
    fn molotov_burn_stacks_increase_damage_per_tick() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: 0,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        });
        let mut attacker = armed(WEAPON_MOLOTOV);
        attacker.slot = 0;
        attacker.status = PLAYER_STATUS_ALIVE;
        attacker.team_id = PLAYER_TEAM_BLUE;
        attacker.cf.position = [5.5, 5.5, 0.25];
        attacker.spawn_immunity_remaining = 0.0;
        game.players[0] = Some(attacker);

        let mut victim = armed(WEAPON_SMG);
        victim.slot = 1;
        victim.status = PLAYER_STATUS_ALIVE;
        victim.team_id = PLAYER_TEAM_RED;
        victim.cf.position = [5.5, 5.5, 0.25];
        victim.spawn_immunity_remaining = 0.0;
        game.players[1] = Some(victim);

        apply_server_burn(&mut game, [5.5, 5.5, 0.25], 0.5, 0);
        let first_life = game.players[1].as_ref().unwrap().life;
        apply_server_burn(&mut game, [5.5, 5.5, 0.25], 0.5, 0);
        let second_life = game.players[1].as_ref().unwrap().life;

        assert!((first_life - 0.85).abs() < 1e-5);
        assert!((second_life - 0.6625).abs() < 1e-4);
        assert_eq!(game.players[1].as_ref().unwrap().flame_burn_stacks, 2);
    }

    #[test]
    fn molotov_kill_awards_burning_streak_to_attacker() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: 0,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        });
        let mut attacker = armed(WEAPON_MOLOTOV);
        attacker.slot = 0;
        attacker.status = PLAYER_STATUS_ALIVE;
        attacker.team_id = PLAYER_TEAM_BLUE;
        attacker.cf.position = [5.5, 5.5, 0.25];
        attacker.spawn_immunity_remaining = 0.0;
        game.players[0] = Some(attacker);

        let mut victim = armed(WEAPON_SMG);
        victim.slot = 1;
        victim.status = PLAYER_STATUS_ALIVE;
        victim.team_id = PLAYER_TEAM_RED;
        victim.life = 0.1;
        victim.cf.position = [5.5, 5.5, 0.25];
        victim.spawn_immunity_remaining = 0.0;
        game.players[1] = Some(victim);

        apply_server_burn(&mut game, [5.5, 5.5, 0.25], 0.5, 0);
        assert_eq!(game.players[0].as_ref().unwrap().molotov_kill_streak, 1);
        assert!(game.players[0].as_ref().unwrap().molotov_kill_streak_decay > 0.0);
        assert!(game.players[1].as_ref().unwrap().status == PLAYER_STATUS_DEAD);

        let mut second_victim = armed(WEAPON_SMG);
        second_victim.slot = 1;
        second_victim.status = PLAYER_STATUS_ALIVE;
        second_victim.team_id = PLAYER_TEAM_RED;
        second_victim.cf.position = [5.5, 5.5, 0.25];
        second_victim.spawn_immunity_remaining = 0.0;
        second_victim.life = 0.5;
        game.players[1] = Some(second_victim);
        apply_server_burn(&mut game, [5.5, 5.5, 0.25], 0.5, 0);

        assert!((game.players[1].as_ref().unwrap().life - 0.32).abs() < 1e-4);
    }

    #[test]
    fn shield_halves_damage_until_final_point_six_seconds() {
        assert_eq!(shield_damage_multiplier(2.0), 0.5);
        assert_eq!(shield_damage_multiplier(0.61), 0.5);
        assert_eq!(shield_damage_multiplier(0.6), 1.0);
    }

    #[test]
    fn native_spawn_immunity_blocks_damage_until_final_point_three_seconds() {
        assert_eq!(protected_damage(0.5, 0.0, 2.0), 0.0);
        assert_eq!(protected_damage(0.5, 0.0, 0.31), 0.0);
        assert_eq!(protected_damage(0.5, 0.0, 0.3), 0.5);
    }

    #[test]
    fn authoritative_grenade_pickup_allows_an_additional_throw() {
        let mut player = armed(0);
        player.grenades_left = 0;
        player.grenade_elapsed = f32::INFINITY;
        assert!(!authorize_projectile_fire(&mut player, 8, 3));
        player.grenades_left += 1;
        assert!(authorize_projectile_fire(&mut player, 8, 3));
        assert_eq!(player.grenades_left, 0);
    }

    #[test]
    fn flamethrower_stream_keeps_fixed_play_tuned_range() {
        let mut player = armed(WEAPON_FLAME_THROWER);
        assert_eq!(
            authorize_direct_fire(&mut player, WEAPON_FLAME_THROWER),
            Some(0)
        );
        assert_eq!(player.flame_seconds_fired, 0.0);
        player.elapsed_since_last_shot = 0.1;
        assert_eq!(
            authorize_direct_fire(&mut player, WEAPON_FLAME_THROWER),
            Some(0)
        );
        assert!(player.flame_seconds_fired > 0.0);
        let range = FLAME_MAX_RANGE;
        assert_eq!(range, 3.0);
    }

    #[test]
    fn old_school_flame_segment_can_hit_multiple_targets_at_full_damage() {
        let from = [1.5, 1.5, 0.25];
        let to = [1.5, 4.5, 0.25];
        let targets = [[1.5, 2.4, 0.25], [1.8, 3.5, 0.25]];
        let hits = targets
            .iter()
            .filter(|target| segment_sphere_hit_t(from, to, **target, 0.5).is_some())
            .count();
        assert_eq!(hits, 2);
        assert_eq!(FLAME_DAMAGE, 0.08);
    }

    #[test]
    fn authoritative_old_school_flame_damages_every_intersected_babo() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_DM,
            max_players: 3,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        });
        game.map_grid = crate::map::MapGrid::fixture(8, &[]);

        let from = [2.5, 2.5, 0.25];
        let endpoint = spread_endpoint(from, [0.0, 1.0, 0.0], FLAME_MAX_RANGE, 10.0, 1);
        for slot in 0..3 {
            let mut player = armed(if slot == 0 {
                WEAPON_FLAME_THROWER
            } else {
                WEAPON_SMG
            });
            player.slot = slot;
            player.status = PLAYER_STATUS_ALIVE;
            player.spawn_immunity_remaining = 0.0;
            let t = slot as f32 * 0.32;
            player.cf.position = [
                from[0] + (endpoint[0] - from[0]) * t,
                from[1] + (endpoint[1] - from[1]) * t,
                0.25,
            ];
            game.players[slot] = Some(player);
        }

        let mut payload = [0u8; 16];
        payload[1] = WEAPON_FLAME_THROWER as u8;
        payload[4..6].copy_from_slice(&((from[0] * ids::WIRE_POS) as i16).to_le_bytes());
        payload[6..8].copy_from_slice(&((from[1] * ids::WIRE_POS) as i16).to_le_bytes());
        payload[8..10].copy_from_slice(&((from[2] * ids::WIRE_POS) as i16).to_le_bytes());
        payload[12..14].copy_from_slice(&(ids::WIRE_POS as i16).to_le_bytes());
        handle_player_shoot(&mut game, 0, &payload);

        assert!((game.players[1].as_ref().unwrap().life - 0.92).abs() < 1e-5);
        assert!((game.players[2].as_ref().unwrap().life - 0.92).abs() < 1e-5);
    }

    #[test]
    fn falling_below_the_map_kills_through_spawn_immunity() {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: 0,
            max_players: 1,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        });
        let mut player = armed(WEAPON_SMG);
        player.status = PLAYER_STATUS_ALIVE;
        player.spawn_immunity_remaining = 2.0;
        game.players[0] = Some(player);

        apply_world_damage(&mut game, 0, 13, 2.0);

        let player = game.players[0].as_ref().unwrap();
        assert_eq!(player.status, PLAYER_STATUS_DEAD);
        assert_eq!(player.life, 0.0);
    }
}
