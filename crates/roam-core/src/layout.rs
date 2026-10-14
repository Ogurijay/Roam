//! 网格布局（与 Deskflow 相同的思路，细到每块显示器）。
//!
//! 每块显示器占网格里的一个格子。不同电脑的显示器之间，只有上下左右相邻的格子可以穿越，
//! 位置沿两块显示器相接的边按比例对应；同一台电脑的几块显示器之间，按它系统设置里的排列移动。
//! 显示器的位置用各自电脑的本地坐标（逻辑点）描述。

use roam_proto::Rect;
use serde::{Deserialize, Serialize};

pub type DeviceId = String;

/// 网格大小（列 × 行），与页面保持一致。
pub const GRID_COLS: i32 = 6;
pub const GRID_ROWS: i32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cell {
    pub col: i32,
    pub row: i32,
}

impl Cell {
    pub fn neighbor(self, side: Side) -> Cell {
        let (dc, dr) = match side {
            Side::Left => (-1, 0),
            Side::Right => (1, 0),
            Side::Top => (0, -1),
            Side::Bottom => (0, 1),
        };
        Cell {
            col: self.col + dc,
            row: self.row + dr,
        }
    }

    pub fn in_grid(self) -> bool {
        (0..GRID_COLS).contains(&self.col) && (0..GRID_ROWS).contains(&self.row)
    }
}

/// 网格里一个格子的键：设备 ID + 显示器序号。
pub fn grid_key(id: &str, display: usize) -> String {
    format!("{id}#{display}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

pub trait RectExt {
    fn contains(&self, x: f64, y: f64) -> bool;
    fn clamp(&self, x: f64, y: f64) -> (f64, f64);
    fn distance(&self, x: f64, y: f64) -> f64;
    /// 点越出了矩形的哪一边（越出两边时取越出更多的那边）；在矩形内返回 None。
    fn exit_side(&self, x: f64, y: f64) -> Option<Side>;
}

impl RectExt for Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        (
            x.clamp(self.x, self.x + self.w - 1.0),
            y.clamp(self.y, self.y + self.h - 1.0),
        )
    }

    fn distance(&self, x: f64, y: f64) -> f64 {
        let (cx, cy) = self.clamp(x, y);
        ((cx - x).powi(2) + (cy - y).powi(2)).sqrt()
    }

    fn exit_side(&self, x: f64, y: f64) -> Option<Side> {
        let candidates = [
            (Side::Left, self.x - x),
            (Side::Right, x - (self.x + self.w - 1.0)),
            (Side::Top, self.y - y),
            (Side::Bottom, y - (self.y + self.h - 1.0)),
        ];
        candidates
            .into_iter()
            .filter(|(_, over)| *over > 0.0)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(side, _)| side)
    }
}

pub fn bbox(rects: &[Rect]) -> Rect {
    if rects.is_empty() {
        return Rect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        };
    }
    let x0 = rects.iter().map(|r| r.x).fold(f64::INFINITY, f64::min);
    let y0 = rects.iter().map(|r| r.y).fold(f64::INFINITY, f64::min);
    let x1 = rects
        .iter()
        .map(|r| r.x + r.w)
        .fold(f64::NEG_INFINITY, f64::max);
    let y1 = rects
        .iter()
        .map(|r| r.y + r.h)
        .fold(f64::NEG_INFINITY, f64::max);
    Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
}

/// 从源显示器 `src` 的 `side` 边离开（位置 `x, y`），进入目标显示器 `dst` 时的位置：沿边按比例对应。
pub fn entry_point(src: Rect, dst: Rect, side: Side, x: f64, y: f64) -> (f64, f64) {
    let ty = ((y - src.y) / src.h).clamp(0.0, 1.0);
    let tx = ((x - src.x) / src.w).clamp(0.0, 1.0);
    match side {
        Side::Right => (dst.x, dst.y + ty * (dst.h - 1.0)),
        Side::Left => (dst.x + dst.w - 1.0, dst.y + ty * (dst.h - 1.0)),
        Side::Bottom => (dst.x + tx * (dst.w - 1.0), dst.y),
        Side::Top => (dst.x + tx * (dst.w - 1.0), dst.y + dst.h - 1.0),
    }
}

/// 一台设备：显示器（本地坐标）+ 每块显示器所在的格子（与 `displays` 一一对应）。
#[derive(Debug, Clone)]
pub struct DeviceGeom {
    pub id: DeviceId,
    pub displays: Vec<Rect>,
    pub cells: Vec<Cell>,
}

impl DeviceGeom {
    pub fn bbox(&self) -> Rect {
        bbox(&self.displays)
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        self.displays.iter().any(|d| d.contains(x, y))
    }

    /// 离点最近（包含该点时就是它）的显示器序号。
    pub fn nearest_index(&self, x: f64, y: f64) -> usize {
        self.displays
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.distance(x, y).total_cmp(&b.1.distance(x, y)))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    pub fn nearest_display(&self, x: f64, y: f64) -> Rect {
        self.displays
            .get(self.nearest_index(x, y))
            .copied()
            .unwrap_or_else(|| self.bbox())
    }

    /// 钳制到最近的显示器上。
    pub fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        self.nearest_display(x, y).clamp(x, y)
    }

    /// 主显示器（本地坐标原点所在的那块，没有就取第一块）的中心。
    pub fn center(&self) -> (f64, f64) {
        let d = self
            .displays
            .iter()
            .find(|d| d.contains(0.0, 0.0))
            .or(self.displays.first())
            .copied()
            .unwrap_or_else(|| self.bbox());
        (d.x + d.w / 2.0, d.y + d.h / 2.0)
    }
}

/// 参与切屏的设备集合（主机 + 在线的被控设备）。
#[derive(Debug, Clone, Default)]
pub struct World {
    pub devices: Vec<DeviceGeom>,
}

impl World {
    pub fn find(&self, id: &str) -> Option<&DeviceGeom> {
        self.devices.iter().find(|d| d.id == id)
    }

    /// 某台设备第 `display` 块显示器在 `side` 那一边相邻的、属于**另一台设备**的显示器。
    pub fn neighbor(&self, id: &str, display: usize, side: Side) -> Option<(&DeviceGeom, usize)> {
        let cell = self.find(id)?.cells.get(display)?.neighbor(side);
        self.devices
            .iter()
            .filter(|d| d.id != id)
            .find_map(|d| d.cells.iter().position(|c| *c == cell).map(|i| (d, i)))
    }
}

/// 主机的主显示器默认放在网格中间偏左上。
pub const HOST_CELL: Cell = Cell { col: 2, row: 1 };

/// 给一台设备的显示器分配默认格子：主显示器放在 `anchor` 附近最近的空格，
/// 其他显示器按系统里的相对方向放在旁边，被占用就找最近的空格。
pub fn default_cells(displays: &[Rect], taken: &[Cell], anchor: Cell) -> Vec<Cell> {
    let mut taken = taken.to_vec();
    let mut cells = vec![Cell { col: 0, row: 0 }; displays.len().max(1)];
    if displays.is_empty() {
        cells[0] = nearest_free(&taken, anchor);
        return cells;
    }
    let primary = displays
        .iter()
        .position(|d| d.contains(0.0, 0.0))
        .unwrap_or(0);
    let first = nearest_free(&taken, anchor);
    cells[primary] = first;
    taken.push(first);

    let p = displays[primary];
    let (pcx, pcy) = (p.x + p.w / 2.0, p.y + p.h / 2.0);
    // 离主显示器近的先放
    let mut others: Vec<usize> = (0..displays.len()).filter(|&i| i != primary).collect();
    others.sort_by(|&a, &b| {
        let d = |i: usize| {
            let r = displays[i];
            (r.x + r.w / 2.0 - pcx).abs() + (r.y + r.h / 2.0 - pcy).abs()
        };
        d(a).total_cmp(&d(b))
    });
    for i in others {
        let r = displays[i];
        let (dx, dy) = (r.x + r.w / 2.0 - pcx, r.y + r.h / 2.0 - pcy);
        let wanted = if dx.abs() >= dy.abs() {
            Cell {
                col: first.col + dx.signum() as i32,
                row: first.row,
            }
        } else {
            Cell {
                col: first.col,
                row: first.row + dy.signum() as i32,
            }
        };
        let cell = nearest_free(&taken, wanted);
        cells[i] = cell;
        taken.push(cell);
    }
    cells
}

/// 离 `target` 最近的空格子；同距离时同一行、右边优先。
pub fn nearest_free(taken: &[Cell], target: Cell) -> Cell {
    let mut best: Option<(i32, Cell)> = None;
    for row in 0..GRID_ROWS {
        for col in 0..GRID_COLS {
            let cell = Cell { col, row };
            if taken.contains(&cell) {
                continue;
            }
            let dist = (col - target.col).abs() * 2 + (row - target.row).abs() * 3
                - i32::from(col > target.col);
            if best.is_none_or(|(d, _)| dist < d) {
                best = Some((dist, cell));
            }
        }
    }
    best.map(|(_, c)| c).unwrap_or(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    fn dev(id: &str, displays: Vec<Rect>, cells: &[(i32, i32)]) -> DeviceGeom {
        DeviceGeom {
            id: id.into(),
            displays,
            cells: cells.iter().map(|&(col, row)| Cell { col, row }).collect(),
        }
    }

    #[test]
    fn exit_side_and_proportional_entry() {
        let mac = rect(0.0, 0.0, 1512.0, 982.0);
        let win = rect(0.0, 0.0, 1920.0, 1080.0);
        assert_eq!(mac.exit_side(1515.0, 400.0), Some(Side::Right));
        assert_eq!(mac.exit_side(-3.0, 400.0), Some(Side::Left));
        assert_eq!(mac.exit_side(500.0, 400.0), None);
        // 从 Mac 右边 50% 高度离开 → Windows 左边 50% 高度进入
        let (x, y) = entry_point(mac, win, Side::Right, 1511.0, 491.0);
        assert_eq!(x, 0.0);
        assert!((y - 539.5).abs() < 1.0, "y = {y}");
        let (x, _) = entry_point(win, mac, Side::Left, 0.0, 540.0);
        assert_eq!(x, 1511.0);
    }

    #[test]
    fn multi_display_device_with_negative_origin() {
        // 副屏在主屏左侧（本地坐标为负）
        let mac = dev(
            "mac",
            vec![
                rect(0.0, 0.0, 1512.0, 982.0),
                rect(-1920.0, 0.0, 1920.0, 1080.0),
            ],
            &[(2, 1), (1, 1)],
        );
        assert!(mac.contains(-100.0, 1000.0));
        // 主屏下方是外框里的空洞：不在任何显示器上
        assert!(!mac.contains(100.0, 1000.0));
        assert_eq!(mac.clamp(100.0, 1000.0), (100.0, 981.0));
        assert_eq!(mac.nearest_index(-5.0, 10.0), 1);
        assert_eq!(mac.center(), (756.0, 491.0));
    }

    #[test]
    fn neighbors_are_per_display_and_skip_same_device() {
        let world = World {
            devices: vec![
                // Mac：主屏 (2,1)，副屏 (1,1)
                dev(
                    "mac",
                    vec![
                        rect(0.0, 0.0, 100.0, 100.0),
                        rect(-100.0, 0.0, 100.0, 100.0),
                    ],
                    &[(2, 1), (1, 1)],
                ),
                // Windows 两块屏：(3,1) 和 (4,1)
                dev(
                    "win",
                    vec![rect(0.0, 0.0, 100.0, 100.0), rect(100.0, 0.0, 100.0, 100.0)],
                    &[(3, 1), (4, 1)],
                ),
                dev("imac", vec![rect(0.0, 0.0, 100.0, 100.0)], &[(1, 0)]),
            ],
        };
        // Mac 主屏右边是 Windows 的第 0 块
        let (d, i) = world.neighbor("mac", 0, Side::Right).unwrap();
        assert_eq!((d.id.as_str(), i), ("win", 0));
        // Mac 主屏左边是自己的副屏：不算跨设备
        assert!(world.neighbor("mac", 0, Side::Left).is_none());
        // Mac 副屏上方是 iMac
        let (d, _) = world.neighbor("mac", 1, Side::Top).unwrap();
        assert_eq!(d.id, "imac");
        // Windows 第 1 块右边没有
        assert!(world.neighbor("win", 1, Side::Right).is_none());
    }

    #[test]
    fn default_cells_follow_os_arrangement() {
        // 主屏在中间，副屏在左边、第三块在右边
        let displays = [
            rect(0.0, 0.0, 1512.0, 982.0),
            rect(-1920.0, 0.0, 1920.0, 1080.0),
            rect(1512.0, 0.0, 2560.0, 1440.0),
        ];
        let cells = default_cells(&displays, &[], HOST_CELL);
        assert_eq!(cells[0], HOST_CELL);
        assert_eq!(cells[1], Cell { col: 1, row: 1 });
        assert_eq!(cells[2], Cell { col: 3, row: 1 });

        // 新设备默认放在主机最右边那块屏幕的右侧；被占了就找最近的空格
        let taken: Vec<Cell> = cells.clone();
        let anchor = Cell { col: 4, row: 1 };
        let peer = default_cells(&[rect(0.0, 0.0, 1920.0, 1080.0)], &taken, anchor);
        assert_eq!(peer, vec![anchor]);
        let mut taken2 = taken.clone();
        taken2.push(anchor);
        assert_eq!(nearest_free(&taken2, anchor), Cell { col: 5, row: 1 });
    }
}
