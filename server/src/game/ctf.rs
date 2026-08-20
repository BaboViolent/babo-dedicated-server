//! CTF flag rules — mirrors jsclient `ctf.js` / ClientRecv.cpp flag packets.

use crate::game::shoot::{self, broadcast_enum};
use crate::game::{
    player_spawn_bytes, GAME_BLUE_WIN, GAME_DRAW, GAME_PLAYING, GAME_RED_WIN, GAME_TYPE_SND,
    MAX_PLAYER, PLAYER_STATUS_ALIVE, PLAYER_TEAM_BLUE, PLAYER_TEAM_RED, PLAYER_TEAM_SPECTATOR,
    SPAWN_IMMUNITY_TIME,
};
use crate::net::{ids, packet};
use crate::GameServer;

pub const GAME_TYPE_CTF: i8 = 2;
pub const FLAG_AT_POD: i8 = -2;
pub const FLAG_DROPPED: i8 = -1;
const MATCH_END_DELAY: f32 = 10.0;

const TAKE_RADIUS: f32 = 0.25;
const DROP_RADIUS: f32 = 0.5;

#[derive(Debug, Clone)]
pub struct CtfState {
    pub flag_state: [i8; 2],
    pub flag_pos: [[f32; 3]; 2],
    pub flag_pod: [[f32; 3]; 2],
}

impl CtfState {
    pub fn new(flag_pod: [[f32; 3]; 2]) -> Self {
        let mut s = Self {
            flag_state: [FLAG_AT_POD, FLAG_AT_POD],
            flag_pos: flag_pod,
            flag_pod,
        };
        s.reset();
        s
    }

    pub fn reset(&mut self) {
        self.flag_state = [FLAG_AT_POD, FLAG_AT_POD];
        self.flag_pos = self.flag_pod;
    }

    pub fn supports_ctf(&self) -> bool {
        self.flag_pod[0] != [0.0, 0.0, 0.0] && self.flag_pod[1] != [0.0, 0.0, 0.0]
    }

    pub fn carrier_flag(&self, slot: usize) -> Option<usize> {
        (0..2).find(|&i| self.flag_state[i] == slot as i8)
    }
}

pub fn map_flag_pod(spawns: &crate::config::MapSpawns) -> [[f32; 3]; 2] {
    spawns.flag_pod
}

pub fn tick_ctf(game: &mut GameServer) {
    if game.config.game_type != GAME_TYPE_CTF
        || game.round_state != GAME_PLAYING
        || !game.ctf.supports_ctf()
    {
        return;
    }

    sync_carrier_positions(game);

    for slot in 0..MAX_PLAYER {
        let Some(p) = game.players[slot].as_ref() else {
            continue;
        };
        if p.status != PLAYER_STATUS_ALIVE {
            continue;
        }
        let team = p.team_id;
        let pos = p.cf.position;
        tick_blue_flag(game, slot, team, pos);
        tick_red_flag(game, slot, team, pos);
    }

    sync_carrier_positions(game);
}

fn sync_carrier_positions(game: &mut GameServer) {
    for i in 0..2 {
        let st = game.ctf.flag_state[i];
        if st == FLAG_AT_POD {
            game.ctf.flag_pos[i] = game.ctf.flag_pod[i];
        } else if st >= 0 {
            let slot = st as usize;
            if let Some(p) = game.players[slot].as_ref() {
                if p.status == PLAYER_STATUS_ALIVE {
                    game.ctf.flag_pos[i] = p.cf.position;
                }
            }
        }
    }
}

fn dist_sq_xy(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    dx * dx + dy * dy
}

fn tick_blue_flag(game: &mut GameServer, slot: usize, team: i8, pos: [f32; 3]) {
    let pod = game.ctf.flag_pod[0];
    let state = game.ctf.flag_state[0];

    if state == FLAG_AT_POD {
        if team == PLAYER_TEAM_RED && dist_sq_xy(pos, pod) <= TAKE_RADIUS * TAKE_RADIUS {
            take_flag(game, 0, slot);
            return;
        }
        if team == PLAYER_TEAM_BLUE
            && game.ctf.flag_state[1] == slot as i8
            && dist_sq_xy(pos, pod) <= TAKE_RADIUS * TAKE_RADIUS
        {
            capture(game, slot, PLAYER_TEAM_BLUE);
        }
    } else if state == FLAG_DROPPED {
        let drop = game.ctf.flag_pos[0];
        if team == PLAYER_TEAM_RED && dist_sq_xy(pos, drop) <= DROP_RADIUS * DROP_RADIUS {
            take_flag(game, 0, slot);
        } else if team == PLAYER_TEAM_BLUE && dist_sq_xy(pos, drop) <= DROP_RADIUS * DROP_RADIUS {
            return_flag(game, 0, slot);
        }
    }
}

fn tick_red_flag(game: &mut GameServer, slot: usize, team: i8, pos: [f32; 3]) {
    let pod = game.ctf.flag_pod[1];
    let state = game.ctf.flag_state[1];

    if state == FLAG_AT_POD {
        if team == PLAYER_TEAM_BLUE && dist_sq_xy(pos, pod) <= TAKE_RADIUS * TAKE_RADIUS {
            take_flag(game, 1, slot);
            return;
        }
        if team == PLAYER_TEAM_RED
            && game.ctf.flag_state[0] == slot as i8
            && dist_sq_xy(pos, pod) <= TAKE_RADIUS * TAKE_RADIUS
        {
            capture(game, slot, PLAYER_TEAM_RED);
        }
    } else if state == FLAG_DROPPED {
        let drop = game.ctf.flag_pos[1];
        if team == PLAYER_TEAM_BLUE && dist_sq_xy(pos, drop) <= DROP_RADIUS * DROP_RADIUS {
            take_flag(game, 1, slot);
        } else if team == PLAYER_TEAM_RED && dist_sq_xy(pos, drop) <= DROP_RADIUS * DROP_RADIUS {
            return_flag(game, 1, slot);
        }
    }
}

fn take_flag(game: &mut GameServer, flag_id: usize, slot: usize) {
    let previous_state = game.ctf.flag_state[flag_id];
    game.ctf.flag_state[flag_id] = slot as i8;
    if let Some(p) = game.players[slot].as_mut() {
        p.flag_attempts += 1;
    }
    log_flag_transition(game, "take", flag_id, slot, previous_state);
    broadcast_change_flag(game, flag_id as i8, slot as i8, slot);
    broadcast_enum(game, slot);
}

fn return_flag(game: &mut GameServer, flag_id: usize, slot: usize) {
    let previous_state = game.ctf.flag_state[flag_id];
    game.ctf.flag_state[flag_id] = FLAG_AT_POD;
    game.ctf.flag_pos[flag_id] = game.ctf.flag_pod[flag_id];
    if let Some(p) = game.players[slot].as_mut() {
        p.returns += 1;
    }
    log_flag_transition(game, "return", flag_id, slot, previous_state);
    broadcast_change_flag(game, flag_id as i8, FLAG_AT_POD, slot);
    broadcast_enum(game, slot);
}

fn capture(game: &mut GameServer, slot: usize, team: i8) {
    if game.round_state != GAME_PLAYING {
        return;
    }
    if team == PLAYER_TEAM_BLUE {
        game.ctf.flag_state[1] = FLAG_AT_POD;
        game.ctf.flag_pos[1] = game.ctf.flag_pod[1];
        game.blue_win += 1;
        game.blue_score = game.blue_win;
        broadcast_change_flag(game, 1, FLAG_AT_POD, slot);
    } else {
        game.ctf.flag_state[0] = FLAG_AT_POD;
        game.ctf.flag_pos[0] = game.ctf.flag_pod[0];
        game.red_win += 1;
        game.red_score = game.red_win;
        broadcast_change_flag(game, 0, FLAG_AT_POD, slot);
    }
    if let Some(p) = game.players[slot].as_mut() {
        p.score += 1;
    }
    let flag_id = usize::from(team == PLAYER_TEAM_BLUE);
    log_flag_transition(game, "capture", flag_id, slot, slot as i8);
    broadcast_enum(game, slot);
    finish_match_if_needed(game);
}

fn winning_state(blue: i16, red: i16, limit: i16) -> Option<i8> {
    if limit <= 0 || (blue < limit && red < limit) {
        None
    } else if blue == red {
        Some(GAME_DRAW)
    } else if blue > red {
        Some(GAME_BLUE_WIN)
    } else {
        Some(GAME_RED_WIN)
    }
}

fn finish_match_if_needed(game: &mut GameServer) {
    if game.round_state != GAME_PLAYING {
        return;
    }
    let Some(state) = winning_state(game.blue_win, game.red_win, game.config.win_limit) else {
        return;
    };
    start_round_end(game, state);
}

fn start_round_end(game: &mut GameServer, state: i8) {
    if game.round_state == state {
        return;
    }
    game.round_state = state;
    game.round_end_remaining = MATCH_END_DELAY;
    freeze_players(game);
    game.broadcast(packet::round_state(state, false));
}

fn freeze_players(game: &mut GameServer) {
    for slot in 0..MAX_PLAYER {
        let Some(player) = game.players[slot].as_mut() else {
            continue;
        };
        if player.team_id == PLAYER_TEAM_SPECTATOR {
            continue;
        }
        // Intermission is a server-side freeze, not a mass death. Keeping the
        // player alive preserves the final scoreboard/body state and avoids a
        // fake death/respawn cycle immediately before the next map.
        player.cf.vel = [0.0, 0.0, 0.0];
        player.time_to_spawn = 0.0;
        player.bot_respawn_timer = 0.0;
        player.spawned_once = true;
        broadcast_enum(game, slot);
    }
}

fn reset_player_spawn(game: &mut GameServer, slot: usize) {
    let team_id = match game.players.get(slot).and_then(|p| p.as_ref()) {
        Some(player) => player.team_id,
        None => return,
    };
    let pos = if game.config.game_type == GAME_TYPE_SND {
        crate::game::snd::pick_spawn(game, slot)
    } else {
        game.pick_spawn_for(team_id, Some(slot))
    };
    let Some(player) = game.players[slot].as_mut() else {
        return;
    };
    player.status = PLAYER_STATUS_ALIVE;
    player.cf.position = [pos[0], pos[1], pos[2]];
    player.cf.vel = [0.0, 0.0, 0.0];
    player.time_to_spawn = 0.0;
    player.bot_respawn_timer = 0.0;
    player.spawned_once = true;
    player.elapsed_since_last_shot = f32::INFINITY;
    player.weapon_current_imp = shoot::weapon_start_spread(player.weapon_id);
    player.burst_shot_count = 0;
    player.flame_seconds_fired = 0.0;
    player.grenade_elapsed = f32::INFINITY;
    player.molotov_elapsed = f32::INFINITY;
    player.grenades_left = 2;
    player.molotovs_left = 1;
    player.melee_elapsed = f32::INFINITY;
    player.life = 1.0;
    player.spawn_immunity_remaining = SPAWN_IMMUNITY_TIME;
    if game.config.game_type == GAME_TYPE_SND {
        player.snd_spawn_slot = None;
    }
}

fn respawn_all_players(game: &mut GameServer) {
    for slot in 0..MAX_PLAYER {
        let is_spectator = game
            .players
            .get(slot)
            .and_then(Option::as_ref)
            .is_some_and(|p| p.team_id == PLAYER_TEAM_SPECTATOR);
        if is_spectator {
            continue;
        }
        reset_player_spawn(game, slot);
        let Some(player) = game.players[slot].as_ref() else {
            continue;
        };
        let spawn_bytes = player_spawn_bytes(slot, player);
        game.broadcast(packet::encode_frame(
            ids::NET_SVCL_PLAYER_SPAWN,
            &spawn_bytes,
        ));
        broadcast_enum(game, slot);
    }
}

pub fn tick_match_lifecycle(game: &mut GameServer, dt: f32) {
    let mut just_finished = false;
    if game.round_state == GAME_PLAYING {
        let scores = match game.config.game_type {
            GAME_TYPE_CTF => (game.blue_win, game.red_win),
            1 => (game.blue_score, game.red_score),
            _ => {
                let high_score = game
                    .players
                    .iter()
                    .flatten()
                    .map(|p| p.score)
                    .max()
                    .unwrap_or(0);
                (high_score, 0)
            }
        };
        let limit = if game.config.game_type == GAME_TYPE_CTF {
            game.config.win_limit
        } else {
            game.config.score_limit
        };
        let reached_limit = limit > 0 && (scores.0 >= limit || scores.1 >= limit);
        let timed_out = game.config.game_time_limit > 0.0 && game.game_time_left <= 0.0;
        if reached_limit || timed_out {
            let state = if scores.0 == scores.1 {
                GAME_DRAW
            } else if scores.0 > scores.1 {
                GAME_BLUE_WIN
            } else {
                GAME_RED_WIN
            };
            start_round_end(game, state);
            just_finished = true;
        }
    }
    if game.round_state == GAME_PLAYING || just_finished {
        return;
    }
    game.round_end_remaining = (game.round_end_remaining - dt).max(0.0);
    if game.round_end_remaining > 0.0 {
        return;
    }
    reset_match(game);
}

fn reset_match(game: &mut GameServer) {
    game.rotate_to_next_map();
    restart_match(game);
}

pub(crate) fn restart_match(game: &mut GameServer) {
    game.blue_score = 0;
    game.red_score = 0;
    game.blue_win = 0;
    game.red_win = 0;
    game.game_time_left = game.config.game_time_limit;
    game.round_time_left = if game.config.game_type == GAME_TYPE_SND {
        crate::game::snd::SND_ROUND_TIME
    } else {
        600.0
    };
    game.round_state = GAME_PLAYING;
    game.round_end_remaining = 0.0;
    game.ctf.reset();
    game.projectiles.clear();
    game.broadcast(packet::round_state(GAME_PLAYING, true));
    game.broadcast(packet::map_change(
        &game.config.map_name,
        game.config.game_type,
    ));
    for slot in 0..MAX_PLAYER {
        if let Some(player) = game.players[slot].as_mut() {
            player.kills = 0;
            player.deaths = 0;
            player.score = 0;
            player.returns = 0;
            player.flag_attempts = 0;
            player.damage = 0;
            player.dmg = 0.0;
            player.life = 0.0;
            player.bot_respawn_timer = 0.0;
        }
        broadcast_enum(game, slot);
    }
    respawn_all_players(game);
    broadcast_flag_enum(game);
}

pub fn drop_carrier_flags(game: &mut GameServer, slot: usize) {
    if game.config.game_type != GAME_TYPE_CTF {
        return;
    }
    while let Some(flag_id) = game.ctf.carrier_flag(slot) {
        if let Some(p) = game.players[slot].as_ref() {
            game.ctf.flag_pos[flag_id] = p.cf.position;
        }
        game.ctf.flag_state[flag_id] = FLAG_DROPPED;
        log_flag_transition(game, "drop", flag_id, slot, slot as i8);
        game.broadcast(packet::drop_flag(flag_id as i8, game.ctf.flag_pos[flag_id]));
    }
}

fn log_flag_transition(
    game: &GameServer,
    action: &'static str,
    flag_id: usize,
    slot: usize,
    previous_state: i8,
) {
    let actor = game.players.get(slot).and_then(Option::as_ref);
    let name = actor
        .map(|player| player.name.as_str())
        .unwrap_or("unknown");
    let team = actor.map(|player| player.team_id).unwrap_or(-1);
    let bot = actor.is_some_and(|player| player.is_bot);
    let position = game.ctf.flag_pos[flag_id];
    tracing::info!(
        target: "bv2_server::ctf",
        action,
        flag_id,
        flag = if flag_id == 0 { "blue" } else { "red" },
        actor_slot = slot,
        actor_name = name,
        actor_team = team,
        actor_bot = bot,
        previous_state,
        new_state = game.ctf.flag_state[flag_id],
        x = position[0],
        y = position[1],
        "flag transition"
    );
}

fn broadcast_change_flag(game: &GameServer, flag_id: i8, new_state: i8, player_id: usize) {
    game.broadcast(packet::change_flag_state(
        flag_id,
        new_state,
        player_id as i8,
    ));
}

pub fn flag_enum_bytes(game: &GameServer) -> Vec<u8> {
    let mut p = vec![0u8; 26];
    p[0] = game.ctf.flag_state[0] as u8;
    p[1] = game.ctf.flag_state[1] as u8;
    p[2..6].copy_from_slice(&game.ctf.flag_pos[0][0].to_le_bytes());
    p[6..10].copy_from_slice(&game.ctf.flag_pos[0][1].to_le_bytes());
    p[10..14].copy_from_slice(&game.ctf.flag_pos[0][2].to_le_bytes());
    p[14..18].copy_from_slice(&game.ctf.flag_pos[1][0].to_le_bytes());
    p[18..22].copy_from_slice(&game.ctf.flag_pos[1][1].to_le_bytes());
    p[22..26].copy_from_slice(&game.ctf.flag_pos[1][2].to_le_bytes());
    p
}

pub fn send_flag_enum_to(game: &GameServer, babonet_id: u32) {
    if game.config.game_type != GAME_TYPE_CTF || !game.ctf.supports_ctf() {
        return;
    }
    game.send_to(
        babonet_id,
        packet::encode_frame(ids::NET_SVCL_FLAG_ENUM, &flag_enum_bytes(game)),
    );
}

fn broadcast_flag_enum(game: &GameServer) {
    game.broadcast(packet::encode_frame(
        ids::NET_SVCL_FLAG_ENUM,
        &flag_enum_bytes(game),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::game::Player;
    use std::path::PathBuf;

    fn game() -> GameServer {
        GameServer::new(ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            content_root: PathBuf::from("../Content/main"),
            map_name: "CTF-Alert".into(),
            game_name: "test".into(),
            password: String::new(),
            game_type: GAME_TYPE_CTF,
            max_players: 2,
            bot_count: 0,
            score_limit: 50,
            win_limit: 1,
            game_time_limit: 15.0 * 60.0,
        })
    }

    fn add_blue(game: &mut GameServer) {
        let mut player = Player::new(0, 1, "127.0.0.1".into());
        player.team_id = PLAYER_TEAM_BLUE;
        player.status = PLAYER_STATUS_ALIVE;
        player.cf.position = game.ctf.flag_pod[0];
        game.players[0] = Some(player);
    }

    #[test]
    fn win_limit_ends_match_exactly_once() {
        let mut game = game();
        add_blue(&mut game);
        capture(&mut game, 0, PLAYER_TEAM_BLUE);
        assert_eq!(game.blue_win, 1);
        assert_eq!(game.round_state, GAME_BLUE_WIN);
        assert_eq!(game.round_end_remaining, MATCH_END_DELAY);
        assert_eq!(game.players[0].as_ref().unwrap().score, 1);
        assert_eq!(
            game.players[0].as_ref().unwrap().status,
            PLAYER_STATUS_ALIVE
        );
        assert_eq!(game.players[0].as_ref().unwrap().cf.vel, [0.0; 3]);

        capture(&mut game, 0, PLAYER_TEAM_BLUE);
        assert_eq!(game.blue_win, 1);
        assert_eq!(game.round_end_remaining, MATCH_END_DELAY);
    }

    #[test]
    fn returning_own_dropped_flag_increments_returns_only() {
        let mut game = game();
        add_blue(&mut game);
        game.ctf.flag_state[0] = FLAG_DROPPED;
        game.ctf.flag_pos[0] = game.players[0].as_ref().unwrap().cf.position;

        return_flag(&mut game, 0, 0);

        let player = game.players[0].as_ref().unwrap();
        assert_eq!(player.returns, 1);
        assert_eq!(player.score, 0);
        assert_eq!(player.dmg, 0.0);
        assert_eq!(game.ctf.flag_state[0], FLAG_AT_POD);
    }

    #[test]
    fn ended_match_resets_scores_players_and_flags() {
        let mut game = game();
        let old_map = game.config.map_name.clone();
        add_blue(&mut game);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        game.register_session(99, tx);
        capture(&mut game, 0, PLAYER_TEAM_BLUE);
        while rx.try_recv().is_ok() {}
        tick_match_lifecycle(&mut game, MATCH_END_DELAY);

        assert_eq!(game.round_state, GAME_PLAYING);
        if game.map_list.len() > 1 {
            assert_ne!(game.config.map_name, old_map);
        }
        assert_eq!((game.blue_win, game.red_win), (0, 0));
        assert_eq!((game.blue_score, game.red_score), (0, 0));
        assert_eq!(game.game_time_left, 15.0 * 60.0);
        assert_eq!(game.ctf.flag_state, [FLAG_AT_POD, FLAG_AT_POD]);
        assert_eq!(game.ctf.flag_pos, game.ctf.flag_pod);
        let player = game.players[0].as_ref().unwrap();
        assert_eq!(player.status, PLAYER_STATUS_ALIVE);
        assert_eq!(player.score, 0);
        assert_eq!(player.time_to_spawn, 0.0);

        let frames: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|bytes| packet::decode_frame(&bytes).unwrap().unwrap())
            .collect();
        let round_state_index = frames.iter().position(|frame| {
            frame.type_id == ids::NET_SVCL_GAME_STATE && frame.payload == [GAME_PLAYING as u8, 1]
        });
        let map_change_index = frames.iter().position(|frame| {
            frame.type_id == ids::NET_SVCL_MAP_CHANGE
                && packet::read_str(&frame.payload, 0, 16) == game.config.map_name
        });
        assert_eq!(
            round_state_index,
            Some(0),
            "round-state reinit should be sent immediately on match reset"
        );
        if game.map_list.len() > 1 {
            assert!(
                map_change_index.is_some(),
                "expected map_change after reinit"
            );
            assert!(
                map_change_index.unwrap_or(usize::MAX) > round_state_index.unwrap_or(usize::MAX)
            );
        }
        assert!(frames
            .iter()
            .any(|frame| frame.type_id == ids::NET_SVCL_FLAG_ENUM));
    }

    #[test]
    fn timer_expiry_declares_team_winner_and_full_intermission() {
        let mut game = game();
        game.blue_win = 3;
        game.red_win = 2;
        game.game_time_left = 0.0;

        tick_match_lifecycle(&mut game, 0.02);

        assert_eq!(game.round_state, GAME_BLUE_WIN);
        assert_eq!(game.round_end_remaining, MATCH_END_DELAY);
    }

    #[test]
    fn tied_team_deathmatch_timer_declares_draw() {
        let mut game = game();
        game.config.game_type = 1;
        game.blue_score = 12;
        game.red_score = 12;
        game.game_time_left = 0.0;

        tick_match_lifecycle(&mut game, 0.02);

        assert_eq!(game.round_state, GAME_DRAW);
        assert_eq!(game.round_end_remaining, MATCH_END_DELAY);
    }

    #[test]
    fn deathmatch_modes_use_the_classic_fifty_point_score_limit() {
        let mut tdm = game();
        tdm.config.game_type = 1;
        tdm.blue_score = 49;
        tick_match_lifecycle(&mut tdm, 0.02);
        assert_eq!(tdm.round_state, GAME_PLAYING);
        tdm.blue_score = 50;
        tick_match_lifecycle(&mut tdm, 0.02);
        assert_eq!(tdm.round_state, GAME_BLUE_WIN);

        let mut ffa = game();
        ffa.config.game_type = 0;
        let mut leader = Player::new(0, 1, "leader".into());
        leader.score = 50;
        ffa.players[0] = Some(leader);
        tick_match_lifecycle(&mut ffa, 0.02);
        assert_ne!(ffa.round_state, GAME_PLAYING);
    }

    #[test]
    fn game_clock_stops_during_match_end_intermission() {
        let mut game = game();
        game.round_state = GAME_BLUE_WIN;
        game.round_end_remaining = MATCH_END_DELAY;
        game.game_time_left = 123.0;

        game.tick(1.0);

        assert_eq!(game.game_time_left, 123.0);
    }

    #[test]
    fn champion_map_change_restores_three_minute_round_timer() {
        let mut game = game();
        game.config.game_type = GAME_TYPE_SND;
        game.round_state = GAME_BLUE_WIN;
        game.round_end_remaining = 0.0;
        game.round_time_left = 0.0;

        tick_match_lifecycle(&mut game, 0.02);

        assert_eq!(game.round_state, GAME_PLAYING);
        assert_eq!(game.round_time_left, crate::game::snd::SND_ROUND_TIME);
    }

    #[test]
    fn team_spawn_uses_dm_points_farthest_from_live_enemy() {
        let mut game = game();
        game.map_spawns.dm = vec![[1.5, 1.5, 0.25], [12.5, 12.5, 0.25]];
        let mut enemy = Player::new(1, 2, "127.0.0.1".into());
        enemy.team_id = PLAYER_TEAM_RED;
        enemy.status = PLAYER_STATUS_ALIVE;
        enemy.cf.position = [2.0, 2.0, 0.25];
        game.players[1] = Some(enemy);
        assert_eq!(
            game.pick_spawn_for(PLAYER_TEAM_BLUE, Some(0)),
            [12.5, 12.5, 0.25]
        );
    }

    #[test]
    fn disconnect_drops_carried_flag_before_removing_player() {
        let mut game = game();
        add_blue(&mut game);
        game.players[0].as_mut().unwrap().cf.position = [7.0, 8.0, 0.25];
        game.ctf.flag_state[1] = 0;
        game.disconnect_babonet(1);
        assert_eq!(game.ctf.flag_state[1], FLAG_DROPPED);
        assert_eq!(game.ctf.flag_pos[1], [7.0, 8.0, 0.25]);
        assert!(game.players[0].is_none());
    }

    #[test]
    fn configured_zero_win_limit_is_unlimited() {
        assert_eq!(winning_state(99, 0, 0), None);
        assert_eq!(winning_state(7, 6, 7), Some(GAME_BLUE_WIN));
        assert_eq!(winning_state(7, 7, 7), Some(GAME_DRAW));
    }
}
