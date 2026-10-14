//! 布局几何。
//!
//! 每台设备有自己的本地坐标系（逻辑点），显示器矩形用本地坐标描述。
//! 布局给每台设备一个"外框左上角在全局坐标中的位置"，于是：
//! `全局 = 本地 - 外框原点 + 设备位置`。

use roam_proto::Rect;

use crate::config::Point;

pub type DeviceId = String;

/// 判断"相邻"时允许的缝隙（逻辑点）。布局页面会自动吸附，这里只是兜底。
pub const GAP_TOLERANCE: f64 = 24.0;

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

#[derive(Debug, Clone)]
pub struct DeviceGeom {
    pub id: DeviceId,
    /// 本地坐标下的显示器。
    pub displays: Vec<Rect>,
    /// 外框左上角的全局位置。
    pub pos: Point,
}

impl DeviceGeom {
    pub fn bbox(&self) -> Rect {
        bbox(&self.displays)
    }

    pub fn to_global(&self, x: f64, y: f64) -> (f64, f64) {
        let b = self.bbox();
        (x - b.x + self.pos.x, y - b.y + self.pos.y)
    }

    pub fn to_local(&self, gx: f64, gy: f64) -> (f64, f64) {
        let b = self.bbox();
        (gx - self.pos.x + b.x, gy - self.pos.y + b.y)
    }

    pub fn global_displays(&self) -> impl Iterator<Item = Rect> + '_ {
        let b = self.bbox();
        self.displays.iter().map(move |d| Rect {
            x: d.x - b.x + self.pos.x,
            y: d.y - b.y + self.pos.y,
            w: d.w,
            h: d.h,
        })
    }

    pub fn contains_global(&self, gx: f64, gy: f64) -> bool {
        self.global_displays().any(|d| d.contains(gx, gy))
    }

    /// 离全局点最近的显示器上的最近点。
    pub fn clamp_global(&self, gx: f64, gy: f64) -> (f64, f64) {
        self.global_displays()
            .map(|d| (d.distance(gx, gy), d.clamp(gx, gy)))
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, p)| p)
            .unwrap_or((gx, gy))
    }

    pub fn distance_global(&self, gx: f64, gy: f64) -> f64 {
        self.global_displays()
            .map(|d| d.distance(gx, gy))
            .fold(f64::INFINITY, f64::min)
    }
}

pub fn bbox(rects: &[Rect]) -> Rect {
    if rects.is_empty() {
        return Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
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

/// 参与切屏的设备集合（主机 + 在线的被控设备）。
#[derive(Debug, Clone, Default)]
pub struct World {
    pub devices: Vec<DeviceGeom>,
}

impl World {
    pub fn find(&self, id: &str) -> Option<&DeviceGeom> {
        self.devices.iter().find(|d| d.id == id)
    }

    /// 精确包含该点的设备。
    pub fn device_at(&self, gx: f64, gy: f64) -> Option<&DeviceGeom> {
        self.devices.iter().find(|d| d.contains_global(gx, gy))
    }

    /// 包含该点、或在容差内离该点最近的设备（排除 `exclude`），返回设备与钳制后的点。
    pub fn device_near(
        &self,
        gx: f64,
        gy: f64,
        exclude: &str,
    ) -> Option<(&DeviceGeom, (f64, f64))> {
        self.devices
            .iter()
            .filter(|d| d.id != exclude)
            .map(|d| (d.distance_global(gx, gy), d))
            .filter(|(dist, _)| *dist <= GAP_TOLERANCE)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, d)| (d, d.clamp_global(gx, gy)))
    }
}

/// 新设备的默认位置：放在已有设备的右侧，顶部对齐。
pub fn default_position(existing: &[DeviceGeom]) -> Point {
    let right = existing
        .iter()
        .flat_map(|d| d.global_displays().collect::<Vec<_>>())
        .map(|r| r.x + r.w)
        .fold(f64::NEG_INFINITY, f64::max);
    let top = existing
        .iter()
        .map(|d| d.pos.y)
        .fold(f64::INFINITY, f64::min);
    if right.is_finite() {
        Point { x: right, y: top }
    } else {
        Point { x: 0.0, y: 0.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn local_global_roundtrip_with_negative_origin() {
        // macOS 副屏在主屏左侧时本地坐标为负
        let d = DeviceGeom {
            id: "mac".into(),
            displays: vec![
                rect(0.0, 0.0, 1512.0, 982.0),
                rect(-1920.0, 0.0, 1920.0, 1080.0),
            ],
            pos: Point { x: 100.0, y: 50.0 },
        };
        assert_eq!(d.bbox(), rect(-1920.0, 0.0, 3432.0, 1080.0));
        assert_eq!(d.to_global(-1920.0, 0.0), (100.0, 50.0));
        assert_eq!(d.to_local(100.0, 50.0), (-1920.0, 0.0));
        assert!(d.contains_global(100.0 + 1920.0 + 10.0, 60.0));
        // 主屏下方（副屏更高，外框里有空洞）不算在内
        assert!(!d.contains_global(100.0 + 1920.0 + 10.0, 50.0 + 1000.0));
    }

    #[test]
    fn device_near_tolerates_small_gaps() {
        let world = World {
            devices: vec![
                DeviceGeom {
                    id: "a".into(),
                    displays: vec![rect(0.0, 0.0, 100.0, 100.0)],
                    pos: Point { x: 0.0, y: 0.0 },
                },
                DeviceGeom {
                    id: "b".into(),
                    displays: vec![rect(0.0, 0.0, 100.0, 100.0)],
                    pos: Point { x: 110.0, y: 0.0 },
                },
            ],
        };
        let (d, p) = world.device_near(102.0, 50.0, "a").unwrap();
        assert_eq!(d.id, "b");
        assert_eq!(p, (110.0, 50.0));
        assert!(world.device_near(102.0, 500.0, "a").is_none());
    }

    #[test]
    fn default_position_goes_right() {
        let existing = vec![DeviceGeom {
            id: "a".into(),
            displays: vec![rect(0.0, 0.0, 1512.0, 982.0)],
            pos: Point { x: 0.0, y: 0.0 },
        }];
        assert_eq!(default_position(&existing), Point { x: 1512.0, y: 0.0 });
    }
}
