//! Frozen pure branches of Ember parser.rs at 9ca7891d58939a368c4e62ee9dd9ad338d944731.
//! Baseline parser SHA-256 b9798977d6691b0094fd0b27eab3d2c4cac4d025fc6e8adc45dddf3b1cd7fb13.
//! Copied independently of candidate: host arms replaced by transactional returns;
//! the remaining match arms are verbatim. Do not regenerate from the engine.
use jterm_engine::{protocol_state::ProtocolState, screen::ScreenState};
pub struct Frozen {
    pub screen: ScreenState,
    pub protocol: ProtocolState,
}
impl Frozen {
    pub fn set_mode(&mut self, mode: u16) -> bool {
        match mode {
            1004 | 47 | 1047 | 1048 | 1049 | 2026 | 5522 => return false,
            25 => {
                // Show cursor (mode 25)
                self.protocol.modes_mut().insert(25);
            }
            2004 => {
                // Bracketed paste mode
                self.protocol.modes_mut().insert(2004);
            }
            1000..=1003 => {
                // Mouse reporting modes
                self.protocol.modes_mut().insert(mode);
            }
            1006 => {
                // SGR mouse reporting format
                self.protocol.modes_mut().insert(mode);
            }
            7 => {
                // Autowrap mode
                self.protocol.modes_mut().insert(7);
            }
            6 => {
                // DECOM - 原点模式:寻址相对滚动区域,光标移到区域原点
                self.screen.origin_mode = true;
                self.screen.cursor_row = self.screen.scroll_region_top;
                self.screen.cursor_col = 0;
            }
            66 => {
                self.protocol.modes_mut().insert(66);
            }
            1007 => {
                self.protocol.modes_mut().insert(1007);
            }
            _ => {
                // Unknown mode, just store it
                self.protocol.modes_mut().insert(mode);
            }
        }
        true
    }
    pub fn reset_mode(&mut self, mode: u16) -> bool {
        match mode {
            1004 | 47 | 1047 | 1048 | 1049 | 2026 | 5522 => return false,
            25 => {
                // Hide cursor
                self.protocol.modes_mut().remove(&25);
            }
            2004 => {
                // Disable bracketed paste mode
                self.protocol.modes_mut().remove(&2004);
            }
            1000..=1003 => {
                // Disable mouse reporting
                self.protocol.modes_mut().remove(&mode);
            }
            1006 => {
                // Disable SGR mouse reporting format
                self.protocol.modes_mut().remove(&mode);
            }
            7 => {
                // Disable autowrap
                self.protocol.modes_mut().remove(&7);
            }
            6 => {
                // DECOM 关闭:恢复绝对寻址,光标移到屏幕原点
                self.screen.origin_mode = false;
                self.screen.cursor_row = 0;
                self.screen.cursor_col = 0;
            }
            66 => {
                self.protocol.modes_mut().remove(&66);
            }
            1007 => {
                self.protocol.modes_mut().remove(&1007);
            }
            _ => {
                // Unknown mode, just remove it
                self.protocol.modes_mut().remove(&mode);
            }
        }
        true
    }
    pub fn set_ansi_mode(&mut self, mode: u16) {
        if mode == 4 {
            // IRM - 插入替换模式
            self.screen.insert_mode = true;
        }
    }
    pub fn reset_ansi_mode(&mut self, mode: u16) {
        if mode == 4 {
            self.screen.insert_mode = false;
        }
    }
}
