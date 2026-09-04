//! Presentation-only mouse animation, sampled on the source recording clock.
//! Geometry and timing match Drive: a neutral arrow, 220ms smoothstep travel,
//! 180ms anticipation, 700ms linger, and 120ms fades. No synthetic app input.

use super::Entry;
use crate::mouse::{Action, MouseEvent};
use crate::render;

const LEAD: f64 = 180.0;
const LINGER: f64 = 700.0;
const MOTION: f64 = 220.0;
const FADE: f64 = 120.0;

#[derive(Clone, Copy, Debug, Default)]
pub struct PointerOptions {
    /// Preserve fades but remove pointer travel and press scaling.
    pub reduced_motion: bool,
}

#[derive(Clone, Copy, Debug)]
struct ButtonTransition {
    at_ms: f64,
    action: Action,
    from: f64,
}

impl ButtonTransition {
    fn scale(self, at_ms: f64) -> f64 {
        let age = (at_ms - self.at_ms).max(0.0);
        match self.action {
            Action::Down => mix(self.from, 0.88, ease(age / 60.0)),
            Action::Up => mix(self.from, 1.0, ease(age / 120.0)),
            Action::Click if age < 50.0 => mix(self.from, 0.88, ease(age / 50.0)),
            Action::Click => mix(0.88, 1.0, ease((age - 50.0) / 120.0)),
            Action::Move => 1.0,
        }
    }
}

struct Point {
    at_ms: u64,
    event: MouseEvent,
    button: Option<ButtonTransition>,
}

pub(super) struct Track {
    points: Vec<Point>,
    options: PointerOptions,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PointerFrame {
    x: f64,
    y: f64,
    opacity: f64,
    scale: f64,
}

impl Track {
    pub(super) fn new(entries: &[Entry], options: PointerOptions) -> Self {
        let mut points = Vec::new();
        let mut button: Option<ButtonTransition> = None;
        for entry in entries {
            if let Entry::Mouse { at_ms, event, .. } = entry {
                if event.action != Action::Move {
                    button = Some(ButtonTransition {
                        at_ms: *at_ms as f64,
                        action: event.action,
                        from: button.map_or(1.0, |button| button.scale(*at_ms as f64)),
                    });
                }
                points.push(Point {
                    at_ms: *at_ms,
                    event: *event,
                    button,
                });
            }
        }
        Self { points, options }
    }

    pub(super) fn last_ms(&self) -> Option<u64> {
        self.points.last().map(|point| point.at_ms)
    }

    fn at(&self, at_ms: f64, cutoff: u64) -> Option<PointerFrame> {
        // A clip cannot anticipate a mouse action in material that was cut away.
        let points = &self.points[..self.points.partition_point(|point| point.at_ms <= cutoff)];
        let index = points.partition_point(|point| point.at_ms as f64 <= at_ms);
        let previous = index.checked_sub(1).map(|index| &points[index]);
        let next = points.get(index);
        let button = previous.and_then(|point| point.button);
        let held = button.is_some_and(|button| button.action == Action::Down);
        let fade_out = previous.map_or(0.0, |point| {
            1.0 - ease((at_ms - point.at_ms as f64 - LINGER + FADE) / FADE)
        });
        let fade_in = next.map_or(0.0, |point| {
            ease((at_ms - point.at_ms as f64 + LEAD) / FADE)
        });
        let opacity = if held { 1.0 } else { fade_out.max(fade_in) };
        if opacity <= 0.0 {
            return None;
        }

        let destination = if self.options.reduced_motion {
            previous.filter(|_| held || fade_out > 0.0).or(next)?
        } else {
            next.filter(|point| at_ms >= point.at_ms as f64 - MOTION)
                .or(previous)?
        };
        let connected = !self.options.reduced_motion
            && previous.is_some_and(|point| {
                held || destination.at_ms.saturating_sub(point.at_ms) as f64 <= LINGER + LEAD
            });
        let origin = if connected {
            previous.unwrap()
        } else {
            destination
        };
        let start = (origin.at_ms as f64).max(destination.at_ms as f64 - MOTION);
        let amount = if connected && destination.at_ms as f64 > start {
            ease((at_ms - start) / (destination.at_ms as f64 - start))
        } else {
            1.0
        };
        Some(PointerFrame {
            x: mix(
                f64::from(origin.event.x),
                f64::from(destination.event.x),
                amount,
            ),
            y: mix(
                f64::from(origin.event.y),
                f64::from(destination.event.y),
                amount,
            ),
            opacity,
            scale: if self.options.reduced_motion {
                1.0
            } else {
                button.map_or(1.0, |button| button.scale(at_ms))
            },
        })
    }

    pub(super) fn svg(
        &self,
        at_ms: f64,
        cutoff: u64,
        cols: u16,
        rows: u16,
        options: &render::Options,
    ) -> String {
        let Some(pointer) = self.at(at_ms, cutoff) else {
            return String::new();
        };
        if pointer.x >= f64::from(cols) || pointer.y >= f64::from(rows) {
            return String::new();
        }
        let x = f64::from(options.padding) + (pointer.x + 0.5) * f64::from(options.cell_width);
        let y = f64::from(options.padding) + (pointer.y + 0.5) * f64::from(options.cell_height);
        // Clip to the actual viewport, never the padded video canvas or its captions.
        // Scale around the tip so a press cannot shift the apparent target cell.
        format!(
            r##"<defs><clipPath id="pointer-clip"><rect x="{padding}" y="{padding}" width="{width}" height="{height}"/></clipPath></defs><g clip-path="url(#pointer-clip)"><g transform="translate({x:.3} {y:.3})" opacity="{opacity:.4}"><g transform="scale({scale:.4})"><path d="M0 0 L1 20 L6 15 L10 23 L14 21 L10 13 L17 13 Z" transform="translate(0 1)" fill="#000" stroke="#000" stroke-width="3" stroke-linejoin="round" opacity="0.25"/><path d="M0 0 L1 20 L6 15 L10 23 L14 21 L10 13 L17 13 Z" fill="#f7f7f7" stroke="#202020" stroke-width="1.5" stroke-linejoin="round"/></g></g></g>"##,
            padding = options.padding,
            width = f32::from(cols) * options.cell_width,
            height = f32::from(rows) * options.cell_height,
            opacity = pointer.opacity,
            scale = pointer.scale,
        )
    }
}

fn ease(value: f64) -> f64 {
    let t = value.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn mix(from: f64, to: f64, amount: f64) -> f64 {
    from + (to - from) * amount
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(events: &[(u64, Action, u16)]) -> Track {
        Track::new(
            &events
                .iter()
                .map(|&(at_ms, action, x)| Entry::Mouse {
                    at_ms,
                    event: MouseEvent::new(action, x, 2),
                    bytes: vec![],
                })
                .collect::<Vec<_>>(),
            PointerOptions::default(),
        )
    }

    #[test]
    fn reveal_press_and_fade_are_smooth_and_tip_stays_anchored() {
        let track = track(&[(1000, Action::Click, 10)]);
        assert!(track.at(800.0, u64::MAX).is_none());
        assert!(track.at(880.0, u64::MAX).unwrap().opacity < 1.0);
        assert_eq!(track.at(1000.0, u64::MAX).unwrap().scale, 1.0);
        let pressed = track.at(1050.0, u64::MAX).unwrap();
        assert_eq!((pressed.x, pressed.scale), (10.0, 0.88));
        assert_eq!(track.at(1170.0, u64::MAX).unwrap().scale, 1.0);
        assert!(track.at(1650.0, u64::MAX).unwrap().opacity < 1.0);
        assert!(track.at(1700.0, u64::MAX).is_none());
    }

    #[test]
    fn travel_arrives_at_input_time_without_crossing_hidden_gaps() {
        let track = track(&[
            (1000, Action::Move, 10),
            (1500, Action::Click, 30),
            (5000, Action::Move, 80),
        ]);
        assert_eq!(track.at(1390.0, u64::MAX).unwrap().x, 20.0);
        assert_eq!(track.at(1500.0, u64::MAX).unwrap().x, 30.0);
        assert!(track.at(3000.0, u64::MAX).is_none());
        assert_eq!(track.at(4900.0, u64::MAX).unwrap().x, 80.0);
        assert_eq!(track.at(1390.0, 1400).unwrap().x, 10.0);
    }

    #[test]
    fn held_drag_stays_visible_and_release_retargets_current_scale() {
        let track = track(&[
            (1000, Action::Down, 10),
            (1030, Action::Up, 10),
            (2000, Action::Down, 10),
            (5000, Action::Move, 20),
            (6000, Action::Up, 20),
        ]);
        let before = track.at(1029.999, u64::MAX).unwrap().scale;
        let after = track.at(1030.0, u64::MAX).unwrap().scale;
        assert!((before - after).abs() < 0.0001);
        assert_eq!(track.at(4000.0, u64::MAX).unwrap().opacity, 1.0);
        assert_eq!(track.at(5900.0, u64::MAX).unwrap().scale, 0.88);
        assert!(track.at(6700.0, u64::MAX).is_none());
    }

    #[test]
    fn reduced_motion_has_no_travel_or_compression_and_clips_to_viewport() {
        let mut track = track(&[
            (1000, Action::Click, 10),
            (1500, Action::Click, 30),
            (5000, Action::Move, 15),
        ]);
        track.options.reduced_motion = true;
        assert_eq!(track.at(1390.0, u64::MAX).unwrap().x, 10.0);
        assert_eq!(track.at(1550.0, u64::MAX).unwrap().scale, 1.0);
        assert!(
            track
                .svg(1550.0, u64::MAX, 20, 10, &render::Options::default())
                .is_empty()
        );
        let svg = track.svg(1550.0, u64::MAX, 40, 10, &render::Options::default());
        assert!(svg.contains("pointer-clip"));
        assert!(!svg.contains("NaN"));
        assert_eq!(track.at(4900.0, u64::MAX).unwrap().x, 15.0);
    }

    #[test]
    fn rapid_repeated_clicks_do_not_snap_or_accumulate_animation() {
        let track = track(&[
            (1000, Action::Click, 10),
            (1080, Action::Click, 10),
            (1100, Action::Move, 15),
        ]);
        assert!(
            (track.at(1079.999, u64::MAX).unwrap().scale
                - track.at(1080.0, u64::MAX).unwrap().scale)
                .abs()
                < 0.0001
        );
        assert_eq!(track.at(1250.0, u64::MAX).unwrap().scale, 1.0);
        assert_eq!(track.at(1100.0, u64::MAX).unwrap().x, 15.0);
        for ms in 900..1800 {
            if let Some(frame) = track.at(f64::from(ms), u64::MAX) {
                assert!(frame.x.is_finite() && frame.y.is_finite());
                assert!((0.88..=1.0).contains(&frame.scale));
                assert!((0.0..=1.0).contains(&frame.opacity));
            }
        }
    }
}
