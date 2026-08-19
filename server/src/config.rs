use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_addr: String,
    pub content_root: PathBuf,
    pub map_name: String,
    pub game_name: String,
    pub password: String,
    pub game_type: i8,
    pub max_players: usize,
    pub bot_count: usize,
    pub win_limit: i16,
    /// Seconds; zero means unlimited.
    pub game_time_limit: f32,
}

impl ServerConfig {
    pub fn from_env() -> Self {
        let content_root = resolve_content_root();
        let maps_dir = content_root.join("maps");
        let map_name = std::env::var("BV2_MAP")
            .ok()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| pick_default_map(&maps_dir));

        Self {
            bind_addr: std::env::var("BV2_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            content_root,
            map_name,
            game_name: std::env::var("BV2_GAME_NAME").unwrap_or_else(|_| "BV2 Web Server".into()),
            password: std::env::var("BV2_PASSWORD").unwrap_or_default(),
            game_type: std::env::var("BV2_GAME_TYPE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            max_players: std::env::var("BV2_MAX_PLAYERS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(16)
                .min(32),
            bot_count: std::env::var("BV2_BOTS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(2)
                .min(32),
            win_limit: std::env::var("BV2_WIN_LIMIT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(7)
                .max(0),
            game_time_limit: std::env::var("BV2_GAME_TIME_LIMIT")
                .ok()
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(15.0 * 60.0)
                .max(0.0),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let maps_dir = self.maps_dir();
        if !maps_dir.is_dir() {
            return Err(format!(
                "maps directory not found: {} (set BV2_CONTENT_ROOT to Content/main)",
                maps_dir.display()
            ));
        }
        let map_path = self.map_path(&self.map_name);
        if !map_path.is_file() {
            let available = list_maps(&maps_dir);
            return Err(format!(
                "map not found: {} ({} maps under {})",
                map_path.display(),
                available.len(),
                maps_dir.display()
            ));
        }
        Ok(())
    }

    pub fn maps_dir(&self) -> PathBuf {
        self.content_root.join("maps")
    }

    pub fn map_path(&self, name: &str) -> PathBuf {
        let file = if name.ends_with(".bvm") {
            name.to_string()
        } else {
            format!("{name}.bvm")
        };
        self.maps_dir().join(file)
    }
}

/// Resolve Content/main — run from repo `server/` or set BV2_CONTENT_ROOT.
fn resolve_content_root() -> PathBuf {
    if let Ok(p) = std::env::var("BV2_CONTENT_ROOT") {
        return PathBuf::from(p);
    }
    for candidate in ["../Content/main", "Content/main", "../../Content/main"] {
        let p = PathBuf::from(candidate);
        if p.join("maps").is_dir() {
            return std::fs::canonicalize(&p).unwrap_or(p);
        }
    }
    PathBuf::from("../Content/main")
}

fn pick_default_map(maps_dir: &Path) -> String {
    let maps = list_maps(maps_dir);
    if let Some(m) = maps.iter().find(|n| *n == "CTF-BurialMound") {
        return m.clone();
    }
    maps.into_iter()
        .next()
        .unwrap_or_else(|| "CTF-BurialMound".into())
}

#[derive(Debug, Error)]
pub enum MapError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad bvm: {0}")]
    Format(String),
}

/// Minimal .bvm read for spawn selection (Map.cpp layout).
#[derive(Debug, Default, Clone)]
pub struct MapSpawns {
    pub name: String,
    pub size_x: i16,
    pub size_y: i16,
    pub dm: Vec<[f32; 3]>,
    pub blue: Vec<[f32; 3]>,
    pub red: Vec<[f32; 3]>,
    pub flag_pod: [[f32; 3]; 2],
}

pub fn load_map_spawns(path: &Path) -> Result<MapSpawns, MapError> {
    let data = std::fs::read(path)?;
    let mut off = 0usize;

    let read_u32 = |d: &[u8], o: &mut usize| -> Result<u32, MapError> {
        if *o + 4 > d.len() {
            return Err(MapError::Format("truncated u32".into()));
        }
        let v = u32::from_le_bytes(d[*o..*o + 4].try_into().unwrap());
        *o += 4;
        Ok(v)
    };
    let read_i16 = |d: &[u8], o: &mut usize| -> Result<i16, MapError> {
        if *o + 2 > d.len() {
            return Err(MapError::Format("truncated i16".into()));
        }
        let v = i16::from_le_bytes(d[*o..*o + 2].try_into().unwrap());
        *o += 2;
        Ok(v)
    };
    let read_f32 = |d: &[u8], o: &mut usize| -> Result<f32, MapError> {
        if *o + 4 > d.len() {
            return Err(MapError::Format("truncated f32".into()));
        }
        let v = f32::from_le_bytes(d[*o..*o + 4].try_into().unwrap());
        *o += 4;
        Ok(v)
    };
    let skip = |d: &[u8], o: &mut usize, n: usize| -> Result<(), MapError> {
        if *o + n > d.len() {
            return Err(MapError::Format("skip past eof".into()));
        }
        *o += n;
        Ok(())
    };
    let read_spawns = |d: &[u8], o: &mut usize| -> Result<Vec<[f32; 3]>, MapError> {
        let n = read_i16(d, o)? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push([read_f32(d, o)?, read_f32(d, o)?, read_f32(d, o)?]);
        }
        Ok(out)
    };

    let read_vec3 = |d: &[u8], o: &mut usize| -> Result<[f32; 3], MapError> {
        Ok([read_f32(d, o)?, read_f32(d, o)?, read_f32(d, o)?])
    };

    let version = read_u32(&data, &mut off)?;
    let mut spawns = MapSpawns {
        name: path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("map")
            .into(),
        ..Default::default()
    };

    if version == 20202 {
        skip(&data, &mut off, 25)?;
    }
    if version >= 20201 {
        skip(&data, &mut off, 4)?;
    }

    spawns.size_x = read_i16(&data, &mut off)?;
    spawns.size_y = read_i16(&data, &mut off)?;
    let cells = (spawns.size_x as usize) * (spawns.size_y as usize);
    skip(&data, &mut off, cells * 2)?;

    if version == 10011 || version == 20201 {
        spawns.flag_pod[0] = read_vec3(&data, &mut off)?;
        spawns.flag_pod[1] = read_vec3(&data, &mut off)?;
        skip(&data, &mut off, 24)?; // objectives
        spawns.dm = read_spawns(&data, &mut off)?;
        spawns.blue = read_spawns(&data, &mut off)?;
        spawns.red = read_spawns(&data, &mut off)?;
    } else if version == 20202 {
        spawns.dm = read_spawns(&data, &mut off)?;
        const GAME_TYPE_COUNT: usize = 4;
        for _ in 0..GAME_TYPE_COUNT {
            let game_type_id = read_i16(&data, &mut off)?;
            match game_type_id {
                2 => {
                    spawns.flag_pod[0] = read_vec3(&data, &mut off)?;
                    spawns.flag_pod[1] = read_vec3(&data, &mut off)?;
                }
                3 => {
                    skip(&data, &mut off, 24)?;
                    spawns.blue = read_spawns(&data, &mut off)?;
                    spawns.red = read_spawns(&data, &mut off)?;
                }
                _ => {}
            }
        }
    }

    Ok(spawns)
}

pub fn list_maps(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for ent in rd.flatten() {
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) == Some("bvm") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    names
}
