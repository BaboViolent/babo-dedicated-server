//! Server-owned projectile simulation ported from GameProjectile.cpp.

use crate::game::{shoot, GameServer, MAX_PLAYER, PLAYER_STATUS_ALIVE};
use crate::map::{segment_hit_t, segment_sphere_hit_t};
use crate::net::{ids, packet};

const ROCKET: i8 = 2;
const GRENADE: i8 = 3;
const MOLOTOV: i8 = 7;
const FLAME: i8 = 8;
const LIFE_PICKUP: i8 = 4;
const WEAPON_PICKUP: i8 = 5;
const GRENADE_PICKUP: i8 = 6;
const GRAVITY: f32 = 9.8;
const BOUNCE: f32 = 0.65;
const ROCKET_MAX_SPEED: f32 = 10.0;
const MAX_ORIGIN_DISTANCE: f32 = 1.0;
const FLAME_BURN_INTERVAL: f32 = 20.0 / 30.0;
const ROCKET_EXPLOSION_RADIUS: f32 = 2.0;
const ROCKET_REMOTE_DETONATE_DELAY: f32 = 0.25;

#[derive(Debug, Clone)]
pub struct ServerProjectile {
    pub unique_id: i32,
    pub owner: usize,
    pub weapon_id: i8,
    pub kind: i8,
    pub position: [f32; 3],
    pub velocity: [f32; 3],
    pub launch_direction: [f32; 3],
    pub duration: f32,
    pub frame_id: i32,
    pub burn_clock: f32,
    pub time_since_thrown: f32,
    pub movement_lock: bool,
    pub stick_to_player: i8,
    pub stick_for: f32,
}

impl ServerProjectile {
    fn moving(
        unique_id: i32,
        owner: usize,
        weapon_id: i8,
        kind: i8,
        position: [f32; 3],
        direction: [f32; 3],
    ) -> Self {
        let (scale, z_boost, duration) = match kind {
            ROCKET => (2.5, 0.0, 10.0),
            GRENADE => (5.0, 5.0, 2.0),
            MOLOTOV => (6.0, 2.0, 10.0),
            _ => (0.0, 0.0, 0.0),
        };
        Self {
            unique_id,
            owner,
            weapon_id,
            kind,
            position,
            velocity: [
                direction[0] * scale,
                direction[1] * scale,
                direction[2] * scale + z_boost,
            ],
            launch_direction: direction,
            duration,
            frame_id: 0,
            burn_clock: 0.0,
            time_since_thrown: 0.0,
            movement_lock: false,
            stick_to_player: -1,
            stick_for: 0.0,
        }
    }

    fn flame(
        unique_id: i32,
        owner: usize,
        position: [f32; 3],
        velocity: [f32; 3],
        movement_lock: bool,
    ) -> Self {
        Self {
            unique_id,
            owner,
            weapon_id: 9,
            kind: FLAME,
            position,
            velocity,
            launch_direction: velocity,
            duration: 10.0,
            frame_id: 0,
            burn_clock: 0.0,
            time_since_thrown: 0.0,
            movement_lock,
            stick_to_player: -1,
            stick_for: 0.0,
        }
    }

    fn pickup(
        unique_id: i32,
        owner: usize,
        kind: i8,
        weapon_id: i8,
        position: [f32; 3],
        velocity: [f32; 3],
    ) -> Self {
        Self {
            unique_id,
            owner,
            weapon_id,
            kind,
            position,
            velocity,
            launch_direction: velocity,
            duration: match kind {
                LIFE_PICKUP => 20.0,
                WEAPON_PICKUP => 30.0,
                _ => 25.0,
            },
            frame_id: 0,
            burn_clock: 0.0,
            time_since_thrown: 0.0,
            movement_lock: false,
            stick_to_player: -1,
            stick_for: 0.0,
        }
    }

    fn integrate(&mut self, dt: f32) {
        self.frame_id += 1;
        self.duration -= dt;
        if self.kind == FLAME {
            self.burn_clock += dt;
            self.time_since_thrown += dt;
            if !self.movement_lock {
                let speed = magnitude(self.velocity);
                if speed > 0.5 || self.position[2] > 0.2 {
                    add_scaled(&mut self.position, self.velocity, dt);
                    self.velocity[2] -= GRAVITY * dt;
                }
            }
            return;
        }
        if self.kind == ROCKET {
            let speed = magnitude(self.velocity);
            if speed > ROCKET_MAX_SPEED {
                scale_to(&mut self.velocity, ROCKET_MAX_SPEED);
            }
            add_scaled(&mut self.position, self.velocity, dt);
            for v in &mut self.velocity {
                *v += *v * dt * 3.0;
            }
        } else {
            add_scaled(&mut self.position, self.velocity, dt);
            self.velocity[2] -= GRAVITY * dt;
        }
    }
}

fn death_drop_velocity(unique_id: i32, player_velocity: [f32; 3]) -> [f32; 3] {
    let seed = unique_id as u32;
    let yaw = (seed.wrapping_mul(2_654_435_761) as f32 / u32::MAX as f32) * std::f32::consts::TAU;
    let pitch =
        (((seed ^ 0xa511_e9b3).wrapping_mul(2_246_822_519) as f32 / u32::MAX as f32) * 90.0 - 45.0)
            .to_radians();
    let cp = pitch.cos();
    let sp = pitch.sin();
    [
        yaw.sin() * sp * 3.0 + player_velocity[0] * 0.25,
        yaw.cos() * sp * 3.0 + player_velocity[1] * 0.25,
        cp * 3.0 + player_velocity[2] * 0.25,
    ]
}

pub fn spawn_death_pickups(
    game: &mut GameServer,
    owner: usize,
    position: [f32; 3],
    player_velocity: [f32; 3],
    weapon_id: i8,
    grenades: u8,
) {
    let drops = std::iter::once((LIFE_PICKUP, 0))
        .chain(std::iter::once((WEAPON_PICKUP, weapon_id)))
        .chain(std::iter::repeat_n((GRENADE_PICKUP, 0), grenades as usize));
    for (kind, drop_weapon) in drops {
        let id = game.next_projectile_id;
        game.next_projectile_id = game.next_projectile_id.wrapping_add(1).max(1);
        let velocity = death_drop_velocity(id, player_velocity);
        let pickup = ServerProjectile::pickup(id, owner, kind, drop_weapon, position, velocity);
        game.broadcast(packet::encode_frame(
            ids::NET_CLSV_SVCL_PLAYER_PROJECTILE,
            &spawn_payload(&pickup, 0),
        ));
        game.projectiles.push(pickup);
    }
}

pub fn spawn_weapon_pickup(
    game: &mut GameServer,
    owner: usize,
    weapon_id: i8,
    position: [f32; 3],
    player_velocity: [f32; 3],
) {
    let id = game.next_projectile_id;
    game.next_projectile_id = game.next_projectile_id.wrapping_add(1).max(1);
    let pickup = ServerProjectile::pickup(
        id,
        owner,
        WEAPON_PICKUP,
        weapon_id,
        position,
        death_drop_velocity(id, player_velocity),
    );
    game.broadcast(packet::encode_frame(
        ids::NET_CLSV_SVCL_PLAYER_PROJECTILE,
        &spawn_payload(&pickup, 0),
    ));
    game.projectiles.push(pickup);
}

enum Event {
    Explosion {
        position: [f32; 3],
        normal: [f32; 3],
        radius: f32,
        owner: usize,
        weapon: i8,
    },
    Flames {
        position: [f32; 3],
        normal: [f32; 3],
        scatter_vel: [f32; 3],
        owner: usize,
    },
    Burn {
        position: [f32; 3],
        owner: usize,
    },
    FlameStick {
        unique_id: i32,
        player: i8,
    },
    Pickup {
        unique_id: i32,
        player: usize,
        kind: i8,
        weapon_id: i8,
    },
}

pub fn handle_spawn_intent(game: &mut GameServer, shooter: usize, payload: &[u8]) {
    if game.round_state != crate::game::GAME_PLAYING {
        return;
    }
    if payload.len() < 17 {
        return;
    }
    let Some(player) = game.players[shooter].as_ref() else {
        return;
    };
    if player.status != PLAYER_STATUS_ALIVE {
        return;
    }
    let weapon_id = payload[1] as i8;
    let kind = payload[3] as i8;
    let requested_position = [
        packet::read_i16(payload, 4).unwrap_or(0) as f32 / ids::WIRE_POS,
        packet::read_i16(payload, 6).unwrap_or(0) as f32 / ids::WIRE_POS,
        packet::read_i16(payload, 8).unwrap_or(0) as f32 / ids::WIRE_POS,
    ];
    let origin_distance = distance(player.cf.position, requested_position);
    if origin_distance > MAX_ORIGIN_DISTANCE {
        tracing::warn!(
            "projectile rejected: shooter={shooter} bad origin distance={origin_distance:.2}"
        );
        return;
    }
    let (clip, _) = game
        .map_grid
        .ray_test(player.cf.position, requested_position);
    let position = clamp_nearby_muzzle_origin(player.cf.position, requested_position, clip);
    let mut direction = [
        payload[10] as i8 as f32 / ids::WIRE_VEL,
        payload[11] as i8 as f32 / ids::WIRE_VEL,
        payload[12] as i8 as f32 / ids::WIRE_VEL,
    ];
    let speed = magnitude(direction);
    if !(0.5..=1.5).contains(&speed) {
        tracing::warn!("projectile rejected: shooter={shooter} bad direction magnitude={speed:.2}");
        return;
    }
    scale_to(&mut direction, 1.0);
    if kind == ROCKET {
        let ready_for_remote = if let Some(player) = game.players[shooter].as_ref() {
            player.elapsed_since_last_shot >= ROCKET_REMOTE_DETONATE_DELAY
        } else {
            false
        };
        if ready_for_remote {
            if let Some(position) = take_remote_detonation(game, shooter) {
                shoot::handle_explosion(
                    game,
                    position,
                    [0.0, 0.0, 1.0],
                    ROCKET_EXPLOSION_RADIUS,
                    shooter as i8,
                    5,
                );
                return;
            }
        }
        if game
            .projectiles
            .iter()
            .any(|p| p.owner == shooter && p.kind == ROCKET && p.duration > 0.0)
        {
            return;
        }
    }
    let authorized = {
        let Some(player) = game.players[shooter].as_mut() else {
            return;
        };
        shoot::authorize_projectile_fire(player, weapon_id, kind)
    };
    if !authorized {
        tracing::debug!("projectile rejected: shooter={shooter} weapon={weapon_id} type={kind} cooldown/equipment");
        return;
    }

    let unique_id = game.next_projectile_id;
    game.next_projectile_id = game.next_projectile_id.wrapping_add(1).max(1);
    let projectile =
        ServerProjectile::moving(unique_id, shooter, weapon_id, kind, position, direction);
    let body = spawn_payload(&projectile, payload[2] as i8);
    game.projectiles.push(projectile);
    game.broadcast(packet::encode_frame(
        ids::NET_CLSV_SVCL_PLAYER_PROJECTILE,
        &body,
    ));
}

fn bot_throw(game: &mut GameServer, shooter: usize, target: [f32; 2], weapon: i8, kind: i8) {
    let Some(player) = game.players.get(shooter).and_then(Option::as_ref) else {
        return;
    };
    let position = player.cf.position;
    let dx = target[0] - position[0];
    let dy = target[1] - position[1];
    let length = dx.hypot(dy).max(0.01);
    let direction = [dx / length, dy / length, 0.0];
    let mut payload = vec![0u8; 17];
    payload[0] = shooter as u8;
    payload[1] = weapon as u8;
    payload[3] = kind as u8;
    for axis in 0..3 {
        payload[4 + axis * 2..6 + axis * 2]
            .copy_from_slice(&((position[axis] * ids::WIRE_POS) as i16).to_le_bytes());
        payload[10 + axis] = (direction[axis] * ids::WIRE_VEL) as i8 as u8;
    }
    handle_spawn_intent(game, shooter, &payload);
}

pub fn bot_throw_grenade(game: &mut GameServer, shooter: usize, target: [f32; 2]) {
    bot_throw(game, shooter, target, 8, GRENADE);
}

pub fn bot_throw_molotov(game: &mut GameServer, shooter: usize, target: [f32; 2]) {
    bot_throw(game, shooter, target, 9, MOLOTOV);
}

pub fn bot_fire_rocket(game: &mut GameServer, shooter: usize, target: [f32; 2]) {
    bot_throw(game, shooter, target, 5, ROCKET);
}

fn take_remote_detonation(game: &mut GameServer, owner: usize) -> Option<[f32; 3]> {
    let index = game
        .projectiles
        .iter()
        .position(|p| p.owner == owner && p.kind == ROCKET && p.duration > 0.0)?;
    Some(game.projectiles.remove(index).position)
}

fn clamp_nearby_muzzle_origin(
    player_position: [f32; 3],
    requested: [f32; 3],
    clipped: [f32; 3],
) -> [f32; 3] {
    if distance(player_position, requested) <= 1e-3
        || segment_hit_t(player_position, requested, clipped) >= 0.999
    {
        return requested;
    }
    // A weapon muzzle can extend through a nearby wall while the player's body
    // remains valid. Start just on the player side so the first simulation tick
    // visibly rebounds at the real surface instead of rejecting the throw.
    let mut toward_player = [
        player_position[0] - clipped[0],
        player_position[1] - clipped[1],
        player_position[2] - clipped[2],
    ];
    scale_to(&mut toward_player, 0.01);
    [
        clipped[0] + toward_player[0],
        clipped[1] + toward_player[1],
        clipped[2] + toward_player[2],
    ]
}

pub fn tick_projectiles(game: &mut GameServer, dt: f32) {
    let mut active = std::mem::take(&mut game.projectiles);
    let mut survivors = Vec::with_capacity(active.len());
    let mut events = Vec::new();
    let mut deleted = Vec::new();

    for mut projectile in active.drain(..) {
        let old = projectile.position;
        projectile.integrate(dt);

        if projectile.kind == FLAME {
            let previous_stick = projectile.stick_to_player;
            if projectile.stick_to_player >= 0 {
                if let Some(stick_player) = game
                    .players
                    .get(projectile.stick_to_player as usize)
                    .and_then(Option::as_ref)
                {
                    if stick_player.status == PLAYER_STATUS_ALIVE {
                        projectile.position = [
                            stick_player.cf.position[0],
                            stick_player.cf.position[1],
                            0.0,
                        ];
                    } else {
                        projectile.stick_to_player = -1;
                        projectile.position = [
                            stick_player.cf.position[0],
                            stick_player.cf.position[1],
                            0.0,
                        ];
                        projectile.stick_for = 0.0;
                        projectile.movement_lock = true;
                    }
                } else {
                    projectile.stick_to_player = -1;
                    projectile.stick_for = 0.0;
                    projectile.movement_lock = true;
                }
            }

            if projectile.stick_to_player >= 0 {
                projectile.stick_for -= dt;
                if projectile.stick_for <= 0.0 {
                    projectile.stick_to_player = -1;
                    projectile.stick_for = 1.0;
                    projectile.movement_lock = false;
                }
            } else if projectile.stick_for > 0.0 {
                projectile.stick_for -= dt;
            }

            if projectile.stick_to_player < 0 && !projectile.movement_lock {
                let (wall_point, normal) = game.map_grid.ray_test(old, projectile.position);
                let hit_wall = segment_hit_t(old, projectile.position, wall_point) < 0.999;
                if hit_wall || projectile.position[2] <= 0.0 {
                    if hit_wall {
                        projectile.position = [
                            wall_point[0] + normal[0] * 0.1,
                            wall_point[1] + normal[1] * 0.1,
                            wall_point[2] + normal[2] * 0.1,
                        ];
                    } else {
                        projectile.position[2] = 0.0;
                    }
                    projectile.velocity = [0.0; 3];
                    projectile.movement_lock = true;
                }
            }

            if projectile.stick_to_player < 0 && projectile.stick_for <= 0.0 {
                let owner = if projectile.time_since_thrown > 0.5 {
                    None
                } else {
                    Some(projectile.owner as i8)
                };
                let target_stick = (0..MAX_PLAYER).find_map(|slot| {
                    let player = game.players[slot].as_ref()?;
                    if player.status != PLAYER_STATUS_ALIVE {
                        return None;
                    }
                    if owner.is_some_and(|id| id == slot as i8) {
                        return None;
                    }
                    let radius = 0.5 + 0.25;
                    if (player.cf.position[0] - projectile.position[0]).powi(2)
                        + (player.cf.position[1] - projectile.position[1]).powi(2)
                        + (player.cf.position[2] - projectile.position[2]).powi(2)
                        <= radius * radius
                    {
                        Some(slot as i8)
                    } else {
                        None
                    }
                });
                if let Some(slot) = target_stick {
                    projectile.movement_lock = true;
                    projectile.stick_for = 3.0;
                    projectile.stick_to_player = slot;
                }
            }

            if projectile.stick_to_player != previous_stick {
                events.push(Event::FlameStick {
                    unique_id: projectile.unique_id,
                    player: projectile.stick_to_player,
                });
            }

            if projectile.burn_clock >= FLAME_BURN_INTERVAL {
                projectile.burn_clock -= FLAME_BURN_INTERVAL;
                events.push(Event::Burn {
                    position: projectile.position,
                    owner: projectile.owner,
                });
            }
            if projectile.duration > 0.0 {
                survivors.push(projectile);
            } else {
                deleted.push(projectile.unique_id);
            }
            continue;
        }

        // Native GameProjectile owns life/grenade proximity collection.  Do
        // this on the authoritative copy instead of waiting for a browser to
        // notice the overlap and request it.
        if matches!(projectile.kind, LIFE_PICKUP | GRENADE_PICKUP) {
            let collector = (0..MAX_PLAYER).find(|&slot| {
                game.players[slot].as_ref().is_some_and(|player| {
                    player.status == PLAYER_STATUS_ALIVE
                        && (player.cf.position[0] - projectile.position[0])
                            .hypot(player.cf.position[1] - projectile.position[1])
                            <= 0.5
                })
            });
            if let Some(player) = collector {
                events.push(Event::Pickup {
                    unique_id: projectile.unique_id,
                    player,
                    kind: projectile.kind,
                    weapon_id: projectile.weapon_id,
                });
                continue;
            }
        }

        let (wall_point, normal) = game.map_grid.ray_test(old, projectile.position);
        let hit_wall = segment_hit_t(old, projectile.position, wall_point) < 0.999;
        let hit_player = if projectile.kind == ROCKET || projectile.kind == MOLOTOV {
            (0..MAX_PLAYER)
                .filter_map(|slot| {
                    if slot == projectile.owner {
                        return None;
                    }
                    let target = game.players[slot].as_ref()?;
                    if target.status != PLAYER_STATUS_ALIVE {
                        return None;
                    }
                    let radius = 0.5;
                    segment_sphere_hit_t(old, projectile.position, target.cf.position, radius)
                        .map(|t| (t, target.cf.position))
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
        } else {
            None
        };

        if projectile.kind == ROCKET && (hit_wall || hit_player.is_some()) {
            let position = hit_player.map(|(_, p)| p).unwrap_or(wall_point);
            events.push(Event::Explosion {
                position,
                normal,
                radius: ROCKET_EXPLOSION_RADIUS,
                owner: projectile.owner,
                weapon: 5,
            });
            deleted.push(projectile.unique_id);
            continue;
        }
        if hit_wall
            && matches!(
                projectile.kind,
                GRENADE | LIFE_PICKUP | WEAPON_PICKUP | GRENADE_PICKUP
            )
        {
            projectile.position = [
                wall_point[0] + normal[0] * 0.01,
                wall_point[1] + normal[1] * 0.01,
                wall_point[2] + normal[2] * 0.01,
            ];
            projectile.velocity = reflect(projectile.velocity, normal);
            for v in &mut projectile.velocity {
                *v *= BOUNCE;
            }
        } else if (hit_wall || hit_player.is_some() || projectile.duration <= 0.0)
            && projectile.kind == MOLOTOV
        {
            events.push(Event::Flames {
                position: hit_player.map(|(_, p)| p).unwrap_or(wall_point),
                normal,
                scatter_vel: if hit_wall {
                    reflect(projectile.velocity, normal)
                } else {
                    reflect(projectile.velocity, [0.0, 0.0, 1.0])
                },
                owner: projectile.owner,
            });
            deleted.push(projectile.unique_id);
            continue;
        }
        if projectile.kind == GRENADE && projectile.duration <= 0.0 {
            events.push(Event::Explosion {
                position: projectile.position,
                normal: [0.0, 0.0, 1.0],
                radius: 1.5,
                owner: projectile.owner,
                weapon: 8,
            });
            deleted.push(projectile.unique_id);
            continue;
        }
        if projectile.duration <= 0.0 {
            deleted.push(projectile.unique_id);
            continue;
        }
        game.broadcast(projectile_coord_frame(&projectile));
        survivors.push(projectile);
    }

    game.projectiles = survivors;
    for unique_id in deleted {
        game.broadcast(packet::encode_frame(
            ids::NET_SVCL_DELETE_PROJECTILE,
            &unique_id.to_le_bytes(),
        ));
    }
    for event in events {
        match event {
            Event::Explosion {
                position,
                normal,
                radius,
                owner,
                weapon,
            } => {
                shoot::handle_explosion(game, position, normal, radius, owner as i8, weapon);
            }
            Event::Flames {
                position,
                normal,
                scatter_vel,
                owner,
            } => spawn_flames(game, position, normal, scatter_vel, owner),
            Event::Burn { position, owner } => shoot::apply_server_burn(game, position, 0.5, owner),
            Event::FlameStick { unique_id, player } => {
                game.broadcast(packet::flame_stick_to_player(unique_id, player));
            }
            Event::Pickup {
                unique_id,
                player,
                kind,
                weapon_id,
            } => {
                if let Some(p) = game.players[player].as_mut() {
                    let item_type = if kind == LIFE_PICKUP {
                        p.life = (p.life + 0.5).min(1.0);
                        1
                    } else {
                        p.grenades_left = (p.grenades_left + 1).min(3);
                        3
                    };
                    game.broadcast(packet::encode_frame(
                        ids::NET_SVCL_PICKUP_ITEM,
                        &[player as u8, item_type, weapon_id as u8],
                    ));
                    game.broadcast(packet::encode_frame(
                        ids::NET_SVCL_DELETE_PROJECTILE,
                        &unique_id.to_le_bytes(),
                    ));
                    shoot::broadcast_enum(game, player);
                }
            }
        }
    }
}

fn spawn_flames(
    game: &mut GameServer,
    position: [f32; 3],
    normal: [f32; 3],
    scatter_vel: [f32; 3],
    owner: usize,
) {
    let base_id = game.next_projectile_id;
    game.next_projectile_id = game.next_projectile_id.wrapping_add(1).max(1);
    let base = ServerProjectile::flame(base_id, owner, position, [0.0, 0.0, 0.0], true);
    game.broadcast(packet::encode_frame(
        ids::NET_CLSV_SVCL_PLAYER_PROJECTILE,
        &spawn_payload(&base, 0),
    ));
    game.projectiles.push(base);

    let scatter = {
        let v = speed(scatter_vel);
        if v > 0.001 {
            [
                scatter_vel[0] + ((base_id as f32 * 97.0) % 2.0) - 1.0,
                scatter_vel[1] + (((base_id + 31) as f32 * 97.0) % 2.0) - 1.0,
                scatter_vel[2] + ((base_id as f32 * 53.0) % 1.0),
            ]
        } else {
            reflect([0.0, 0.0, 1.0], normal)
        }
    };
    let scatter_speed = speed(scatter);
    if scatter_speed > 0.01 {
        let scatter_id = game.next_projectile_id;
        game.next_projectile_id = game.next_projectile_id.wrapping_add(1).max(1);
        let scatter_flame = ServerProjectile::flame(scatter_id, owner, position, scatter, false);
        game.broadcast(packet::encode_frame(
            ids::NET_CLSV_SVCL_PLAYER_PROJECTILE,
            &spawn_payload(&scatter_flame, 0),
        ));
        game.projectiles.push(scatter_flame);
    }
}

fn spawn_payload(p: &ServerProjectile, muzzle: i8) -> Vec<u8> {
    let mut out = vec![0u8; 17];
    out[0] = p.owner as u8;
    out[1] = p.weapon_id as u8;
    out[2] = muzzle as u8;
    out[3] = p.kind as u8;
    for i in 0..3 {
        out[4 + i * 2..6 + i * 2]
            .copy_from_slice(&((p.position[i] * ids::WIRE_POS) as i16).to_le_bytes());
        out[10 + i] = (p.launch_direction[i] * ids::WIRE_VEL) as i8 as u8;
    }
    out[13..17].copy_from_slice(&p.unique_id.to_le_bytes());
    out
}

pub fn snapshot_frames(projectiles: &[ServerProjectile]) -> Vec<Vec<u8>> {
    projectiles
        .iter()
        .map(|projectile| {
            packet::encode_frame(
                ids::NET_CLSV_SVCL_PLAYER_PROJECTILE,
                &spawn_payload(projectile, 0),
            )
        })
        .collect()
}

fn projectile_coord_frame(p: &ServerProjectile) -> Vec<u8> {
    let mut out = vec![0u8; 24];
    out[0..4].copy_from_slice(&p.unique_id.to_le_bytes());
    out[4..6].copy_from_slice(&0i16.to_le_bytes());
    out[8..12].copy_from_slice(&p.frame_id.to_le_bytes());
    for i in 0..3 {
        out[12 + i * 2..14 + i * 2]
            .copy_from_slice(&((p.position[i] * ids::WIRE_POS) as i16).to_le_bytes());
        out[18 + i] = (p.velocity[i] * ids::WIRE_VEL).clamp(-127.0, 127.0) as i8 as u8;
    }
    packet::encode_frame(ids::NET_SVCL_PROJECTILE_COORD_FRAME, &out)
}

fn magnitude(v: [f32; 3]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}
fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    magnitude([a[0] - b[0], a[1] - b[1], a[2] - b[2]])
}
fn speed(v: [f32; 3]) -> f32 {
    magnitude(v)
}
fn scale_to(v: &mut [f32; 3], length: f32) {
    let k = length / magnitude(*v).max(1e-6);
    for value in v {
        *value *= k;
    }
}
fn add_scaled(position: &mut [f32; 3], velocity: [f32; 3], dt: f32) {
    for i in 0..3 {
        position[i] += velocity[i] * dt;
    }
}
fn reflect(v: [f32; 3], n: [f32; 3]) -> [f32; 3] {
    let d = v[0] * n[0] + v[1] * n[1] + v[2] * n[2];
    [
        v[0] - 2.0 * d * n[0],
        v[1] - 2.0 * d * n[1],
        v[2] - 2.0 * d * n[2],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::game::{Player, PLAYER_STATUS_DEAD, PLAYER_TEAM_BLUE};
    use std::path::PathBuf;

    #[test]
    fn rocket_accelerates_and_caps_before_integration() {
        let mut p = ServerProjectile::moving(1, 0, 5, ROCKET, [0.0; 3], [0.0, 1.0, 0.0]);
        p.integrate(0.02);
        assert!((p.position[1] - 0.05).abs() < 1e-5);
        for _ in 0..100 {
            p.integrate(0.02);
        }
        assert!(magnitude(p.velocity) <= ROCKET_MAX_SPEED * 1.061);
    }

    #[test]
    fn grenade_has_legacy_launch_and_gravity() {
        let mut p = ServerProjectile::moving(1, 0, 8, GRENADE, [0.0; 3], [0.0, 1.0, 0.0]);
        assert_eq!(p.velocity, [0.0, 5.0, 5.0]);
        p.integrate(0.02);
        assert!((p.velocity[2] - 4.804).abs() < 1e-4);
    }

    #[test]
    fn molotov_flame_patch_settles_to_floor_after_wall_impact() {
        let flame = ServerProjectile::flame(1, 0, [4.0, 5.0, 1.25], [0.0, 0.0, 0.0], false);
        assert_eq!(flame.position, [4.0, 5.0, 1.25]);
        assert_eq!(flame.duration, 10.0);
    }

    #[test]
    fn grenade_reflection_uses_point_six_five_bounce() {
        let reflected = reflect([2.0, -3.0, 1.0], [0.0, 1.0, 0.0]);
        let bounced = reflected.map(|v| v * BOUNCE);
        assert_eq!(bounced, [1.3, 1.9499999, 0.65]);
    }

    #[test]
    fn nearby_muzzle_crossing_is_clamped_to_visible_wall() {
        let player = [1.5, 1.5, 0.25];
        let requested = [2.1, 1.5, 0.25];
        let origin = clamp_nearby_muzzle_origin(player, requested, [2.0, 1.5, 0.25]);
        assert!((origin[0] - 1.99).abs() < 1e-5);
        assert_eq!(origin[1], 1.5);
    }

    #[test]
    fn unobstructed_muzzle_origin_is_unchanged() {
        let requested = [1.9, 1.5, 0.25];
        assert_eq!(
            clamp_nearby_muzzle_origin([1.5, 1.5, 0.25], requested, requested),
            requested
        );
    }

    #[test]
    fn fuse_counts_down_on_server() {
        let mut p = ServerProjectile::moving(1, 0, 8, GRENADE, [0.0; 3], [0.0, 1.0, 0.0]);
        for _ in 0..100 {
            p.integrate(0.02);
        }
        assert!(p.duration <= 0.001);
    }

    #[test]
    fn spawn_intent_becomes_server_owned_state() {
        let config = ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-BurialMound".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: 0,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        };
        let mut game = GameServer::new(config);
        let mut player = Player::new(0, 1, "127.0.0.1".into());
        player.status = PLAYER_STATUS_ALIVE;
        player.team_id = PLAYER_TEAM_BLUE;
        player.weapon_id = 5;
        player.cf.position = game.pick_spawn_for(PLAYER_TEAM_BLUE, None);
        game.players[0] = Some(player);

        let origin = game.players[0].as_ref().unwrap().cf.position;
        let mut payload = vec![0u8; 17];
        payload[0] = 0;
        payload[1] = 5;
        payload[3] = ROCKET as u8;
        for i in 0..3 {
            payload[4 + i * 2..6 + i * 2]
                .copy_from_slice(&((origin[i] * ids::WIRE_POS) as i16).to_le_bytes());
        }
        payload[10] = 10; // +X unit direction at wire ×10

        handle_spawn_intent(&mut game, 0, &payload);
        assert_eq!(game.projectiles.len(), 1);
        assert_eq!(game.projectiles[0].unique_id, 1);
        assert_eq!(game.projectiles[0].owner, 0);
        assert_eq!(game.projectiles[0].kind, ROCKET);
        assert_eq!(game.next_projectile_id, 2);

        // Replaying immediately is rejected by the server fire clock.
        handle_spawn_intent(&mut game, 0, &payload);
        assert_eq!(game.projectiles.len(), 1);

        game.projectiles[0].duration = 9.7;
        game.players[0].as_mut().unwrap().elapsed_since_last_shot = 0.5;
        handle_spawn_intent(&mut game, 0, &payload);
        assert!(
            game.projectiles.is_empty(),
            "second click remotely detonates the owned rocket"
        );
    }

    #[test]
    fn server_collects_health_drop_without_client_request() {
        let config = ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-BurialMound".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: 0,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        };
        let mut game = GameServer::new(config);
        let mut player = Player::new(0, 1, "127.0.0.1".into());
        player.status = PLAYER_STATUS_ALIVE;
        player.life = 0.25;
        player.cf.position = game.pick_spawn_for(PLAYER_TEAM_BLUE, None);
        game.players[0] = Some(player);
        let position = game.players[0].as_ref().unwrap().cf.position;
        game.projectiles.push(ServerProjectile::pickup(
            77,
            1,
            LIFE_PICKUP,
            0,
            position,
            [0.0; 3],
        ));

        tick_projectiles(&mut game, 1.0 / 30.0);

        assert_eq!(game.players[0].as_ref().unwrap().life, 0.75);
        assert!(game.projectiles.is_empty());
    }

    #[test]
    fn authoritative_flame_projectile_periodically_hurts_nearby_player() {
        let config = ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-BurialMound".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: 0,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        };
        let mut game = GameServer::new(config);
        let position = game.pick_spawn_for(PLAYER_TEAM_BLUE, None);
        let mut victim = Player::new(0, 1, "127.0.0.1".into());
        victim.status = PLAYER_STATUS_ALIVE;
        victim.life = 1.0;
        victim.cf.position = position;
        victim.spawn_immunity_remaining = 0.0;
        game.players[0] = Some(victim);
        game.projectiles.push(ServerProjectile::flame(
            88,
            1,
            position,
            [0.0, 0.0, 0.0],
            false,
        ));

        tick_projectiles(&mut game, FLAME_BURN_INTERVAL);

        assert!(game.players[0].as_ref().unwrap().life < 1.0);
        assert_eq!(
            game.projectiles.len(),
            1,
            "flame persists after a burn tick"
        );
    }

    #[test]
    fn flame_stays_on_ground_when_carrier_dies() {
        let config = ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-BurialMound".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: 0,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 15.0 * 60.0,
        };
        let mut game = GameServer::new(config);
        let mut victim = Player::new(0, 1, "127.0.0.1".into());
        victim.status = PLAYER_STATUS_ALIVE;
        victim.cf.position = [4.0, 5.0, 0.25];
        game.players[0] = Some(victim);

        let mut flame = ServerProjectile::flame(9, 0, [4.0, 5.0, 0.0], [0.0; 3], true);
        flame.stick_to_player = 0;
        flame.movement_lock = true;
        flame.stick_for = 3.0;
        flame.time_since_thrown = 0.6;
        game.projectiles.push(flame);

        if let Some(dead_carrier) = game.players[0].as_mut() {
            dead_carrier.status = PLAYER_STATUS_DEAD;
        }

        tick_projectiles(&mut game, 1.0 / 30.0);
        let mut flame = game.projectiles[0].clone();
        let grounded = flame.position;
        assert_eq!(grounded[2], 0.0);

        if let Some(dead_carrier) = game.players[0].as_mut() {
            dead_carrier.cf.position = [12.0, 8.0, 0.25];
        }
        tick_projectiles(&mut game, 1.0 / 30.0);

        flame = game.projectiles[0].clone();
        assert_eq!(flame.stick_to_player, -1);
        assert!(flame.movement_lock);
        assert_eq!(flame.position, grounded);
    }
}
