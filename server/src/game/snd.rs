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

pub fn pick_spawn(game: &mut GameServer, player_id: usize) -> [f32; 3] {
    let spawns = &game.map_spawns.dm;
    if spawns.is_empty() {
        return [0.0, 0.0, 0.25];
    }

    let existing = game.players[player_id]
        .as_ref()
        .and_then(|player| player.snd_spawn_slot);
    let slot = existing.unwrap_or_else(|| {
        let used: Vec<usize> = game
            .players
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != player_id)
            .filter_map(|(_, player)| {
                let player = player.as_ref()?;
                matches!(player.team_id, PLAYER_TEAM_BLUE | PLAYER_TEAM_RED)
                    .then_some(player.snd_spawn_slot)
                    .flatten()
            })
            .collect();
        (0..MAX_PLAYER)
            .find(|candidate| !used.contains(candidate))
            .unwrap_or(0)
    });
    if let Some(player) = game.players[player_id].as_mut() {
        player.snd_spawn_slot = Some(slot);
    }

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
    fn champion_players_keep_distinct_five_point_spawn_groups() {
        let mut game = game();
        let first = pick_spawn(&mut game, 0);
        let second = pick_spawn(&mut game, 1);
        assert!(first[0] < 5.0);
        assert!((5.0..10.0).contains(&second[0]));
        assert_eq!(game.players[0].as_ref().unwrap().snd_spawn_slot, Some(0));
        assert_eq!(game.players[1].as_ref().unwrap().snd_spawn_slot, Some(1));
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
