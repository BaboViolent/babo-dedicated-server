//! Bot display names (from CCredit.cpp + extras) and unique skins.

pub const BOT_SKIN_COUNT: usize = 23;

const BOT_NAMES: &[&str] = &[
    // BaboViolent 2 credits (src/Menu2/CCredit.cpp)
    "RndLabs",
    "Marc Durocher",
    "nuvem",
    "cnik",
    "PM",
    "Lordlou",
    "Adam Pilkington",
    "Daivuk",
    "Pacifist",
    "Doimuk",
    "Sunita",
    "Lamb of God",
    // Community / extras
    "BaboFan42",
    "Grenade Gary",
    "Flag Runner",
    "Rocket Ric",
    "Molotov Mike",
    "Shield Sheila",
    "Sniper Sam",
    "Knife Knight",
    "Blue Babo",
    "Red Menace",
    "Pod Pouncer",
    "Wall Hugger",
    "Spawn Camper",
    "Chaos Ball",
    "BitHeads",
    "Zeven",
    "HeadGames",
    "RndLabs Jr",
    "Community Hero",
    "Last Babo Standing",
];

pub fn bot_name(index: usize) -> String {
    format!("{} [bot]", BOT_NAMES[index % BOT_NAMES.len()])
}

pub fn bot_skin(index: usize) -> [u8; 7] {
    let n = (index % BOT_SKIN_COUNT) + 1;
    let s = format!("skin{n:02}");
    let mut out = [0u8; 7];
    for (i, b) in s.bytes().enumerate().take(6) {
        out[i] = b;
    }
    out
}

/// Distinct decal tint per bot (DM); team modes tint at render time on the client.
pub fn bot_decals(index: usize) -> ([u8; 3], [u8; 3], [u8; 3]) {
    let i = index as u8;
    let red = [200u8.wrapping_add(i.wrapping_mul(17)), 80, 120];
    let green = [80, 200u8.wrapping_add(i.wrapping_mul(23)), 100];
    let blue = [100, 80, 200u8.wrapping_add(i.wrapping_mul(29))];
    (red, green, blue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_roster_name_is_explicitly_marked_as_a_bot() {
        for index in 0..BOT_NAMES.len() {
            assert!(bot_name(index).ends_with(" [bot]"));
        }
    }
}
