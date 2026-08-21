use std::collections::HashSet;

use super::{ctf, BaboNetId, GameServer, PLAYER_TEAM_BLUE, PLAYER_TEAM_RED};
use crate::config::load_map_spawns;
use crate::map::MapGrid;
use crate::net::{ids, packet};

const VOTE_DURATION: f32 = 30.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoteCommand {
    ChangeMap(String),
    GameType(i8),
}

#[derive(Debug, Default)]
pub struct VoteState {
    pub command: Option<VoteCommand>,
    pub eligible: HashSet<BaboNetId>,
    pub voted: HashSet<BaboNetId>,
    pub yes: u8,
    pub no: u8,
    pub remaining: f32,
}

impl VoteState {
    pub fn active(&self) -> bool {
        self.command.is_some()
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

fn parse_command(text: &str) -> Option<VoteCommand> {
    let parts: Vec<_> = text.split_whitespace().collect();
    if parts.len() == 2 && parts[0].eq_ignore_ascii_case("changemap") {
        return Some(VoteCommand::ChangeMap(parts[1].to_string()));
    }
    if parts.len() == 3
        && parts[0].eq_ignore_ascii_case("set")
        && parts[1].eq_ignore_ascii_case("sv_gameType")
    {
        let game_type = parts[2].parse().ok()?;
        return matches!(game_type, 0..=3).then_some(VoteCommand::GameType(game_type));
    }
    None
}

fn valid_command(game: &GameServer, command: &VoteCommand) -> bool {
    match command {
        VoteCommand::ChangeMap(name) => {
            if !super::is_advertised_map(&game.map_list, name)
                || !super::map_allowed_for_game_type(game.config.game_type, name)
            {
                return false;
            }
            load_map_spawns(&game.config.map_path(name)).is_ok_and(|spawns| {
                super::map_compatible_for_game_type(game.config.game_type, &spawns)
            })
        }
        VoteCommand::GameType(_) => true,
    }
}

pub fn handle_vote_request(game: &mut GameServer, babonet_id: BaboNetId, payload: &[u8]) {
    if !game.voting_enabled || game.vote.active() || payload.len() < 81 {
        return;
    }
    let Some(slot) = super::wire_player_slot(&payload[80..]) else {
        return;
    };
    let Some(player) = game.players[slot].as_ref() else {
        return;
    };
    if player.babonet_id != babonet_id
        || player.is_bot
        || player.external_bot
        || !matches!(player.team_id, PLAYER_TEAM_BLUE | PLAYER_TEAM_RED)
    {
        return;
    }
    let display = packet::read_str(payload, 0, 80);
    let Some(command) = parse_command(&display) else {
        return;
    };
    if !valid_command(game, &command) {
        return;
    }
    let eligible = game
        .players
        .iter()
        .flatten()
        .filter(|player| {
            !player.is_bot
                && !player.external_bot
                && matches!(player.team_id, PLAYER_TEAM_BLUE | PLAYER_TEAM_RED)
        })
        .map(|player| player.babonet_id)
        .collect();
    game.vote = VoteState {
        command: Some(command),
        eligible,
        voted: HashSet::new(),
        yes: 0,
        no: 0,
        remaining: VOTE_DURATION,
    };
    game.broadcast(packet::encode_frame(
        ids::NET_CLSV_SVCL_VOTE_REQUEST,
        payload,
    ));
    game.broadcast(packet::encode_frame(ids::NET_SVCL_UPDATE_VOTE, &[0, 0]));
}

pub fn handle_vote(game: &mut GameServer, babonet_id: BaboNetId, payload: &[u8]) {
    if !game.voting_enabled || !game.vote.active() || payload.len() < 2 {
        return;
    }
    let Some(slot) = super::wire_player_slot(&payload[1..]) else {
        return;
    };
    let Some(player) = game.players[slot].as_ref() else {
        return;
    };
    if player.babonet_id != babonet_id
        || !game.vote.eligible.contains(&babonet_id)
        || !game.vote.voted.insert(babonet_id)
    {
        return;
    }
    if payload[0] != 0 {
        game.vote.yes = game.vote.yes.saturating_add(1);
    } else {
        game.vote.no = game.vote.no.saturating_add(1);
    }
    game.broadcast(packet::encode_frame(
        ids::NET_SVCL_UPDATE_VOTE,
        &[game.vote.yes, game.vote.no],
    ));
    finish_if_decided(game);
}

pub fn tick_vote(game: &mut GameServer, dt: f32) {
    if !game.vote.active() {
        return;
    }
    game.vote.remaining = (game.vote.remaining - dt).max(0.0);
    if game.vote.remaining == 0.0 {
        finish_vote(game);
    }
}

fn finish_if_decided(game: &mut GameServer) {
    let majority = game.vote.eligible.len() / 2;
    if game.vote.yes as usize > majority
        || game.vote.no as usize > majority
        || game.vote.voted.len() >= game.vote.eligible.len()
    {
        finish_vote(game);
    }
}

fn finish_vote(game: &mut GameServer) {
    let passed = game.vote.yes as usize > game.vote.eligible.len() / 2;
    let command = game.vote.command.clone();
    game.broadcast(packet::encode_frame(
        ids::NET_SVCL_VOTE_RESULT,
        &[passed as u8],
    ));
    game.vote.clear();
    if passed {
        if let Some(command) = command {
            execute(game, command);
        }
    }
}

fn execute(game: &mut GameServer, command: VoteCommand) {
    match command {
        VoteCommand::ChangeMap(name) => {
            let path = game.config.map_path(&name);
            let (Ok(spawns), Ok(grid)) = (load_map_spawns(&path), MapGrid::load(&path)) else {
                return;
            };
            game.config.map_name = name;
            game.map_spawns = spawns;
            game.map_grid = grid;
            game.ctf = super::CtfState::new(ctf::map_flag_pod(&game.map_spawns));
        }
        VoteCommand::GameType(game_type) => {
            game.config.game_type = game_type;
            game.rotate_to_next_map();
        }
    }
    ctf::restart_match(game);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_allowlisted_vote_commands() {
        assert_eq!(
            parse_command("changemap CTF-Alert"),
            Some(VoteCommand::ChangeMap("CTF-Alert".into()))
        );
        assert_eq!(
            parse_command("set sv_gameType 2"),
            Some(VoteCommand::GameType(2))
        );
        assert_eq!(parse_command("set sv_gameType 9"), None);
        assert_eq!(parse_command("quit"), None);
        assert_eq!(parse_command("set sv_password nope"), None);
    }
}
