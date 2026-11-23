//! Photoshop-style panel dock for the side column: stacked panels with collapsible headers,
//! each scrolling on its own, with draggable dividers that trade height between neighbours.

use egui::{Pos2, Rect};
use serde::{Deserialize, Serialize};

pub const HEADER_H: f32 = 32.0;
/// Smallest body an open panel can be dragged to.
pub const MIN_BODY: f32 = 60.0;
/// Half-height of a divider's grab zone.
pub const GRIP: f32 = 4.0;

/// Panel heights (body only, excluding the header) and collapsed flags, persisted in prefs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dock {
    pub heights: Vec<f32>,
    pub collapsed: Vec<bool>,
}

impl Default for Dock {
    fn default() -> Self {
        // Clips, Camera RAW, Viewer, Export, Metadata
        Self { heights: vec![150.0, 400.0, 250.0, 360.0, 220.0], collapsed: vec![false, false, false, false, true] }
    }
}

pub struct Slot {
    pub header: Rect,
    /// `None` when collapsed.
    pub body: Option<Rect>,
}

impl Dock {
    /// Lay `n` panels out in `rect`. Open panels keep their proportions and together fill
    /// the space left by the headers, so resizing the window scales them evenly.
    pub fn layout(&mut self, rect: Rect, n: usize) -> Vec<Slot> {
        let d = Dock::default();
        while self.heights.len() < n {
            self.heights.push(d.heights.get(self.heights.len()).copied().unwrap_or(200.0));
        }
        while self.collapsed.len() < n {
            self.collapsed.push(false);
        }
        let open: Vec<usize> = (0..n).filter(|&i| !self.collapsed[i]).collect();
        let avail = (rect.height() - n as f32 * HEADER_H).max(0.0);
        let sum: f32 = open.iter().map(|&i| self.heights[i].max(MIN_BODY)).sum();
        if sum > 0.0 {
            let scale = avail / sum;
            for &i in &open {
                self.heights[i] = (self.heights[i].max(MIN_BODY) * scale).floor();
            }
            // Rounding goes to the last open panel so the column ends flush.
            let used: f32 = open.iter().map(|&i| self.heights[i]).sum();
            if let Some(&last) = open.last() {
                self.heights[last] += avail - used;
            }
        }
        let mut y = rect.top();
        (0..n)
            .map(|i| {
                let header = Rect::from_min_max(Pos2::new(rect.left(), y), Pos2::new(rect.right(), y + HEADER_H));
                y += HEADER_H;
                let body = (!self.collapsed[i]).then(|| {
                    let b = Rect::from_min_max(Pos2::new(rect.left(), y), Pos2::new(rect.right(), y + self.heights[i]));
                    y += self.heights[i];
                    b
                });
                Slot { header, body }
            })
            .collect()
    }

    /// The open panel below `i` that a divider under `i` trades height with.
    pub fn partner(&self, i: usize) -> Option<usize> {
        if self.collapsed.get(i).copied().unwrap_or(true) {
            return None;
        }
        (i + 1..self.collapsed.len()).find(|&j| !self.collapsed[j])
    }

    /// Drag the divider under panel `i` by `dy` (positive grows `i`).
    pub fn drag(&mut self, i: usize, dy: f32) {
        let Some(j) = self.partner(i) else { return };
        let dy = dy.clamp(-(self.heights[i] - MIN_BODY).max(0.0), (self.heights[j] - MIN_BODY).max(0.0));
        self.heights[i] += dy;
        self.heights[j] -= dy;
    }

    pub fn toggle(&mut self, i: usize) {
        if let Some(c) = self.collapsed.get_mut(i) {
            *c = !*c;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(h: f32) -> Rect {
        Rect::from_min_max(Pos2::ZERO, Pos2::new(300.0, h))
    }

    #[test]
    fn fills_and_trades_height() {
        let mut d = Dock::default();
        let s = d.layout(rect(1000.0), 5);
        // Open bodies + all headers fill the column exactly (the collapsed Metadata header last).
        let end = s.last().map(|s| s.body.unwrap_or(s.header).bottom()).unwrap();
        assert!((end - 1000.0).abs() < 0.5, "{end}");
        assert!(s[4].body.is_none(), "metadata starts collapsed");
        let (a, b) = (d.heights[0], d.heights[1]);
        d.drag(0, 40.0);
        assert_eq!((d.heights[0], d.heights[1]), (a + 40.0, b - 40.0));
        // Can't shrink below the minimum.
        d.drag(0, -10_000.0);
        assert_eq!(d.heights[0], MIN_BODY);
        // Collapsing gives the space to the others; the last open panel has no partner.
        d.toggle(1);
        d.layout(rect(1000.0), 5);
        assert_eq!(d.partner(0), Some(2));
        assert_eq!(d.partner(3), None);
    }
}
