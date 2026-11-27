//! XTerm's bounded video-rendition stack. Cell ownership and protection are not SGR.
use crate::{
    grid::{Color, StyleFlags},
    screen::ScreenState,
};

#[derive(Clone, Default)]
pub(crate) struct SgrStack {
    frames: Vec<Saved>,
    overflow: u64,
}
#[derive(Clone)]
struct Saved {
    mask: u32,
    flags: StyleFlags,
    foreground: Color,
    background: Color,
}

impl SgrStack {
    pub(crate) fn push(&mut self, screen: &ScreenState, params: &[u16]) {
        if self.frames.len() == 10 {
            self.overflow = self.overflow.saturating_add(1);
            return;
        }
        let mask = if params.is_empty() {
            u32::MAX
        } else {
            params.iter().fold(0, |mask, &p| {
                let p = match p {
                    10 => 30,
                    11 => 31,
                    p => p,
                };
                if (1..=31).contains(&p) {
                    mask | (1 << (p - 1))
                } else {
                    mask
                }
            })
        };
        self.frames.push(Saved {
            mask,
            flags: screen.current_flags,
            foreground: screen.current_fg,
            background: screen.current_bg,
        });
    }
    pub(crate) fn pop(&mut self, screen: &mut ScreenState) -> Option<Color> {
        if self.overflow != 0 {
            self.overflow -= 1;
            return None;
        }
        let saved = self.frames.pop()?;
        let has = |p: u32| saved.mask & (1 << (p - 1)) != 0;
        let flags = &mut screen.current_flags;
        if has(1) {
            flags.set_bold(saved.flags.bold());
        }
        if has(2) {
            flags.set_dim(saved.flags.dim());
        }
        if has(3) {
            flags.set_italic(saved.flags.italic());
        }
        if has(4) || has(21) {
            flags.set_underline(saved.flags.underline());
        }
        if has(5) {
            flags.set_blink(saved.flags.blink());
        }
        if has(7) {
            flags.set_inverse(saved.flags.inverse());
        }
        if has(8) {
            flags.set_concealed(saved.flags.concealed());
        }
        if has(9) {
            flags.set_strikethrough(saved.flags.strikethrough());
        }
        if saved.mask == u32::MAX {
            flags.set_overline(saved.flags.overline());
        }
        if has(30) {
            screen.current_fg = saved.foreground;
        }
        if has(31) && screen.current_bg != saved.background {
            screen.current_bg = saved.background;
            return Some(saved.background);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::UnderlineStyle;
    #[test]
    fn selection_overflow_and_ownership_are_independent() {
        let mut screen = ScreenState::new(2, 4);
        let mut stack = SgrStack::default();
        screen.current_fg = Color::Rgb(1, 2, 3);
        screen.current_bg = Color::Blue;
        screen.current_flags.set_underline(UnderlineStyle::Curly);
        stack.push(&screen, &[30, 4]);
        screen.current_flags.set_protected(true);
        screen.current_flags.set_bold(true);
        screen.current_flags.set_underline(UnderlineStyle::None);
        screen.current_fg = Color::Red;
        screen.current_bg = Color::Green;
        assert_eq!(stack.pop(&mut screen), None);
        assert_eq!(screen.current_fg, Color::Rgb(1, 2, 3));
        assert_eq!(screen.current_bg, Color::Green);
        assert!(screen.current_flags.protected() && screen.current_flags.bold());
        assert_eq!(screen.current_flags.underline(), UnderlineStyle::Curly);
        stack.push(&screen, &[10, 11]);
        screen.current_fg = Color::Black;
        screen.current_bg = Color::Black;
        assert_eq!(stack.pop(&mut screen), Some(Color::Green));
        assert_eq!(screen.current_fg, Color::Rgb(1, 2, 3));
        stack.push(&screen, &[0]);
        screen.current_fg = Color::Yellow;
        stack.pop(&mut screen);
        assert_eq!(screen.current_fg, Color::Yellow);
        for n in 0..12 {
            screen.current_fg = Color::Indexed(n);
            stack.push(&screen, &[]);
        }
        screen.current_fg = Color::White;
        for _ in 0..2 {
            stack.pop(&mut screen);
            assert_eq!(screen.current_fg, Color::White);
        }
        for n in (0..10).rev() {
            stack.pop(&mut screen);
            assert_eq!(screen.current_fg, Color::Indexed(n));
        }
        assert!(stack.frames.is_empty());
    }
}
