//! Minimal `.bvm` tile grid + raycast for server-side hitscan (mirrors jsclient `raycast.js`).

use std::collections::{HashMap, VecDeque};

use crate::config::{load_map_spawns, MapError, MapSpawns};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct MapGrid {
    pub spawns: MapSpawns,
    /// Per cell: bit7 = passable, bits0-6 = height.
    cells: Vec<u8>,
}

impl MapGrid {
    pub fn empty(spawns: MapSpawns) -> Self {
        Self {
            spawns,
            cells: vec![],
        }
    }

    #[cfg(test)]
    pub(crate) fn fixture(size: i16, solids: &[(i32, i32)]) -> Self {
        let spawns = MapSpawns {
            size_x: size,
            size_y: size,
            ..Default::default()
        };
        let mut cells = vec![0x80; size as usize * size as usize];
        for &(x, y) in solids {
            cells[y as usize * size as usize + x as usize] = 1;
        }
        Self { spawns, cells }
    }

    pub fn load(path: &Path) -> Result<Self, MapError> {
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
        let skip = |d: &[u8], o: &mut usize, n: usize| -> Result<(), MapError> {
            if *o + n > d.len() {
                return Err(MapError::Format("skip past eof".into()));
            }
            *o += n;
            Ok(())
        };

        let version = read_u32(&data, &mut off)?;
        let mut spawns = load_map_spawns(path)?;

        if version == 20202 {
            skip(&data, &mut off, 25)?;
        }
        if version >= 20201 {
            skip(&data, &mut off, 4)?;
        }

        let size_x = read_i16(&data, &mut off)?;
        let size_y = read_i16(&data, &mut off)?;
        spawns.size_x = size_x;
        spawns.size_y = size_y;

        let cells_len = (size_x as usize) * (size_y as usize);
        if off + cells_len * 2 > data.len() {
            return Err(MapError::Format("truncated cells".into()));
        }
        let mut cells = Vec::with_capacity(cells_len);
        for _ in 0..cells_len {
            cells.push(data[off]);
            off += 2; // data + dirt
        }

        Ok(Self { spawns, cells })
    }

    pub fn size_x(&self) -> i16 {
        self.spawns.size_x
    }

    pub fn size_y(&self) -> i16 {
        self.spawns.size_y
    }

    fn cell(&self, x: i32, y: i32) -> Option<u8> {
        if x < 0 || y < 0 {
            return None;
        }
        let x = x as usize;
        let y = y as usize;
        if x >= self.spawns.size_x as usize || y >= self.spawns.size_y as usize {
            return None;
        }
        Some(self.cells[y * self.spawns.size_x as usize + x])
    }

    pub fn height_at(&self, x: i32, y: i32) -> f32 {
        self.cell(x, y).map(|c| (c & 0x7f) as f32).unwrap_or(0.0)
    }

    pub fn is_passable(&self, x: i32, y: i32) -> bool {
        self.cell(x, y).map(|c| c & 0x80 != 0).unwrap_or(false)
    }

    pub fn contains_xy(&self, x: f32, y: f32) -> bool {
        x >= 0.0 && y >= 0.0 && x < self.size_x() as f32 && y < self.size_y() as f32
    }

    fn is_solid(&self, x: i32, y: i32) -> bool {
        self.cell(x, y).is_some_and(|c| c & 0x80 == 0)
    }

    /// Returns `(impact_point, normal)`; point equals ray end if no wall hit.
    pub fn ray_test(&self, from: [f32; 3], to: [f32; 3]) -> ([f32; 3], [f32; 3]) {
        let p1 = from;
        let mut p2 = to;
        let mut normal = [0.0f32, 0.0, 1.0];

        let mut i = p1[0].floor() as i32;
        let mut j = p1[1].floor() as i32;

        if i < 0 || j < 0 || i >= self.size_x() as i32 || j >= self.size_y() as i32 {
            return (p2, normal);
        }
        if !self.is_passable(i, j) && p1[2] < self.height_at(i, j) {
            return (p1, normal);
        }

        let dir_x = (p2[0] - p1[0]).abs() > (p2[1] - p1[1]).abs();
        let max_steps = (self.size_x() + self.size_y()) as i32 * 2 + 8;

        for _ in 0..max_steps {
            if i < 0 || j < 0 || i >= self.size_x() as i32 || j >= self.size_y() as i32 {
                break;
            }
            if dir_x {
                if (p2[0] - p1[0]).abs() < 1e-6 {
                    break;
                }
                if (p2[0] > p1[0] && i > p2[0].floor() as i32)
                    || (p2[0] < p1[0] && i < p2[0].floor() as i32)
                {
                    break;
                }
            } else if (p2[1] - p1[1]).abs() < 1e-6
                || (p2[1] > p1[1] && j > p2[1].floor() as i32)
                || (p2[1] < p1[1] && j < p2[1].floor() as i32)
            {
                break;
            }

            if dir_x {
                if self.ray_tile(i, j, p1, &mut p2, &mut normal)
                    || self.ray_tile(i, j - 1, p1, &mut p2, &mut normal)
                    || self.ray_tile(i, j + 1, p1, &mut p2, &mut normal)
                {
                    return (p2, normal);
                }
                if p2[0] > p1[0] {
                    i += 1;
                } else {
                    i -= 1;
                }
                // Keep the direction sign. Using an absolute denominator made
                // leftward diagonal rays advance into the opposite grid row,
                // allowing hitscan fire to skip walls.
                let percent = (i as f32 - p1[0]) / (p2[0] - p1[0]);
                j = (p1[1] + (p2[1] - p1[1]) * percent).floor() as i32;
            } else {
                if self.ray_tile(i, j, p1, &mut p2, &mut normal)
                    || self.ray_tile(i - 1, j, p1, &mut p2, &mut normal)
                    || self.ray_tile(i + 1, j, p1, &mut p2, &mut normal)
                {
                    return (p2, normal);
                }
                if p2[1] > p1[1] {
                    j += 1;
                } else {
                    j -= 1;
                }
                // The signed denominator is equally required for upward rays.
                let percent = (j as f32 - p1[1]) / (p2[1] - p1[1]);
                i = (p1[0] + (p2[0] - p1[0]) * percent).floor() as i32;
            }
        }
        (p2, normal)
    }

    fn ray_tile(
        &self,
        x: i32,
        y: i32,
        p1: [f32; 3],
        p2: &mut [f32; 3],
        normal: &mut [f32; 3],
    ) -> bool {
        let x1 = x as f32;
        let x2 = x as f32 + 1.0;
        let y1 = y as f32;
        let y2 = y as f32 + 1.0;
        let height = self.height_at(x, y);
        let passable = self.is_passable(x, y);

        let lerp = |t: f32| {
            [
                p1[0] + (p2[0] - p1[0]) * t,
                p1[1] + (p2[1] - p1[1]) * t,
                p1[2] + (p2[2] - p1[2]) * t,
            ]
        };

        if passable {
            if p1[2] > 0.0 && p2[2] <= 0.0 {
                let t = p1[2] / (p1[2] - p2[2]).abs().max(1e-6);
                let p = lerp(t);
                if p[0] >= x1 && p[0] <= x2 && p[1] >= y1 && p[1] <= y2 {
                    *p2 = p;
                    *normal = [0.0, 0.0, 1.0];
                    return true;
                }
            }
            return false;
        }

        if p1[2] > height && p2[2] <= height {
            let t = (p1[2] - height) / ((p2[2] - height) - (p1[2] - height)).abs().max(1e-6);
            let p = lerp(t);
            if p[0] >= x1 && p[0] <= x2 && p[1] >= y1 && p[1] <= y2 {
                *p2 = p;
                *normal = [0.0, 0.0, 1.0];
                return true;
            }
        }

        let sides: [([f32; 3], f32); 4] = [
            (
                [-1.0, 0.0, 0.0],
                if p1[0] <= x1 && p2[0] > x1 {
                    (x1 - p1[0]).abs() / (p2[0] - p1[0]).abs().max(1e-6)
                } else {
                    -1.0
                },
            ),
            (
                [1.0, 0.0, 0.0],
                if p1[0] >= x2 && p2[0] < x2 {
                    (p1[0] - x2).abs() / (p2[0] - p1[0]).abs().max(1e-6)
                } else {
                    -1.0
                },
            ),
            (
                [0.0, -1.0, 0.0],
                if p1[1] <= y1 && p2[1] > y1 {
                    (y1 - p1[1]).abs() / (p2[1] - p1[1]).abs().max(1e-6)
                } else {
                    -1.0
                },
            ),
            (
                [0.0, 1.0, 0.0],
                if p1[1] >= y2 && p2[1] < y2 {
                    (p1[1] - y2).abs() / (p2[1] - p1[1]).abs().max(1e-6)
                } else {
                    -1.0
                },
            ),
        ];

        for (n, t) in sides {
            if t < 0.0 {
                continue;
            }
            let p = lerp(t);
            // A hit on an X face must lie within the tile's Y span, and vice
            // versa. Checking the face axis itself makes neighboring tile
            // faces effectively infinite and catches rockets far from edges.
            let axis = if n[0].abs() > 0.0 { 1 } else { 0 };
            let lo = if axis == 1 { y1 } else { x1 };
            let hi = if axis == 1 { y2 } else { x2 };
            if p[axis] <= hi && p[axis] >= lo && p[2] < height {
                *p2 = p;
                *normal = n;
                return true;
            }
        }
        false
    }

    pub fn walkable_at(&self, x: f32, y: f32) -> bool {
        let tx = x.floor() as i32;
        let ty = y.floor() as i32;
        self.is_passable(tx, ty)
    }

    /// Whether a circular actor can stand at this point without being pushed
    /// by the authoritative collision clip.
    pub fn has_clearance(&self, point: [f32; 2], radius: f32) -> bool {
        if !self.contains_xy(point[0], point[1]) || !self.walkable_at(point[0], point[1]) {
            return false;
        }
        let mut clipped = [point[0], point[1], 0.25];
        clip_entity_xy(self, &mut clipped, radius);
        (clipped[0] - point[0]).abs() < 0.001 && (clipped[1] - point[1]).abs() < 0.001
    }

    fn segment_has_clearance(&self, from: [f32; 2], to: [f32; 2], radius: f32) -> bool {
        let dx = to[0] - from[0];
        let dy = to[1] - from[1];
        let len = dx.hypot(dy);
        if len < 0.001 {
            return self.has_clearance(from, radius);
        }
        let perpendicular = [-dy / len * radius, dx / len * radius];
        [-1.0f32, 0.0, 1.0].into_iter().all(|side| {
            let p1 = [
                from[0] + perpendicular[0] * side,
                from[1] + perpendicular[1] * side,
                0.25,
            ];
            let p2 = [
                to[0] + perpendicular[0] * side,
                to[1] + perpendicular[1] * side,
                0.25,
            ];
            let (hit, _) = self.ray_test(p1, p2);
            segment_hit_t(p1, p2, hit) >= 0.98
        })
    }

    /// Quake's AAS uses a fuzzy reachability-area lookup when a goal is not
    /// itself navigable. Project generated tactical goals onto the nearest
    /// passable tile center for the same reason.
    pub fn resolve_navigation_goal(&self, goal: [f32; 2]) -> [f32; 2] {
        if self.has_clearance(goal, 0.30) {
            return goal;
        }
        let gx = goal[0].floor() as i32;
        let gy = goal[1].floor() as i32;
        let max_radius = i32::from(self.size_x().max(self.size_y()));
        for radius in 0..=max_radius {
            let mut best: Option<([f32; 2], f32)> = None;
            for y in gy - radius..=gy + radius {
                for x in gx - radius..=gx + radius {
                    if radius > 0
                        && x != gx - radius
                        && x != gx + radius
                        && y != gy - radius
                        && y != gy + radius
                    {
                        continue;
                    }
                    let point = [x as f32 + 0.5, y as f32 + 0.5];
                    if !self.has_clearance(point, 0.30) {
                        continue;
                    }
                    let distance = (point[0] - goal[0]).powi(2) + (point[1] - goal[1]).powi(2);
                    if best.as_ref().is_none_or(|(_, current)| distance < *current) {
                        best = Some((point, distance));
                    }
                }
            }
            if let Some((point, _)) = best {
                return point;
            }
        }
        goal
    }

    /// Steer target using tile BFS when a wall blocks the direct route.
    pub fn navigate_toward(&self, from: [f32; 2], goal: [f32; 2]) -> [f32; 2] {
        let goal = self.resolve_navigation_goal(goal);
        if self.segment_has_clearance(from, goal, 0.30) {
            return goal;
        }

        let start = (from[0].floor() as i32, from[1].floor() as i32);
        let end = (goal[0].floor() as i32, goal[1].floor() as i32);
        if let Some(next) = self.bfs_next_tile(start, end, self.cells.len()) {
            return [next.0 as f32 + 0.5, next.1 as f32 + 0.5];
        }
        // If the waypoint graph has no route, stay close to the current
        // position and peel off toward the requested goal to escape local
        // dead-ends (quake-style recovery behavior).
        self.unstuck_heading(from, goal)
    }

    fn bfs_next_tile(
        &self,
        start: (i32, i32),
        end: (i32, i32),
        max_nodes: usize,
    ) -> Option<(i32, i32)> {
        if !self.is_passable(start.0, start.1) {
            return None;
        }
        if start == end {
            return Some(end);
        }

        let dirs = [(0, 1), (0, -1), (1, 0), (-1, 0)];
        let mut parent: HashMap<(i32, i32), (i32, i32)> = HashMap::new();
        let mut q = VecDeque::new();
        q.push_back(start);
        parent.insert(start, start);
        let mut visited = 1usize;

        while let Some(cur) = q.pop_front() {
            if cur == end {
                let mut node = end;
                while parent[&node] != start {
                    node = parent[&node];
                }
                return Some(node);
            }
            if visited >= max_nodes {
                break;
            }
            for (dx, dy) in dirs {
                let nx = cur.0 + dx;
                let ny = cur.1 + dy;
                if !self.is_passable(nx, ny) {
                    continue;
                }
                let n = (nx, ny);
                if parent.contains_key(&n) {
                    continue;
                }
                parent.insert(n, cur);
                q.push_back(n);
                visited += 1;
            }
        }
        None
    }

    /// Pick a passable direction when stuck against a wall (8-way feelers).
    pub fn unstuck_heading(&self, from: [f32; 2], preferred: [f32; 2]) -> [f32; 2] {
        let dx = preferred[0] - from[0];
        let dy = preferred[1] - from[1];
        let plen = (dx * dx + dy * dy).sqrt().max(1e-4);
        let ux = dx / plen;
        let uy = dy / plen;

        let mut best = (preferred, -1.0f32);
        for i in 0..8 {
            let a = (i as f32) * std::f32::consts::PI / 4.0;
            let cx = a.cos();
            let cy = a.sin();
            let test = [from[0] + cx * 0.6, from[1] + cy * 0.6];
            if !self.walkable_at(test[0], test[1]) {
                continue;
            }
            // A passable endpoint is not sufficient: a diagonal feeler can
            // still cut through the solid corner between the two points.
            if !self.segment_has_clearance(from, test, 0.30) {
                continue;
            }
            let score = cx * ux + cy * uy;
            if score > best.1 {
                best = ([from[0] + cx * 3.0, from[1] + cy * 3.0], score);
            }
        }
        best.0
    }
}

/// Segment vs sphere (player radius 0.25). Returns hit parameter t in [0,1] along p1→p2.
pub fn segment_sphere_hit_t(p1: [f32; 3], p2: [f32; 3], center: [f32; 3], r: f32) -> Option<f32> {
    let dx = p2[0] - p1[0];
    let dy = p2[1] - p1[1];
    let dz = p2[2] - p1[2];
    let fx = p1[0] - center[0];
    let fy = p1[1] - center[1];
    let fz = p1[2] - center[2];
    let a = dx * dx + dy * dy + dz * dz;
    if a < 1e-8 {
        let d = (fx * fx + fy * fy + fz * fz).sqrt();
        return if d <= r { Some(0.0) } else { None };
    }
    let b = 2.0 * (fx * dx + fy * dy + fz * dz);
    let c = fx * fx + fy * fy + fz * fz - r * r;
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return None;
    }
    let t = (-b - disc.sqrt()) / (2.0 * a);
    if (0.0..=1.0).contains(&t) {
        Some(t)
    } else {
        None
    }
}

pub fn segment_hit_t(p1: [f32; 3], p2: [f32; 3], point: [f32; 3]) -> f32 {
    let dx = p2[0] - p1[0];
    let dy = p2[1] - p1[1];
    let dz = p2[2] - p1[2];
    let len = (dx * dx + dy * dy + dz * dz).sqrt().max(1e-6);
    let px = point[0] - p1[0];
    let py = point[1] - p1[1];
    let pz = point[2] - p1[2];
    ((px * dx + py * dy + pz * dz) / len).clamp(0.0, len) / len
}

/// Push a player center out of solid tiles (jsclient collisionClip).
pub fn clip_entity_xy(grid: &MapGrid, pos: &mut [f32; 3], radius: f32) {
    const EPS: f32 = 0.05;
    let x = pos[0].floor() as i32;
    let y = pos[1].floor() as i32;

    if !grid.contains_xy(pos[0], pos[1]) {
        return;
    }

    if pos[0] + radius + EPS > x as f32 + 1.0 && grid.is_solid(x + 1, y) {
        pos[0] = x as f32 + 1.0 - radius - EPS;
    }
    if pos[0] - radius - EPS < x as f32 && grid.is_solid(x - 1, y) {
        pos[0] = x as f32 + radius + EPS;
    }
    if pos[1] + radius + EPS > y as f32 + 1.0 && grid.is_solid(x, y + 1) {
        pos[1] = y as f32 + 1.0 - radius - EPS;
    }
    if pos[1] - radius - EPS < y as f32 && grid.is_solid(x, y - 1) {
        pos[1] = y as f32 + radius + EPS;
    }

    if grid.is_passable(x, y) {
        return;
    }
    let candidates = [
        (grid.is_passable(x - 1, y), pos[0] - x as f32),
        (grid.is_passable(x + 1, y), 1.0 - (pos[0] - x as f32)),
        (grid.is_passable(x, y - 1), pos[1] - y as f32),
        (grid.is_passable(x, y + 1), 1.0 - (pos[1] - y as f32)),
    ];
    let mut best = f32::MAX;
    let mut dir = (0.0f32, 0.0f32);
    for (ok, dis) in candidates {
        if ok && dis < best {
            best = dis;
        }
    }
    if best == f32::MAX {
        return;
    }
    for (i, (ok, dis)) in candidates.iter().enumerate() {
        if *ok && (*dis - best).abs() < 1e-4 {
            match i {
                0 => dir.0 = -1.0,
                1 => dir.0 = 1.0,
                2 => dir.1 = -1.0,
                _ => dir.1 = 1.0,
            }
            break;
        }
    }
    if dir.0 < 0.0 {
        pos[0] = x as f32 - radius - EPS;
    } else if dir.0 > 0.0 {
        pos[0] = x as f32 + 1.0 + radius + EPS;
    } else if dir.1 < 0.0 {
        pos[1] = y as f32 - radius - EPS;
    } else if dir.1 > 0.0 {
        pos[1] = y as f32 + 1.0 + radius + EPS;
    }
}

/// Apply BV2's axis-sequential player collision. The unusual use of the
/// pre-move coordinate on the perpendicular axis is inherited from
/// `Map::performCollision` and is part of the original cornering feel.
pub fn perform_collision_xy(
    grid: &MapGrid,
    previous: [f32; 3],
    pos: &mut [f32; 3],
    vel: &mut [f32; 3],
    radius: f32,
) {
    const EPS: f32 = 0.05;
    const BOUNCE: f32 = 0.45;
    let x = pos[0].floor() as i32;
    let y = pos[1].floor() as i32;

    if vel[1] < 0.0 {
        for dx in [0, -1, 1] {
            if grid.is_solid(x + dx, y - 1)
                && previous[0] - radius <= (x + dx + 1) as f32
                && previous[0] + radius >= (x + dx) as f32
                && pos[1] - radius <= y as f32
                && pos[1] + radius >= (y - 1) as f32
            {
                pos[1] = y as f32 + radius + EPS;
                vel[1] = -vel[1] * BOUNCE;
            }
        }
    } else if vel[1] > 0.0 {
        for dx in [0, -1, 1] {
            if grid.is_solid(x + dx, y + 1)
                && previous[0] - radius <= (x + dx + 1) as f32
                && previous[0] + radius >= (x + dx) as f32
                && pos[1] - radius <= (y + 2) as f32
                && pos[1] + radius >= (y + 1) as f32
            {
                pos[1] = (y + 1) as f32 - radius - EPS;
                vel[1] = -vel[1] * BOUNCE;
            }
        }
    }

    if vel[0] < 0.0 {
        for dy in [0, -1, 1] {
            if grid.is_solid(x - 1, y + dy)
                && pos[0] - radius <= x as f32
                && pos[0] + radius >= (x - 1) as f32
                && previous[1] - radius <= (y + dy + 1) as f32
                && previous[1] + radius >= (y + dy) as f32
            {
                pos[0] = x as f32 + radius + EPS;
                vel[0] = -vel[0] * BOUNCE;
            }
        }
    } else if vel[0] > 0.0 {
        for dy in [0, -1, 1] {
            if grid.is_solid(x + 1, y + dy)
                && pos[0] - radius <= (x + 2) as f32
                && pos[0] + radius >= (x + 1) as f32
                && previous[1] - radius <= (y + dy + 1) as f32
                && previous[1] + radius >= (y + dy) as f32
            {
                pos[0] = (x + 1) as f32 - radius - EPS;
                vel[0] = -vel[0] * BOUNCE;
            }
        }
    }

    clip_entity_xy(grid, pos, radius);
}

#[cfg(test)]
mod movement_tests {
    use super::*;

    fn fixture(size: i16, solids: &[(i32, i32)]) -> MapGrid {
        MapGrid::fixture(size, solids)
    }

    #[test]
    fn collision_is_y_then_x_with_legacy_bounce() {
        let grid = fixture(8, &[(3, 4), (4, 3)]);
        let previous = [3.5, 3.5, 0.25];
        let mut pos = [3.8, 3.8, 0.25];
        let mut vel = [2.0, 2.0, 0.0];
        perform_collision_xy(&grid, previous, &mut pos, &mut vel, 0.25);
        assert!((pos[1] - 3.70).abs() < 0.001);
        assert!((pos[0] - 3.70).abs() < 0.001);
        assert!((vel[0] + 0.9).abs() < 0.001);
        assert!((vel[1] + 0.9).abs() < 0.001);
    }

    #[test]
    fn collision_clip_ejects_from_solid_cell_like_js() {
        let grid = fixture(8, &[(3, 3), (4, 3), (3, 2), (3, 4)]);
        let mut pos = [3.1, 3.5, 0.25];
        clip_entity_xy(&grid, &mut pos, 0.25);
        assert!((pos[0] - 2.70).abs() < 0.001);
    }

    #[test]
    fn open_map_edge_does_not_act_like_an_invisible_wall() {
        let grid = fixture(8, &[]);
        let previous = [0.1, 4.0, 0.25];
        let mut pos = [-0.1, 4.0, 0.25];
        let mut vel = [-2.0, 0.0, 0.0];
        perform_collision_xy(&grid, previous, &mut pos, &mut vel, 0.25);
        assert_eq!(pos[0], -0.1);
        assert_eq!(vel[0], -2.0);
    }

    #[test]
    fn ray_does_not_hit_a_neighboring_wall_outside_its_edge() {
        let grid = fixture(8, &[(3, 2)]);
        let from = [1.5, 3.5, 0.25];
        let to = [5.5, 3.5, 0.25];
        let (end, _) = grid.ray_test(from, to);
        assert_eq!(end, to);
    }

    #[test]
    fn diagonal_rays_hit_the_same_wall_in_both_directions() {
        let grid = fixture(8, &[(3, 3)]);
        for (from, to) in [
            ([1.5, 1.5, 0.25], [5.5, 5.5, 0.25]),
            ([5.5, 5.5, 0.25], [1.5, 1.5, 0.25]),
            ([1.5, 5.5, 0.25], [5.5, 1.5, 0.25]),
            ([5.5, 1.5, 0.25], [1.5, 5.5, 0.25]),
        ] {
            let (end, _) = grid.ray_test(from, to);
            assert_ne!(end, to, "ray {from:?} -> {to:?} skipped the wall");
            assert!(segment_hit_t(from, to, end) < 1.0);
        }
    }

    #[test]
    fn generated_goal_inside_wall_projects_to_clear_tile() {
        let grid = fixture(8, &[(4, 4)]);
        let resolved = grid.resolve_navigation_goal([4.2, 4.7]);
        assert_ne!(resolved, [4.2, 4.7]);
        assert!(grid.has_clearance(resolved, 0.30));
    }

    #[test]
    fn routing_search_is_not_truncated_at_five_hundred_twelve_tiles() {
        let solids: Vec<_> = (0..39).map(|y| (20, y)).collect();
        let grid = MapGrid::fixture(40, &solids);
        let next = grid.navigate_toward([1.5, 1.5], [38.5, 1.5]);
        assert_eq!(next, [1.5, 2.5]);
    }

    #[test]
    fn crazy_authored_spawns_have_player_clearance() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../Content/main/maps");
        let grid = MapGrid::load(&root.join("CTF-Crazy.bvm")).unwrap();
        assert!(!grid.spawns.dm.is_empty());
        for spawn in &grid.spawns.dm {
            assert!(
                grid.has_clearance([spawn[0], spawn[1]], 0.25),
                "authored spawn lacks clearance: {spawn:?}"
            );
        }
    }

    #[test]
    fn shipped_maps_keep_players_out_of_neighboring_walls() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../Content/main/maps");
        for name in ["CTF-Alert.bvm", "CHP-MiniArena.bvm"] {
            let grid = MapGrid::load(&root.join(name)).unwrap();
            let mut case = None;
            for y in 2..grid.size_y() as i32 - 2 {
                for x in 2..grid.size_x() as i32 - 2 {
                    if grid.is_passable(x, y) && !grid.is_passable(x + 1, y) {
                        case = Some((x, y));
                        break;
                    }
                }
                if case.is_some() {
                    break;
                }
            }
            let (x, y) = case.expect("representative wall boundary");
            let previous = [x as f32 + 0.5, y as f32 + 0.5, 0.25];
            let mut pos = [x as f32 + 0.9, y as f32 + 0.5, 0.25];
            let mut vel = [3.0, 0.0, 0.0];
            perform_collision_xy(&grid, previous, &mut pos, &mut vel, 0.25);
            assert!((pos[0] - (x as f32 + 0.70)).abs() < 0.001, "{name}");
            assert!(vel[0] < 0.0, "{name}");
        }
    }

    fn decode_base64(input: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut bits = 0u32;
        let mut count = 0u32;
        for byte in input
            .bytes()
            .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        {
            let value = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => panic!("invalid fixture base64"),
            } as u32;
            bits = (bits << 6) | value;
            count += 6;
            if count >= 8 {
                count -= 8;
                out.push((bits >> count) as u8);
                bits &= (1 << count) - 1;
            }
        }
        out
    }

    #[test]
    fn rust_loads_canonical_browser_editor_export() {
        let encoded = include_str!("../../../jsclient/test/fixtures/editor-rust.bvm.base64");
        let path = std::env::temp_dir().join(format!("bv2-editor-{}.bvm", std::process::id()));
        std::fs::write(&path, decode_base64(encoded)).unwrap();
        let grid = MapGrid::load(&path).unwrap();
        let _ = std::fs::remove_file(path);
        assert_eq!((grid.size_x(), grid.size_y()), (2, 2));
        assert!(grid.is_passable(0, 0));
        assert_eq!(grid.height_at(1, 0), 2.0);
        assert_eq!(grid.spawns.dm, vec![[0.5, 0.5, 0.25]]);
        assert_eq!(grid.spawns.blue, vec![[0.5, 1.5, 0.25]]);
        assert_eq!(grid.spawns.red, vec![[1.5, 0.5, 0.25]]);
        assert_eq!(grid.spawns.flag_pod, [[0.5, 0.5, 0.25], [1.5, 1.5, 0.25]]);
    }
}
