//! Vim-owned cursor appearance and a renderer-independent idle blink clock.

use super::{Vim9PixelRect, Vim9RenderCursor, Vim9RenderFrame, Vim9RenderStyle};
use serde::Deserialize;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Vim9CursorShape {
    #[default]
    Block,
    Vertical,
    Horizontal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9CursorAppearance {
    pub shape: Vim9CursorShape,
    pub percentage: u16,
    pub style: Vim9RenderStyle,
    pub blinkwait: u32,
    pub blinkon: u32,
    pub blinkoff: u32,
}

impl Default for Vim9CursorAppearance {
    fn default() -> Self {
        Self {
            shape: Vim9CursorShape::Block,
            percentage: 100,
            style: Vim9RenderStyle {
                reverse: true,
                ..Default::default()
            },
            blinkwait: 700,
            blinkon: 400,
            blinkoff: 250,
        }
    }
}

impl Vim9CursorAppearance {
    pub(super) fn valid(&self) -> bool {
        (1..=100).contains(&self.percentage)
            && [
                self.style.foreground,
                self.style.background,
                self.style.special,
            ]
            .into_iter()
            .flatten()
            .all(|c| c <= 0xff_ffff)
    }

    /// Preserve the full source cell for IME/hit testing; only its paint changes.
    /// A vertical cursor over CJK uses a percentage of one cell, as Vim does.
    pub fn paint_rect(&self, cell: Vim9PixelRect, cell_width: f32, scale: f32) -> Vim9PixelRect {
        let pixel = if scale.is_finite() && scale > 0.0 {
            scale.recip()
        } else {
            1.0
        };
        let fraction = f32::from(self.percentage.clamp(1, 100)) / 100.0;
        let snap = |size: f32, limit: f32| ((size / pixel).ceil().max(1.0) * pixel).min(limit);
        match self.shape {
            Vim9CursorShape::Block => cell,
            Vim9CursorShape::Vertical => Vim9PixelRect {
                width: snap(cell_width.min(cell.width) * fraction, cell.width),
                ..cell
            },
            Vim9CursorShape::Horizontal => {
                let height = snap(cell.height * fraction, cell.height);
                Vim9PixelRect {
                    y: cell.y + cell.height - height,
                    height,
                    ..cell
                }
            }
        }
    }

    /// Resolve against the actual text beneath the caret, including syntax and
    /// Visual selection. An unspecified cursor group reverses that text.
    pub fn colors(&self, under: &Vim9RenderStyle, normal: &Vim9RenderStyle) -> (u32, u32) {
        let mut fg = under.foreground.or(normal.foreground).unwrap_or(0xd8dee9);
        let mut bg = under.background.or(normal.background).unwrap_or(0x20242c);
        if under.reverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        fg = self.style.foreground.unwrap_or(fg);
        bg = self.style.background.unwrap_or(bg);
        if self.style.reverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        (fg, bg)
    }
}

type CursorKey = (i64, i64, u64, Vim9RenderCursor, String, bool);

#[derive(Clone, Debug, Default)]
pub struct Vim9CursorBlink {
    key: Option<CursorKey>,
    started: Option<Instant>,
}

impl Vim9CursorBlink {
    /// An accepted key or IME commit restarts the wait even at an unchanged cell.
    pub fn reset(&mut self) {
        self.key = None;
        self.started = None;
    }

    /// Returns visibility and the next transition. Idle paint revisions never
    /// restart the clock; unfocused/steady cursors never request animation.
    pub fn sample(
        &mut self,
        frame: &Vim9RenderFrame,
        focused: bool,
        now: Instant,
    ) -> (bool, Option<Duration>) {
        let key = (
            frame.window,
            frame.buffer,
            frame.changedtick,
            frame.cursor.clone(),
            frame.mode.clone(),
            focused,
        );
        if self.key.as_ref() != Some(&key) {
            self.key = Some(key);
            self.started = Some(now);
        }
        if !frame.cursor.visible {
            return (false, None);
        }
        let Some(a) = &frame.cursor.appearance else {
            return (true, None);
        };
        if !focused || a.blinkwait == 0 || a.blinkon == 0 || a.blinkoff == 0 {
            return (true, None);
        }
        let elapsed = now
            .saturating_duration_since(self.started.unwrap_or(now))
            .as_secs_f64()
            * 1000.0;
        let wait = f64::from(a.blinkwait);
        let (visible, remaining) = if elapsed < wait {
            (true, wait - elapsed)
        } else {
            let off = f64::from(a.blinkoff);
            let cycle = (elapsed - wait) % (off + f64::from(a.blinkon));
            (
                cycle >= off,
                if cycle < off {
                    off - cycle
                } else {
                    off + f64::from(a.blinkon) - cycle
                },
            )
        };
        (
            visible,
            Some(Duration::from_secs_f64((remaining / 1000.0).max(0.000_001))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_geometry_preserves_wide_source_and_physical_pixels() {
        let cell = Vim9PixelRect {
            x: 10.25,
            y: 30.5,
            width: 17.0,
            height: 19.5,
        };
        let mut a = Vim9CursorAppearance {
            shape: Vim9CursorShape::Vertical,
            percentage: 25,
            ..Default::default()
        };
        let bar = a.paint_rect(cell, 8.5, 2.0);
        assert_eq!(bar.width, 2.5);
        assert_eq!(bar.height, cell.height);
        a.shape = Vim9CursorShape::Horizontal;
        let bar = a.paint_rect(cell, 8.5, 2.0);
        assert_eq!(bar.height, 5.0);
        assert_eq!(bar.y + bar.height, cell.y + cell.height);
        a.shape = Vim9CursorShape::Block;
        assert_eq!(a.paint_rect(cell, 8.5, 2.0), cell);
    }

    #[test]
    fn cursor_rgb_reverses_actual_syntax_and_accepts_linked_gui_colors() {
        let normal = Vim9RenderStyle {
            foreground: Some(0x123456),
            background: Some(0x654321),
            ..Default::default()
        };
        let under = Vim9RenderStyle {
            foreground: Some(0xabcdef),
            ..Default::default()
        };
        let mut a = Vim9CursorAppearance::default();
        assert_eq!(a.colors(&under, &normal), (0x654321, 0xabcdef));
        a.style.foreground = Some(0x102030);
        a.style.background = Some(0xa0b0c0);
        a.style.reverse = false;
        assert_eq!(a.colors(&under, &normal), (0x102030, 0xa0b0c0));
        a.style.reverse = true;
        assert_eq!(a.colors(&under, &normal), (0xa0b0c0, 0x102030));
        a.percentage = 0;
        assert!(!a.valid());
    }

    #[test]
    fn blink_obeys_wait_off_on_and_does_not_restart_for_render_revisions() {
        let mut frame = Vim9RenderFrame::default();
        frame.cursor.visible = true;
        frame.cursor.appearance = Some(Vim9CursorAppearance::default());
        let mut blink = Vim9CursorBlink::default();
        let t = Instant::now();
        assert_eq!(
            blink.sample(&frame, true, t),
            (true, Some(Duration::from_millis(700)))
        );
        frame.revision += 1;
        assert!(!blink.sample(&frame, true, t + Duration::from_millis(700)).0);
        assert!(blink.sample(&frame, true, t + Duration::from_millis(950)).0);
        assert!(
            !blink
                .sample(&frame, true, t + Duration::from_millis(1350))
                .0
        );
        frame.cursor.column += 1;
        assert!(
            blink
                .sample(&frame, true, t + Duration::from_millis(1360))
                .0
        );
        blink.reset();
        assert!(
            blink
                .sample(&frame, true, t + Duration::from_millis(3000))
                .0
        );
        assert_eq!(blink.sample(&frame, false, t), (true, None));
        frame.cursor.appearance.as_mut().unwrap().blinkon = 0;
        assert_eq!(blink.sample(&frame, true, t), (true, None));
        frame.cursor.visible = false;
        assert_eq!(blink.sample(&frame, true, t), (false, None));
    }
}
