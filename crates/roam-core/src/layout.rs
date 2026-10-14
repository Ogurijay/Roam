//! 网格布局（与 Deskflow 相同的思路）。
//!
//! 每台设备占网格里的一个格子，只有上下左右相邻的格子之间可以穿越。
//! 设备自己的显示器用本地坐标（逻辑点）描述；跨设备时按边的比例对应，
//! 所以不需要关心两台设备屏幕的实际尺寸和摆放偏移。

use roam_proto::Rect;
use serde::{Deserialize, Serialize};

pub type DeviceId = String;

/// 网格大小（列 × 行），与页面保持一致。
pub const GRID_COLS: i32 = 5;
pub const GRID_ROWS: i32 = 3;

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

/// 一台设备：显示器（本地坐标）+ 所在格子。
#[derive(Debug, Clone)]
pub struct DeviceGeom {
    pub id: DeviceId,
    pub displays: Vec<Rect>,
    pub cell: Cell,
}

impl DeviceGeom {
    pub fn bbox(&self) -> Rect {
        bbox(&self.displays)
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        self.displays.iter().any(|d| d.contains(x, y))
    }

    pub fn nearest_display(&self, x: f64, y: f64) -> Rect {
        self.displays
            .iter()
            .copied()
            .min_by(|a, b| a.distance(x, y).total_cmp(&b.distance(x, y)))
            .unwrap_or_else(|| self.bbox())
    }

    /// 钳制到最近的显示器上。
    pub fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        self.nearest_display(x, y).clamp(x, y)
    }

    pub fn center(&self) -> (f64, f64) {
        let d = self
            .displays
            .first()
            .copied()
            .unwrap_or_else(|| self.bbox());
        (d.x + d.w / 2.0, d.y + d.h / 2.0)
    }

    /// 点越出了设备外框的哪一边（越出两边时取越出更多的那边）；还在外框内返回 None。
    pub fn exit_side(&self, x: f64, y: f64) -> Option<Side> {
        let b = self.bbox();
        let candidates = [
            (Side::Left, b.x - x),
            (Side::Right, x - (b.x + b.w - 1.0)),
            (Side::Top, b.y - y),
            (Side::Bottom, y - (b.y + b.h - 1.0)),
        ];
        candidates
            .into_iter()
            .filter(|(_, over)| *over > 0.0)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(side, _)| side)
    }

    /// 从 `side` 这一边离开（位置 `x, y`），进入 `target` 时的位置：沿边按比例对应。
    pub fn entry_into(&self, target: &DeviceGeom, side: Side, x: f64, y: f64) -> (f64, f64) {
        let src = self.bbox();
        let dst = target.bbox();
        let ty = ((y - src.y) / src.h).clamp(0.0, 1.0);
        let tx = ((x - src.x) / src.w).clamp(0.0, 1.0);
        let point = match side {
            Side::Right => (dst.x, dst.y + ty * (dst.h - 1.0)),
            Side::Left => (dst.x + dst.w - 1.0, dst.y + ty * (dst.h - 1.0)),
            Side::Bottom => (dst.x + tx * (dst.w - 1.0), dst.y),
            Side::Top => (dst.x + tx * (dst.w - 1.0), dst.y + dst.h - 1.0),
        };
        target.clamp(point.0, point.1)
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

    /// 某台设备在某一边的相邻设备。
    pub fn neighbor(&self, id: &str, side: Side) -> Option<&DeviceGeom> {
        let cell = self.find(id)?.cell.neighbor(side);
        self.devices.iter().find(|d| d.cell == cell)
    }
}

/// 新设备的默认格子：离 `anchor`（通常是主机）最近的空格子，优先放在同一行、右边。
pub fn default_cell(taken: &[Cell], anchor: Cell) -> Cell {
    let mut best: Option<(i32, Cell)> = None;
    for row in 0..GRID_ROWS {
        for col in 0..GRID_COLS {
            let cell = Cell { col, row };
            if taken.contains(&cell) {
                continue;
            }
            let dist = (col - anchor.col).abs() * 2 + (row - anchor.row).abs() * 3
                - i32::from(col > anchor.col);
            if best.is_none_or(|(d, _)| dist < d) {
                best = Some((dist, cell));
            }
        }
    }
    best.map(|(_, c)| c).unwrap_or(anchor)
}

/// 主机默认放在网格中央。
pub const HOST_CELL: Cell = Cell {
    col: GRID_COLS / 2,
    row: GRID_ROWS / 2,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    fn dev(id: &str, displays: Vec<Rect>, col: i32, row: i32) -> DeviceGeom {
        DeviceGeom {
            id: id.into(),
            displays,
            cell: Cell { col, row },
        }
    }

    #[test]
    fn exit_side_and_proportional_entry() {
        let mac = dev("mac", vec![rect(0.0, 0.0, 1512.0, 982.0)], 2, 1);
        let win = dev("win", vec![rect(0.0, 0.0, 1920.0, 1080.0)], 3, 1);
        assert_eq!(mac.exit_side(1515.0, 400.0), Some(Side::Right));
        assert_eq!(mac.exit_side(-3.0, 400.0), Some(Side::Left));
        assert_eq!(mac.exit_side(500.0, 400.0), None);
        // 从 Mac 右边 50% 高度离开 → Windows 左边 50% 高度进入
        let (x, y) = mac.entry_into(&win, Side::Right, 1511.0, 491.0);
        assert_eq!(x, 0.0);
        assert!((y - 539.5).abs() < 1.0, "y = {y}");
        // 反过来从 Windows 左边离开 → Mac 右边缘
        let (x, _) = win.entry_into(&mac, Side::Left, 0.0, 540.0);
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
            2,
            1,
        );
        assert!(mac.contains(-100.0, 1000.0));
        // 主屏下方是外框里的空洞：不算越界，但也不在任何显示器上
        assert!(!mac.contains(100.0, 1000.0));
        assert_eq!(mac.exit_side(100.0, 1000.0), None);
        assert_eq!(mac.clamp(100.0, 1000.0), (100.0, 981.0));
        assert_eq!(mac.exit_side(-1925.0, 500.0), Some(Side::Left));
    }

    #[test]
    fn neighbors_in_grid() {
        let world = World {
            devices: vec![
                dev("mac", vec![rect(0.0, 0.0, 100.0, 100.0)], 2, 1),
                dev("win", vec![rect(0.0, 0.0, 100.0, 100.0)], 3, 1),
                dev("imac", vec![rect(0.0, 0.0, 100.0, 100.0)], 2, 0),
            ],
        };
        assert_eq!(world.neighbor("mac", Side::Right).unwrap().id, "win");
        assert_eq!(world.neighbor("mac", Side::Top).unwrap().id, "imac");
        assert!(world.neighbor("mac", Side::Left).is_none());
        assert!(world.neighbor("win", Side::Top).is_none());
    }

    #[test]
    fn default_cells_fill_around_host() {
        let mut taken = vec![HOST_CELL];
        let first = default_cell(&taken, HOST_CELL);
        assert_eq!(first, Cell { col: 3, row: 1 });
        taken.push(first);
        assert_eq!(default_cell(&taken, HOST_CELL), Cell { col: 1, row: 1 });
    }
}
