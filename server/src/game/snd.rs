//! Native `_PRO_` Search & Destroy is the "Champion" ruleset.  Despite the
//! old non-Pro UI/map data retaining bomb objectives, `ServerSnD.cpp` is empty:
//! the shipped server uses timed spawn groups and a forced mini-round reset.

use super::{
    shoot::broadcast_enum, GameServer, GAME_PLAYING, GAME_TYPE_SND, MAX_PLAYER, PLAYER_STATUS_DEAD,
    PLAYER_TEAM_BLUE, PLAYER_TEAM_RED,
};
use crate::net::{ids, packet};

pub const SND_ROUND_TIME: f32 = 3.0 * 60.0;
const SPAWNS_PER_SLOT: usize = 5;
const SECTION_SAFE_CAPACITY: usize = 4;

pub fn rebalance_sections(game: &mut GameServer) {
    let usable_sections = (game.map_spawns.dm.len() / SPAWNS_PER_SLOT).max(1);
    let active: Vec<usize> = game
        .players
        .iter()
        .enumerate()
        .filter_map(|(slot, player)| {
            player
                .as_ref()
                .filter(|player| matches!(player.team_id, PLAYER_TEAM_BLUE | PLAYER_TEAM_RED))
                .map(|_| slot)
        })
        .collect();
    if active.is_empty() {
        return;
    }
    let section_count = active
        .len()
        .div_ceil(SECTION_SAFE_CAPACITY)
        .min(usable_sections);
    for (index, player_slot) in active.into_iter().enumerate() {
        // Round-robin produces balanced 3+2 rather than 4+1 for five players.
        if let Some(player) = game.players[player_slot].as_mut() {
            player.snd_spawn_slot = Some(index % section_count);
        }
    }
}

pub fn pick_spawn(game: &mut GameServer, player_id: usize) -> [f32; 3] {
    if game.map_spawns.dm.is_empty() {
        return [0.0, 0.0, 0.25];
    }

    let existing = game.players[player_id]
        .as_ref()
        .and_then(|player| player.snd_spawn_slot);
    if existing.is_none() {
        rebalance_sections(game);
    }
    let slot = game.players[player_id]
        .as_ref()
        .and_then(|player| player.snd_spawn_slot)
        .unwrap_or(0);
    if let Some(player) = game.players[player_id].as_mut() {
        player.snd_spawn_slot = Some(slot);
    }
    let spawns = &game.map_spawns.dm;

    // GameSpawn.cpp groups five consecutive DM points per slot and chooses a
    // point within that group. Frame/player IDs give deterministic variety.
    let group_start = slot.saturating_mul(SPAWNS_PER_SLOT);
    let within = (game.frame_id as usize + player_id) % SPAWNS_PER_SLOT;
    let index = (group_start + within).min(spawns.len() - 1);
    let point = spawns[index];
    [point[0], point[1], 0.25]
}

pub fn tick_snd(game: &mut GameServer) {
    if game.config.game_type != GAME_TYPE_SND
        || game.round_state != GAME_PLAYING
        || game.round_time_left > 0.0
    {
        return;
    }

    game.round_time_left = SND_ROUND_TIME;
    for projectile in &game.projectiles {
        game.broadcast(packet::encode_frame(
            ids::NET_SVCL_DELETE_PROJECTILE,
            &projectile.unique_id.to_le_bytes(),
        ));
    }
    game.projectiles.clear();
    for slot in 0..MAX_PLAYER {
        if let Some(player) = game.players[slot].as_mut() {
            player.snd_spawn_slot = None;
            if matches!(player.team_id, PLAYER_TEAM_BLUE | PLAYER_TEAM_RED) {
                player.status = PLAYER_STATUS_DEAD;
                player.life = 0.0;
                player.time_to_spawn = 0.0;
                player.bot_respawn_timer = 0.0;
            }
        }
        broadcast_enum(game, slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MapSpawns, ServerConfig};
    use crate::game::{Player, PLAYER_STATUS_ALIVE, PLAYER_TEAM_BLUE};
    use std::path::PathBuf;

    fn game() -> GameServer {
        let mut game = GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("missing"),
            map_name: "fixture".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_SND,
            max_players: 8,
            bot_count: 0,
            score_limit: 50,
            win_limit: 7,
            game_time_limit: 900.0,
        });
        game.map_spawns = MapSpawns {
            dm: (0..12).map(|x| [x as f32, 1.0, 0.0]).collect(),
            ..Default::default()
        };
        for slot in 0..2 {
            let mut player = Player::new(slot, slot as u32 + 1, String::new());
            player.team_id = PLAYER_TEAM_BLUE;
            player.status = PLAYER_STATUS_ALIVE;
            game.players[slot] = Some(player);
        }
        game
    }

    #[test]
    fn first_two_champion_players_share_a_populated_section() {
        let mut game = game();
        let first = pick_spawn(&mut game, 0);
        let second = pick_spawn(&mut game, 1);
        assert!(first[0] < 5.0);
        assert!(second[0] < 5.0);
        assert_eq!(game.players[0].as_ref().unwrap().snd_spawn_slot, Some(0));
        assert_eq!(game.players[1].as_ref().unwrap().snd_spawn_slot, Some(0));
    }

    #[test]
    fn champion_populations_balance_without_singleton_sections() {
        let mut game = game();
        game.map_spawns.dm = (0..80).map(|x| [x as f32, 1.0, 0.0]).collect();
        for population in 1..=16 {
            for slot in 0..16 {
                game.players[slot] = (slot < population).then(|| {
                    let mut player = Player::new(slot, slot as u32 + 1, String::new());
                    player.team_id = PLAYER_TEAM_BLUE;
                    player
                });
            }
            rebalance_sections(&mut game);
            let mut counts = [0usize; 16];
            for player in game.players.iter().flatten() {
                counts[player.snd_spawn_slot.unwrap()] += 1;
            }
            let populated: Vec<_> = counts.into_iter().filter(|count| *count > 0).collect();
            assert!(populated
                .iter()
                .all(|count| *count <= SECTION_SAFE_CAPACITY));
            if population > 1 {
                assert!(populated.iter().all(|count| *count >= 2));
            }
            assert!(populated.iter().max().unwrap() - populated.iter().min().unwrap() <= 1);
        }
    }

    #[test]
    fn champion_timer_resets_every_player_and_spawn_slot() {
        let mut game = game();
        game.players[0].as_mut().unwrap().snd_spawn_slot = Some(3);
        game.round_time_left = 0.0;
        tick_snd(&mut game);
        assert_eq!(game.round_time_left, SND_ROUND_TIME);
        assert_eq!(game.players[0].as_ref().unwrap().status, PLAYER_STATUS_DEAD);
        assert_eq!(game.players[0].as_ref().unwrap().snd_spawn_slot, None);
        assert_eq!(game.players[0].as_ref().unwrap().time_to_spawn, 0.0);
    }
}
