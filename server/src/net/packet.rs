use thiserror::Error;

use super::ids;

#[derive(Debug, Clone)]
pub struct NetFrame {
    pub type_id: u16,
    pub payload: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum PacketError {
    #[error("incomplete frame: need {need} bytes, have {have}")]
    Incomplete { need: usize, have: usize },
}

/// BaboNet TCP header: LE u16 payload size, LE u16 type id (cPacket.h).
pub fn decode_frame(buf: &[u8]) -> Result<Option<NetFrame>, PacketError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let payload_size = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    let type_id = u16::from_le_bytes([buf[2], buf[3]]);
    let total = 4 + payload_size;
    if buf.len() < total {
        return Err(PacketError::Incomplete {
            need: total,
            have: buf.len(),
        });
    }
    Ok(Some(NetFrame {
        type_id,
        payload: buf[4..total].to_vec(),
    }))
}

pub fn encode_frame(type_id: u16, payload: &[u8]) -> Vec<u8> {
    let size = payload.len().min(u16::MAX as usize) as u16;
    let mut out = Vec::with_capacity(4 + size as usize);
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&type_id.to_le_bytes());
    out.extend_from_slice(&payload[..size as usize]);
    out
}

// --- Typed builders (layout matches netPacket.h / gcc x86_64) ---

pub fn newplayer(player_id: i8, babonet_id: i32) -> Vec<u8> {
    let mut p = vec![0u8; 8];
    p[0] = player_id as u8;
    p[4..8].copy_from_slice(&babonet_id.to_le_bytes());
    encode_frame(ids::NET_SVCL_NEWPLAYER, &p)
}

pub fn gameversion() -> Vec<u8> {
    // The native contract only consumes the first four bytes. Web clients may
    // consume the optional NUL-terminated release label that follows them.
    let release = option_env!("BV2_RELEASE_VERSION").unwrap_or("development");
    let mut payload = ids::GAME_VERSION_SV.to_le_bytes().to_vec();
    payload.extend_from_slice(release.as_bytes());
    payload.push(0);
    encode_frame(ids::NET_SVCL_GAMEVERSION, &payload)
}

pub fn server_info(
    map_name: &str,
    game_type: i8,
    blue_score: i16,
    red_score: i16,
    blue_win: i16,
    red_win: i16,
) -> Vec<u8> {
    let mut p = vec![0u8; 29];
    p[4..20].copy_from_slice(&fixed_str16(map_name));
    p[20] = game_type as u8;
    p[21..23].copy_from_slice(&blue_score.to_le_bytes());
    p[23..25].copy_from_slice(&red_score.to_le_bytes());
    p[25..27].copy_from_slice(&blue_win.to_le_bytes());
    p[27..29].copy_from_slice(&red_win.to_le_bytes());
    encode_frame(ids::NET_SVCL_SERVER_INFO, &p)
}

pub fn round_state(new_state: i8, re_init: bool) -> Vec<u8> {
    let mut p = [0u8; 2];
    p[0] = new_state as u8;
    p[1] = re_init as u8;
    encode_frame(ids::NET_SVCL_GAME_STATE, &p)
}

pub fn map_change(map_name: &str, game_type: i8) -> Vec<u8> {
    let mut p = [0u8; 17];
    p[..16].copy_from_slice(&fixed_str16(map_name));
    p[16] = game_type as u8;
    encode_frame(ids::NET_SVCL_MAP_CHANGE, &p)
}

pub fn change_flag_state(
    flag_id: i8,
    new_state: i8,
    player_id: i8,
    action: u8,
    revision: u32,
) -> Vec<u8> {
    // The original three fields remain in place for legacy clients. Modern
    // clients consume the appended action and monotonic revision instead of
    // inferring transitions or accepting stale flag state/position pairs.
    let mut p = [0u8; 8];
    p[0] = flag_id as u8;
    p[1] = new_state as u8;
    p[2] = player_id as u8;
    p[3] = action;
    p[4..8].copy_from_slice(&revision.to_le_bytes());
    encode_frame(ids::NET_SVCL_CHANGE_FLAG_STATE, &p)
}

pub fn drop_flag(flag_id: i8, position: [f32; 3], revision: u32) -> Vec<u8> {
    let mut p = vec![0u8; 17];
    p[0] = flag_id as u8;
    p[1..5].copy_from_slice(&position[0].to_le_bytes());
    p[5..9].copy_from_slice(&position[1].to_le_bytes());
    p[9..13].copy_from_slice(&position[2].to_le_bytes());
    p[13..17].copy_from_slice(&revision.to_le_bytes());
    encode_frame(ids::NET_SVCL_DROP_FLAG, &p)
}

pub fn flame_stick_to_player(projectile_id: i32, player_id: i8) -> Vec<u8> {
    let mut p = [0u8; 5];
    p[..4].copy_from_slice(&projectile_id.to_le_bytes());
    p[4] = player_id as u8;
    encode_frame(ids::NET_SVCL_FLAME_STICK_TO_PLAYER, &p)
}

pub fn player_disconnect(player_id: i8) -> Vec<u8> {
    encode_frame(ids::NET_SVCL_PLAYER_DISCONNECT, &[player_id as u8])
}

pub fn ping(player_id: i8) -> Vec<u8> {
    encode_frame(ids::NET_SVCL_PING, &[player_id as u8])
}

pub fn player_ping(player_id: i8, ping_ms: i16) -> Vec<u8> {
    let mut p = [0u8; 3];
    p[0] = player_id as u8;
    p[1..3].copy_from_slice(&ping_ms.to_le_bytes());
    encode_frame(ids::NET_SVCL_PLAYER_PING, &p)
}

pub fn synchronize_timer(frame_id: i32, game_time_left: f32, round_time_left: f32) -> Vec<u8> {
    let mut p = vec![0u8; 12];
    p[0..4].copy_from_slice(&frame_id.to_le_bytes());
    p[4..8].copy_from_slice(&game_time_left.to_le_bytes());
    p[8..12].copy_from_slice(&round_time_left.to_le_bytes());
    encode_frame(ids::NET_SVCL_SYNCHRONIZE_TIMER, &p)
}

pub fn koth_state(controller: i8, blue: f32, red: f32, goal: f32, bounds: [f32; 4]) -> Vec<u8> {
    let mut p = vec![0u8; 29];
    p[0] = controller as u8;
    p[1..5].copy_from_slice(&blue.to_le_bytes());
    p[5..9].copy_from_slice(&red.to_le_bytes());
    p[9..13].copy_from_slice(&goal.to_le_bytes());
    for (index, value) in bounds.into_iter().enumerate() {
        let start = 13 + index * 4;
        p[start..start + 4].copy_from_slice(&value.to_le_bytes());
    }
    encode_frame(ids::NET_SVCL_KOTH_STATE, &p)
}

pub fn map_list(name: &str) -> Vec<u8> {
    encode_frame(ids::NET_SVCL_MAP_LIST, &fixed_str16(name))
}

pub fn map_chunk(data: &[u8]) -> Vec<u8> {
    let n = data.len().min(250);
    let mut p = vec![0u8; 2 + n];
    p[0..2].copy_from_slice(&(n as u16).to_le_bytes());
    p[2..2 + n].copy_from_slice(&data[..n]);
    encode_frame(ids::NET_SVCL_MAP_CHUNK, &p)
}

pub fn fixed_str16(s: &str) -> [u8; 16] {
    let mut buf = [0u8; 16];
    let bytes = s.as_bytes();
    let n = bytes.len().min(15);
    buf[..n].copy_from_slice(&bytes[..n]);
    buf
}

pub fn read_i16(payload: &[u8], off: usize) -> Option<i16> {
    payload
        .get(off..off + 2)
        .map(|s| i16::from_le_bytes([s[0], s[1]]))
}

pub fn read_i32(payload: &[u8], off: usize) -> Option<i32> {
    payload
        .get(off..off + 4)
        .map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

pub fn read_str(payload: &[u8], off: usize, len: usize) -> String {
    payload
        .get(off..off + len)
        .map(|s| {
            let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
            String::from_utf8_lossy(&s[..end]).into_owned()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gameversion_keeps_native_prefix_and_appends_release_label() {
        let frame = decode_frame(&gameversion()).unwrap().unwrap();
        assert_eq!(frame.type_id, ids::NET_SVCL_GAMEVERSION);
        assert_eq!(
            u32::from_le_bytes(frame.payload[..4].try_into().unwrap()),
            ids::GAME_VERSION_SV
        );
        assert_eq!(frame.payload.last(), Some(&0));
        assert!(!read_str(&frame.payload, 4, frame.payload.len() - 4).is_empty());
    }
}
