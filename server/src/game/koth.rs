use crate::net::{ids, packet};

use super::{
    ctf, shoot::broadcast_enum, GameServer, CHAT_TEAM_ALL, GAME_PLAYING, MAX_PLAYER,
    PLAYER_STATUS_ALIVE, PLAYER_TEAM_BLUE, PLAYER_TEAM_RED,
};

pub const GAME_TYPE_KOTH: i8 = 4;
pub const KOTH_HOLD_TIME: f32 = 15.0;

#[derive(Debug, Clone)]
pub struct KothState {
    /// left, right, bottom, top; original TKOTH expands this by half a tile.
    pub bounds: [f32; 4],
    pub controller: i8,
    pub blue_progress: f32,
    pub red_progress: f32,
    pub player_zone_time: [f32; MAX_PLAYER],
}

impl KothState {
    pub fn new(blue_markers: &[[f32; 3]]) -> Self {
        let bounds = if blue_markers.is_empty() {
            [0.0; 4]
        } else {
            blue_markers.iter().fold(
                [
                    f32::INFINITY,
                    f32::NEG_INFINITY,
                    f32::INFINITY,
                    f32::NEG_INFINITY,
                ],
                |[left, right, bottom, top], point| {
                    [
                        left.min(point[0]),
                        right.max(point[0]),
                        bottom.min(point[1]),
                        top.max(point[1]),
                    ]
                },
            )
        };
        Self {
            bounds,
            controller: -1,
            blue_progress: 0.0,
            red_progress: 0.0,
            player_zone_time: [0.0; MAX_PLAYER],
        }
    }

    pub fn supported(&self) -> bool {
        self.bounds[0].is_finite()
            && self.bounds[1] > self.bounds[0]
            && self.bounds[3] > self.bounds[2]
    }

    pub fn reset_round(&mut self) {
        self.controller = -1;
        self.blue_progress = 0.0;
        self.red_progress = 0.0;
        self.player_zone_time.fill(0.0);
    }

    fn contains(&self, position: [f32; 3]) -> bool {
        position[0] > self.bounds[0] - 0.5
            && position[0] < self.bounds[1] + 0.5
            && position[1] > self.bounds[2] - 0.5
            && position[1] < self.bounds[3] + 0.5
    }
}

pub fn state_packet(game: &GameServer) -> Vec<u8> {
    packet::koth_state(
        game.koth.controller,
        game.koth.blue_progress,
        game.koth.red_progress,
        KOTH_HOLD_TIME,
        game.koth.bounds,
    )
}

pub fn tick_koth(game: &mut GameServer, dt: f32) {
    if game.config.game_type != GAME_TYPE_KOTH
        || game.round_state != GAME_PLAYING
        || !game.koth.supported()
    {
        return;
    }
    let mut blue = false;
    let mut red = false;
    for slot in 0..MAX_PLAYER {
        let Some(player) = game.players[slot].as_ref() else {
            continue;
        };
        if player.status != PLAYER_STATUS_ALIVE || !game.koth.contains(player.cf.position) {
            game.koth.player_zone_time[slot] = 0.0;
            continue;
        }
        game.koth.player_zone_time[slot] += dt;
        blue |= player.team_id == PLAYER_TEAM_BLUE;
        red |= player.team_id == PLAYER_TEAM_RED;
    }

    update_control(&mut game.koth, blue, red, dt);

    let scoring_team = if game.koth.blue_progress >= KOTH_HOLD_TIME {
        Some(PLAYER_TEAM_BLUE)
    } else if game.koth.red_progress >= KOTH_HOLD_TIME {
        Some(PLAYER_TEAM_RED)
    } else {
        None
    };
    if let Some(team) = scoring_team {
        score(game, team);
    } else if game.frame_id.rem_euclid(5) == 0 {
        game.broadcast(state_packet(game));
    }
}

fn update_control(state: &mut KothState, blue: bool, red: bool, dt: f32) {
    state.controller = match (blue, red) {
        (true, true) => 2,
        (true, false) => {
            state.blue_progress += dt;
            state.red_progress = 0.0;
            0
        }
        (false, true) => {
            state.red_progress += dt;
            state.blue_progress = 0.0;
            1
        }
        (false, false) => {
            state.blue_progress = 0.0;
            state.red_progress = 0.0;
            -1
        }
    };
}

fn score(game: &mut GameServer, team: i8) {
    let scorer = (0..MAX_PLAYER)
        .filter(|&slot| {
            game.players[slot]
                .as_ref()
                .is_some_and(|p| p.team_id == team)
        })
        .max_by(|&a, &b| game.koth.player_zone_time[a].total_cmp(&game.koth.player_zone_time[b]));
    if team == PLAYER_TEAM_BLUE {
        game.blue_win += 1;
        game.blue_score = game.blue_win;
    } else {
        game.red_win += 1;
        game.red_score = game.red_win;
    }
    let message = if team == PLAYER_TEAM_BLUE {
        "\x01BLUE TEAM SCORES!\x09 New round starting..."
    } else {
        "\x04RED TEAM SCORES!\x09 New round starting..."
    };
    let mut body = vec![0u8; 132];
    body[1] = CHAT_TEAM_ALL as u8;
    let bytes = message.as_bytes();
    body[2..2 + bytes.len()].copy_from_slice(bytes);
    game.broadcast(packet::encode_frame(ids::NET_SVCL_MSG, &body));
    if let Some(slot) = scorer {
        if let Some(player) = game.players[slot].as_mut() {
            player.score += 1;
        }
        broadcast_enum(game, slot);
    }
    game.koth.reset_round();
    game.broadcast(packet::server_info(
        &game.config.map_name,
        game.config.game_type,
        game.blue_score,
        game.red_score,
        game.blue_win,
        game.red_win,
    ));
    game.broadcast(state_packet(game));
    ctf::respawn_all_players(game);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hill_uses_original_blue_marker_bounds_and_padding() {
        let hill = KothState::new(&[[2.0, 4.0, 0.0], [5.0, 8.0, 0.0]]);
        assert_eq!(hill.bounds, [2.0, 5.0, 4.0, 8.0]);
        assert!(hill.contains([1.6, 4.0, 0.25]));
        assert!(!hill.contains([1.4, 4.0, 0.25]));
    }

    #[test]
    fn contested_hill_pauses_both_counters_and_control_resets_the_opponent() {
        let mut hill = KothState::new(&[[2.0, 4.0, 0.0], [5.0, 8.0, 0.0]]);
        hill.blue_progress = 4.0;
        hill.red_progress = 3.0;
        update_control(&mut hill, true, true, 1.0);
        assert_eq!(
            (hill.controller, hill.blue_progress, hill.red_progress),
            (2, 4.0, 3.0)
        );

        update_control(&mut hill, false, true, 1.0);
        assert_eq!(
            (hill.controller, hill.blue_progress, hill.red_progress),
            (1, 0.0, 4.0)
        );
    }
}
