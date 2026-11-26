/// Locate cancellation before the first real string terminator. A fresh ESC
/// abandons the string and is parsed again; CAN/SUB are consumed. The bool says
/// that ESC was the final byte of the previous read and must be restored.
pub fn control_string_abort(
    input: &[u8],
    previous_escape: bool,
    osc: bool,
) -> Option<(usize, bool)> {
    if previous_escape {
        match input.first().copied()? {
            b'\\' => return None,
            0x18 | 0x1a => return Some((1, false)),
            _ => return Some((0, true)),
        }
    }
    for (index, byte) in input.iter().copied().enumerate() {
        match byte {
            0x18 | 0x1a => return Some((index + 1, false)),
            0x07 if osc => return None,
            0x1b => match input.get(index + 1) {
                Some(b'\\') | None => return None,
                Some(_) => return Some((index, false)),
            },
            _ => {}
        }
    }
    None
}
