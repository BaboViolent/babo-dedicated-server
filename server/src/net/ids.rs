// Message ids from src/Game/netPacket.h

pub const NET_CLSV_PONG: u16 = 1;
pub const NET_CLSV_SPAWN_REQUEST: u16 = 2;
pub const NET_CLSV_PLAYER_SHOOT: u16 = 3;
pub const NET_CLSV_GAMEVERSION_ACCEPTED: u16 = 4;
pub const NET_CLSV_PICKUP_REQUEST: u16 = 5;
pub const NET_CLSV_MAP_LIST_REQUEST: u16 = 8;

pub const NET_SVCL_NEWPLAYER: u16 = 101;
pub const NET_SVCL_SERVER_INFO: u16 = 102;
pub const NET_SVCL_PLAYER_DISCONNECT: u16 = 104;
pub const NET_SVCL_PLAYER_ENUM_STATE: u16 = 105;
pub const NET_SVCL_PING: u16 = 106;
pub const NET_SVCL_PLAYER_PING: u16 = 107;
pub const NET_SVCL_PLAYER_SPAWN: u16 = 108;
pub const NET_SVCL_PLAYER_SHOOT: u16 = 110;
pub const NET_SVCL_SYNCHRONIZE_TIMER: u16 = 117;
pub const NET_SVCL_CHANGE_FLAG_STATE: u16 = 118;
pub const NET_SVCL_DROP_FLAG: u16 = 119;
pub const NET_SVCL_FLAG_ENUM: u16 = 120;
pub const NET_SVCL_GAME_STATE: u16 = 121;
pub const NET_SVCL_MAP_CHANGE: u16 = 123;
pub const NET_SVCL_PICKUP_ITEM: u16 = 124;
pub const NET_SVCL_AUTOBALANCE: u16 = 128;
pub const NET_SVCL_GAMEVERSION: u16 = 116;

pub const NET_CLSV_SVCL_PLAYER_INFO: u16 = 201;
pub const NET_CLSV_SVCL_CHAT: u16 = 202;
pub const NET_CLSV_SVCL_TEAM_REQUEST: u16 = 203;
pub const NET_CLSV_SVCL_PLAYER_COORD_FRAME: u16 = 204;
pub const NET_SVCL_PLAYER_HIT: u16 = 114;
pub const NET_SVCL_EXPLOSION: u16 = 113;
pub const NET_SVCL_DELETE_PROJECTILE: u16 = 111;
#[allow(dead_code)]
pub const NET_SVCL_PROJECTILE_COORD_FRAME: u16 = 112;
#[allow(dead_code)]
pub const NET_SVCL_MSG: u16 = 132;

pub const NET_CLSV_SVCL_PLAYER_PROJECTILE: u16 = 206;
pub const NET_CLSV_SVCL_PLAYER_SHOOT_MELEE: u16 = 207;
pub const NET_CLSV_MAP_REQUEST: u16 = 209;
pub const NET_CLSV_REPORT_EXPLOSION: u16 = 220;
pub const NET_CLSV_REPORT_BURN: u16 = 221;
pub const NET_SVCL_MAP_CHUNK: u16 = 210;
pub const NET_SVCL_MAP_LIST: u16 = 211;

/// Open-source build server version (Server.h).
pub const GAME_VERSION_SV: u32 = 21_000;

pub const WIRE_POS: f32 = 100.0;
pub const WIRE_VEL: f32 = 10.0;
