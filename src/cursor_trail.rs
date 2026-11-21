use eframe::egui::{self, Color32, Id, Mesh, Painter, Pos2, Rect, Vec2};
use serde::Deserialize;

use crate::animation::{Direction, Style};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    #[default]
    None,
    Editor,
    Terminal,
    Both,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Appearance {
    Ribbon,
    #[default]
    Glow,
    Solid,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub target: Target,
    delay_ms: u16,
    decay_seconds: [f32; 2],
    start_threshold_cells: [f32; 2],
    color: String,
    opacity: f32,
    width_scale: f32,
    appearance: Appearance,
    direction: Direction,
    style: Style,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target: Target::None,
            delay_ms: 65,
            decay_seconds: [0.10, 0.40],
            start_threshold_cells: [2.0, 2.0],
            color: "auto".into(),
            opacity: 0.62,
            width_scale: 1.0,
            appearance: Appearance::Glow,
            direction: Direction::Out,
            style: Style::Cubic,
        }
    }
}

impl Settings {
    pub fn enabled_for(&self, target: Target) -> bool {
        self.target == target || self.target == Target::Both
    }

    pub fn validate(&self) -> Result<(), String> {
        let [fast, slow] = self.decay_seconds;
        if !(0.01..=5.0).contains(&fast) || !(fast..=5.0).contains(&slow) {
            return Err("cursor_trail.decay_seconds must contain two ordered times between 0.01 and 5 seconds".into());
        }
        if self
            .start_threshold_cells
            .iter()
            .any(|value| !(0.0..=256.0).contains(value))
        {
            return Err(
                "cursor_trail.start_threshold_cells values must be between 0 and 256".into(),
            );
        }
        if !(0.0..=1.0).contains(&self.opacity) {
            return Err("cursor_trail.opacity must be between 0 and 1".into());
        }
        if !(0.1..=4.0).contains(&self.width_scale) {
            return Err("cursor_trail.width_scale must be between 0.1 and 4".into());
        }
        if self.color != "auto" && Color32::from_hex(&self.color).is_err() {
            return Err("cursor_trail.color must be auto or a hex color such as #77BFFF".into());
        }
        Ok(())
    }
}

pub struct Cursor {
    pub surface: Id,
    pub rect: Rect,
    pub origin: Vec2,
    pub cell: Vec2,
    pub color: Color32,
}

struct Motion {
    from: Pos2,
    started_at: f64,
    duration: f32,
}

impl Motion {
    fn sample(&self, now: f64, target: Pos2, settings: &Settings) -> Option<(Pos2, f32)> {
        let phase = ((now - self.started_at) / f64::from(self.duration)) as f32;
        if phase >= 1.0 {
            return None;
        }
        let phase = phase.max(0.0);
        let easing = settings
            .style
            .progress(settings.direction, phase)
            .clamp(0.0, 1.15);
        Some((
            self.from + (target - self.from) * easing,
            (1.0 - phase).sqrt(),
        ))
    }
}

struct Track {
    surface: Id,
    cursor: Pos2,
    stayed_since: f64,
    motion: Option<Motion>,
}

#[derive(Default)]
pub struct Trail {
    track: Option<Track>,
}

impl Trail {
    pub fn draw(&mut self, painter: &Painter, settings: &Settings, cursor: Option<Cursor>) {
        let Some(cursor) = cursor.filter(|cursor| {
            painter
                .clip_rect()
                .intersects(cursor.rect.translate(cursor.origin))
        }) else {
            self.track = None;
            return;
        };
        let now = painter.ctx().input(|input| input.time);
        let center = cursor.rect.center();
        let track = self.track.get_or_insert(Track {
            surface: cursor.surface,
            cursor: center,
            stayed_since: now,
            motion: None,
        });
        if cursor.surface != track.surface {
            *track = Track {
                surface: cursor.surface,
                cursor: center,
                stayed_since: now,
                motion: None,
            };
        }
        if center != track.cursor {
            let distance = (center - track.cursor).abs() / cursor.cell.max(Vec2::splat(1.0));
            let [x, y] = settings.start_threshold_cells;
            let meets_threshold =
                (distance.x > 0.0 && distance.x >= x) || (distance.y > 0.0 && distance.y >= y);
            let ready = now - track.stayed_since >= f64::from(settings.delay_ms) / 1000.0;
            if meets_threshold && ready {
                let from = track
                    .motion
                    .as_ref()
                    .and_then(|motion| motion.sample(now, track.cursor, settings))
                    .map_or(track.cursor, |(position, _)| position);
                let [fast, slow] = settings.decay_seconds;
                let duration = egui::lerp(fast..=slow, (distance.length() / 12.0).min(1.0));
                track.motion = Some(Motion {
                    from,
                    started_at: now,
                    duration,
                });
            }
            track.cursor = center;
            track.stayed_since = now;
        }
        let Some((tail, fade)) = track
            .motion
            .as_ref()
            .and_then(|motion| motion.sample(now, center, settings))
        else {
            track.motion = None;
            return;
        };
        painter.ctx().request_repaint();
        let head = center + cursor.origin;
        let tail = tail + cursor.origin;
        let tangent = head - tail;
        if tangent.length_sq() < 0.25 {
            return;
        }
        let tangent = tangent.normalized();
        let normal = egui::vec2(-tangent.y, tangent.x);
        let head = head - tangent * (cursor.rect.width().min(cursor.rect.height()) * 0.5);
        let thickness = ((normal.x.abs() * cursor.rect.width()
            + normal.y.abs() * cursor.rect.height())
            * 0.5
            * settings.width_scale)
            .max(0.75);
        let color = Color32::from_hex(&settings.color)
            .unwrap_or(cursor.color)
            .gamma_multiply(settings.opacity * fade);
        if settings.appearance == Appearance::Glow {
            ribbon(
                painter,
                head,
                tail,
                normal * thickness * 1.65,
                color.gamma_multiply(0.2),
                false,
            );
        }
        ribbon(
            painter,
            head,
            tail,
            normal * thickness,
            color,
            settings.appearance == Appearance::Solid,
        );
    }
}

fn ribbon(painter: &Painter, head: Pos2, tail: Pos2, width: Vec2, color: Color32, solid: bool) {
    let mut mesh = Mesh::default();
    let tail_color = if solid { color } else { Color32::TRANSPARENT };
    let tail_width = width * if solid { 0.43 } else { 0.18 };
    mesh.colored_vertex(head + width, color);
    mesh.colored_vertex(head - width, color);
    mesh.colored_vertex(tail - tail_width, tail_color);
    mesh.colored_vertex(tail + tail_width, tail_color);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(egui::Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_route_cursor_trails_to_the_selected_views() {
        let mut settings = Settings::default();
        assert!(!settings.enabled_for(Target::Editor));
        assert!(!settings.enabled_for(Target::Terminal));
        settings.target = Target::Editor;
        assert!(settings.enabled_for(Target::Editor));
        assert!(!settings.enabled_for(Target::Terminal));
        settings.target = Target::Terminal;
        assert!(!settings.enabled_for(Target::Editor));
        assert!(settings.enabled_for(Target::Terminal));
        settings.target = Target::Both;
        assert!(settings.enabled_for(Target::Editor));
        assert!(settings.enabled_for(Target::Terminal));
    }

    #[test]
    fn trail_fades_and_catches_up_to_the_cursor() {
        let settings = Settings {
            direction: Direction::Out,
            style: Style::Linear,
            ..Default::default()
        };
        let from = Pos2::ZERO;
        let target = egui::pos2(100.0, 40.0);
        let motion = Motion {
            from,
            started_at: 1.0,
            duration: 0.25,
        };
        assert_eq!(motion.sample(1.0, target, &settings), Some((from, 1.0)));
        let (middle, opacity) = motion.sample(1.125, target, &settings).unwrap();
        assert_eq!(middle, egui::pos2(50.0, 20.0));
        assert!(opacity < 1.0 && opacity > 0.0);
        assert!(motion.sample(1.25, target, &settings).is_none());
    }
}
