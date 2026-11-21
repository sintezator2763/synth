use eframe::egui;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[serde(alias = "ease_in")]
    In,
    #[default]
    #[serde(alias = "ease_out")]
    Out,
    #[serde(alias = "ease_in_out", alias = "inout")]
    InOut,
    #[serde(alias = "ease_out_in", alias = "outin")]
    OutIn,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Style {
    Linear,
    Sine,
    #[default]
    #[serde(alias = "quardic")]
    Quadratic,
    #[serde(alias = "qubic")]
    Cubic,
    Quartic,
    Quintic,
    Exponential,
    Circular,
    Back,
    Elastic,
    Bounce,
}

impl Style {
    fn ease_in(self, t: f32) -> f32 {
        use std::f32::consts::PI;
        if t <= 0.0 || t >= 1.0 {
            return t;
        }
        match self {
            Self::Linear => t,
            Self::Sine => 1.0 - (t * PI / 2.0).cos(),
            Self::Quadratic => t * t,
            Self::Cubic => t.powi(3),
            Self::Quartic => t.powi(4),
            Self::Quintic => t.powi(5),
            Self::Exponential => 2.0_f32.powf(10.0 * (t - 1.0)),
            Self::Circular => 1.0 - (1.0 - t * t).sqrt(),
            Self::Back => 2.70158 * t.powi(3) - 1.70158 * t * t,
            Self::Elastic => {
                -2.0_f32.powf(10.0 * (t - 1.0)) * ((10.0 * t - 10.75) * (2.0 * PI / 3.0)).sin()
            }
            Self::Bounce => 1.0 - bounce_out(1.0 - t),
        }
    }

    fn ease_out(self, t: f32) -> f32 {
        1.0 - self.ease_in(1.0 - t)
    }

    fn progress(self, direction: Direction, t: f32) -> f32 {
        match direction {
            Direction::In => self.ease_in(t),
            Direction::Out => self.ease_out(t),
            Direction::InOut if t < 0.5 => self.ease_in(2.0 * t) / 2.0,
            Direction::InOut => (1.0 + self.ease_out(2.0 * t - 1.0)) / 2.0,
            Direction::OutIn if t < 0.5 => self.ease_out(2.0 * t) / 2.0,
            Direction::OutIn => (1.0 + self.ease_in(2.0 * t - 1.0)) / 2.0,
        }
    }
}

fn bounce_out(t: f32) -> f32 {
    let (shift, height) = if t < 1.0 / 2.75 {
        (0.0, 0.0)
    } else if t < 2.0 / 2.75 {
        (1.5 / 2.75, 0.75)
    } else if t < 2.5 / 2.75 {
        (2.25 / 2.75, 0.9375)
    } else {
        (2.625 / 2.75, 0.984375)
    };
    7.5625 * (t - shift).powi(2) + height
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Animation {
    pub duration_ms: u16,
    #[serde(alias = "easing")]
    pub direction: Direction,
    pub style: Style,
}

impl Default for Animation {
    fn default() -> Self {
        Self {
            duration_ms: 120,
            direction: Direction::Out,
            style: Style::Quadratic,
        }
    }
}

#[derive(Clone, Copy)]
struct Motion {
    started_at: f64,
    start: f32,
    end: f32,
    animation: Animation,
}

impl Motion {
    fn sample(self, now: f64) -> (f32, bool) {
        if self.start == self.end || self.animation.duration_ms == 0 {
            return (self.end, false);
        }
        let elapsed_ms = (now - self.started_at).max(0.0) * 1000.0;
        let phase = (elapsed_ms / f64::from(self.animation.duration_ms)) as f32;
        if phase >= 1.0 {
            return (self.end, false);
        }
        let eased = self
            .animation
            .style
            .progress(self.animation.direction, phase);
        (egui::lerp(self.start..=self.end, eased), true)
    }

    fn retarget(self, now: f64, end: f32, animation: Animation) -> Self {
        let (current, moving) = self.sample(now);
        if end != self.end || (moving && animation != self.animation) {
            return Self {
                started_at: now,
                start: current,
                end,
                animation,
            };
        }
        self
    }
}

impl Animation {
    pub fn progress(
        self,
        ctx: &egui::Context,
        name: &'static str,
        visible: bool,
        enabled: bool,
    ) -> f32 {
        let id = egui::Id::new(name);
        let now = ctx.input(|input| input.time);
        let end = if visible { 1.0 } else { 0.0 };
        let settled = Motion {
            started_at: now,
            start: end,
            end,
            animation: self,
        };
        let (progress, moving) = ctx.data_mut(|data| {
            let motion = if enabled && self.duration_ms != 0 {
                data.get_temp::<Motion>(id)
                    .unwrap_or(settled)
                    .retarget(now, end, self)
            } else {
                settled
            };
            data.insert_temp(id, motion);
            motion.sample(now)
        });
        if moving {
            ctx.request_repaint();
        }
        progress.clamp(0.0, 1.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_covers_both_directions_and_combined_curves() {
        let curve = Style::Cubic;
        assert_eq!(curve.progress(Direction::In, 0.5), 0.125);
        assert_eq!(curve.progress(Direction::Out, 0.5), 0.875);
        assert_eq!(curve.progress(Direction::InOut, 0.25), 0.0625);
        assert_eq!(curve.progress(Direction::OutIn, 0.25), 0.4375);
        for style in [Style::Elastic, Style::Bounce, Style::Back] {
            for direction in [
                Direction::In,
                Direction::Out,
                Direction::InOut,
                Direction::OutIn,
            ] {
                assert_eq!(style.progress(direction, 0.0), 0.0);
                assert_eq!(style.progress(direction, 1.0), 1.0);
                assert!(style.progress(direction, 0.4).is_finite());
            }
        }
    }

    #[test]
    fn reversing_a_panel_keeps_its_current_position() {
        let opening = Animation {
            duration_ms: 200,
            direction: Direction::Out,
            style: Style::Cubic,
        };
        let closing = Animation {
            duration_ms: 150,
            direction: Direction::In,
            style: Style::Bounce,
        };
        let motion = Motion {
            started_at: 0.0,
            start: 0.0,
            end: 1.0,
            animation: opening,
        };
        let reverse = motion.retarget(0.07, 0.0, closing);
        assert_eq!(motion.sample(0.07).0, reverse.sample(0.07).0);
        assert_eq!(reverse.sample(0.23), (0.0, false));
    }
}
