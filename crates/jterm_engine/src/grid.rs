use super::hyperlink::HyperlinkId;
use smallvec::SmallVec;

/// 连续内存网格存储 - 优化内存局部性和缓存命中率
/// 内存布局：`cells[row * cols + col]` 对应 `grid[row][col]`。
#[derive(Clone)]
pub struct TerminalGrid {
    cells: Vec<TerminalCell>,
    rows: usize,
    cols: usize,
    pub row_wrapped: Vec<bool>,
    /// Stable identity travels with retained physical rows across structural
    /// moves. `UNTRACKED` is fail-closed after allocator exhaustion.
    row_ids: Vec<RawRowId>,
}

impl TerminalGrid {
    pub fn new(rows: usize, cols: usize) -> Self {
        TerminalGrid {
            cells: vec![TerminalCell::default(); rows * cols],
            rows,
            cols,
            row_wrapped: vec![false; rows],
            row_ids: vec![RawRowId::UNTRACKED; rows],
        }
    }

    /// Flat cells with a fixed length. Hosts must preserve wide-cell invariants.
    pub fn cells_mut(&mut self) -> &mut [TerminalCell] {
        &mut self.cells
    }

    /// Stable row identities in physical row order.
    pub fn row_ids(&self) -> &[RawRowId] {
        &self.row_ids
    }

    /// Fixed-length identity view for host-owned allocation and region moves.
    /// The host must keep identities synchronized with physical cell moves.
    pub fn row_ids_mut(&mut self) -> &mut [RawRowId] {
        &mut self.row_ids
    }

    #[inline]
    pub fn get(&self, row: usize, col: usize) -> &TerminalCell {
        &self.cells[row * self.cols + col]
    }

    #[inline]
    pub fn get_mut(&mut self, row: usize, col: usize) -> &mut TerminalCell {
        &mut self.cells[row * self.cols + col]
    }

    #[inline]
    pub fn rows(&self) -> usize {
        self.rows
    }

    #[inline]
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// 返回行数（兼容 `grid[i].len()`）。
    #[inline]
    pub fn row_len(&self) -> usize {
        self.cols
    }

    #[inline]
    pub fn row_id(&self, row: usize) -> RawRowId {
        self.row_ids
            .get(row)
            .copied()
            .unwrap_or(RawRowId::UNTRACKED)
    }

    /// 获取所有行为 `Vec<Vec<_>>`（用于兼容旧代码）。
    pub fn to_vec(&self) -> Vec<Vec<TerminalCell>> {
        self.cells
            .chunks_exact(self.cols)
            .map(|chunk| chunk.to_vec())
            .collect()
    }

    /// 在行内指定列插入一个cell，右侧cell右移，末尾cell被丢弃
    pub fn insert_cell_in_row(&mut self, row: usize, col: usize, cell: TerminalCell) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        let start = row * self.cols;
        self.cells
            .copy_within(start + col..start + self.cols - 1, start + col + 1);
        self.cells[start + col] = cell;
    }

    /// 删除行内指定列的cell，右侧cell左移，末尾补blank
    pub fn remove_cell_from_row(&mut self, row: usize, col: usize) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        let start = row * self.cols;
        self.cells
            .copy_within(start + col + 1..start + self.cols, start + col);
        // Fill last cell with default
        self.cells[start + self.cols - 1] = TerminalCell::default();
    }

    /// Clear a double-width character left holding only its lead half at the
    /// right edge. The renderer paints `wide()` cells two columns wide, and a
    /// later shift would carry the half character back into the middle of the
    /// row, where it hides or overpaints a neighbour. Callers use this after an
    /// operation that can push a continuation cell off the row: an ICH/IRM
    /// insert, or a resize that narrows the grid.
    pub fn clear_dangling_wide_at_row_end(&mut self, row: usize, blank: TerminalCell) {
        if row >= self.rows || self.cols == 0 {
            return;
        }
        let last = row * self.cols + self.cols - 1;
        if self.cells[last].flags.wide() {
            self.cells[last] = blank;
        }
    }

    /// 是否为空
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// 调整网格大小
    pub fn resize(&mut self, new_rows: usize, new_cols: usize, default_cell: TerminalCell) {
        let mut new_cells = vec![default_cell; new_rows * new_cols];
        let copy_rows = self.rows.min(new_rows);
        let copy_cols = self.cols.min(new_cols);
        for row in 0..copy_rows {
            let src_start = row * self.cols;
            let dst_start = row * new_cols;
            new_cells[dst_start..dst_start + copy_cols]
                .copy_from_slice(&self.cells[src_start..src_start + copy_cols]);
        }
        self.cells = new_cells;
        let mut new_wrapped = vec![false; new_rows];
        new_wrapped[..copy_rows].copy_from_slice(&self.row_wrapped[..copy_rows]);
        self.row_wrapped = new_wrapped;
        let mut new_row_ids = vec![RawRowId::UNTRACKED; new_rows];
        new_row_ids[..copy_rows].copy_from_slice(&self.row_ids[..copy_rows]);
        self.row_ids = new_row_ids;
        self.rows = new_rows;
        self.cols = new_cols;
    }

    /// 向上滚动 n 行:丢弃顶部 n 行,其余行上移,底部补 blank。缩小高度前调用,
    /// 把底部内容(光标行/近期输出)上移保留;调用方负责把被丢弃的顶部行存入 scrollback。
    pub fn scroll_up_by(&mut self, n: usize, blank: TerminalCell) {
        if n == 0 {
            return;
        }
        if n >= self.rows {
            self.cells.fill(blank);
            for w in self.row_wrapped.iter_mut() {
                *w = false;
            }
            self.row_ids.fill(RawRowId::UNTRACKED);
            return;
        }
        let cols = self.cols;
        self.cells.copy_within(n * cols.., 0);
        let keep = (self.rows - n) * cols;
        self.cells[keep..].fill(blank);
        self.row_wrapped.copy_within(n.., 0);
        for w in self.row_wrapped[self.rows - n..].iter_mut() {
            *w = false;
        }
        self.row_ids.copy_within(n.., 0);
        self.row_ids[self.rows - n..].fill(RawRowId::UNTRACKED);
    }

    /// 获取mut访问所有行
    pub fn iter_mut(&mut self) -> std::slice::ChunksMut<'_, TerminalCell> {
        self.cells.chunks_mut(self.cols)
    }

    /// 获取只读访问所有行
    pub fn iter(&self) -> std::slice::Chunks<'_, TerminalCell> {
        self.cells.chunks(self.cols)
    }
}

impl std::ops::Index<usize> for TerminalGrid {
    type Output = [TerminalCell];
    fn index(&self, row: usize) -> &[TerminalCell] {
        let start = row * self.cols;
        &self.cells[start..start + self.cols]
    }
}

impl std::ops::IndexMut<usize> for TerminalGrid {
    fn index_mut(&mut self, row: usize) -> &mut [TerminalCell] {
        let start = row * self.cols;
        &mut self.cells[start..start + self.cols]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    Indexed(u8),
    Rgb(u8, u8, u8),
    Default,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block, // 0 or 1 - block cursor (default)
    Underline, // 2 - underline cursor
    Beam,      // 3 - beam/vertical line cursor
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,  // SGR 4:3
    Dotted, // SGR 4:4
    Dashed, // SGR 4:5
}

/// Packed style flags in a u16 bitfield (includes wide character bits).
/// Layout:
///   bit 0: bold
///   bit 1: italic
///   bit 2-4: underline style (3 bits, 0-5)
///   bit 5: inverse
///   bit 6: dim
///   bit 7: blink
///   bit 8: strikethrough
///   bit 9: wide
///   bit 10: wide_continuation
///   bit 11: protected (DECSCA)
#[derive(Clone, Copy, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct StyleFlags(u16);

impl std::fmt::Debug for StyleFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StyleFlags")
            .field("bold", &self.bold())
            .field("italic", &self.italic())
            .field("underline", &self.underline())
            .field("inverse", &self.inverse())
            .field("dim", &self.dim())
            .field("blink", &self.blink())
            .field("strikethrough", &self.strikethrough())
            .field("protected", &self.protected())
            .finish()
    }
}

const BOLD_BIT: u16 = 1 << 0;
const ITALIC_BIT: u16 = 1 << 1;
const UNDERLINE_SHIFT: u32 = 2;
const UNDERLINE_MASK: u16 = 0b111 << 2;
const INVERSE_BIT: u16 = 1 << 5;
const DIM_BIT: u16 = 1 << 6;
const BLINK_BIT: u16 = 1 << 7;
const STRIKETHROUGH_BIT: u16 = 1 << 8;
const WIDE_BIT: u16 = 1 << 9;
const WIDE_CONT_BIT: u16 = 1 << 10;
const PROTECTED_BIT: u16 = 1 << 11;

impl StyleFlags {
    #[inline(always)]
    pub fn new() -> Self {
        Self(0)
    }

    #[inline(always)]
    pub fn bold(&self) -> bool {
        self.0 & BOLD_BIT != 0
    }
    #[inline(always)]
    pub fn italic(&self) -> bool {
        self.0 & ITALIC_BIT != 0
    }
    #[inline(always)]
    pub fn underline(&self) -> UnderlineStyle {
        match (self.0 & UNDERLINE_MASK) >> UNDERLINE_SHIFT {
            1 => UnderlineStyle::Single,
            2 => UnderlineStyle::Double,
            3 => UnderlineStyle::Curly,
            4 => UnderlineStyle::Dotted,
            5 => UnderlineStyle::Dashed,
            _ => UnderlineStyle::None,
        }
    }
    #[inline(always)]
    pub fn inverse(&self) -> bool {
        self.0 & INVERSE_BIT != 0
    }
    #[inline(always)]
    pub fn dim(&self) -> bool {
        self.0 & DIM_BIT != 0
    }
    #[inline(always)]
    pub fn blink(&self) -> bool {
        self.0 & BLINK_BIT != 0
    }
    #[inline(always)]
    pub fn strikethrough(&self) -> bool {
        self.0 & STRIKETHROUGH_BIT != 0
    }
    #[inline(always)]
    pub fn wide(&self) -> bool {
        self.0 & WIDE_BIT != 0
    }
    #[inline(always)]
    pub fn wide_continuation(&self) -> bool {
        self.0 & WIDE_CONT_BIT != 0
    }
    #[inline(always)]
    pub fn protected(&self) -> bool {
        self.0 & PROTECTED_BIT != 0
    }

    #[inline(always)]
    pub fn set_bold(&mut self, v: bool) {
        if v {
            self.0 |= BOLD_BIT;
        } else {
            self.0 &= !BOLD_BIT;
        }
    }
    #[inline(always)]
    pub fn set_italic(&mut self, v: bool) {
        if v {
            self.0 |= ITALIC_BIT;
        } else {
            self.0 &= !ITALIC_BIT;
        }
    }
    #[inline(always)]
    pub fn set_underline(&mut self, v: UnderlineStyle) {
        self.0 = (self.0 & !UNDERLINE_MASK) | ((v as u16) << UNDERLINE_SHIFT);
    }
    #[inline(always)]
    pub fn set_inverse(&mut self, v: bool) {
        if v {
            self.0 |= INVERSE_BIT;
        } else {
            self.0 &= !INVERSE_BIT;
        }
    }
    #[inline(always)]
    pub fn set_dim(&mut self, v: bool) {
        if v {
            self.0 |= DIM_BIT;
        } else {
            self.0 &= !DIM_BIT;
        }
    }
    #[inline(always)]
    pub fn set_blink(&mut self, v: bool) {
        if v {
            self.0 |= BLINK_BIT;
        } else {
            self.0 &= !BLINK_BIT;
        }
    }
    #[inline(always)]
    pub fn set_strikethrough(&mut self, v: bool) {
        if v {
            self.0 |= STRIKETHROUGH_BIT;
        } else {
            self.0 &= !STRIKETHROUGH_BIT;
        }
    }
    #[inline(always)]
    pub fn set_wide(&mut self, v: bool) {
        if v {
            self.0 |= WIDE_BIT;
        } else {
            self.0 &= !WIDE_BIT;
        }
    }
    #[inline(always)]
    pub fn set_wide_continuation(&mut self, v: bool) {
        if v {
            self.0 |= WIDE_CONT_BIT;
        } else {
            self.0 &= !WIDE_CONT_BIT;
        }
    }
    #[inline(always)]
    pub fn set_protected(&mut self, v: bool) {
        if v {
            self.0 |= PROTECTED_BIT;
        } else {
            self.0 &= !PROTECTED_BIT;
        }
    }

    #[inline(always)]
    pub fn is_default_style(&self) -> bool {
        self.0 & 0x1FF == 0
    }
}

/// Stable identity of one retained physical raw row. Zero means the row is
/// deliberately untracked (allocator exhausted); it never aliases a real id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RawRowId(u64);

impl RawRowId {
    pub const UNTRACKED: Self = Self(0);

    #[allow(dead_code)] // Public library accessor; binary keeps the id opaque.
    pub const fn get(self) -> Option<u64> {
        if self.0 == 0 {
            None
        } else {
            Some(self.0)
        }
    }

    pub const fn is_tracked(self) -> bool {
        self.0 != 0
    }

    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug)]
pub struct ScrollbackLine {
    data: CompressedLineData,
    pub is_wrapped: bool,
    cols: u16,
    /// Number of cells retained when historical soft-wrapped rows are joined
    /// before being split at the current terminal width.  Computing this when
    /// the line is compressed lets viewport reflow count rows without first
    /// allocating and decoding every line in the scrollback tail.
    reflow_content_len: u16,
    /// Physical columns containing wide-character continuation cells inside
    /// the exact prefix retained by projection reflow. Keeping this compact
    /// geometry beside the compressed row lets the projection planner avoid
    /// decoding historical cells merely to preserve wide glyph boundaries.
    reflow_wide_continuations: SmallVec<[u16; 2]>,
    raw_row_id: RawRowId,
}

#[derive(Clone, Debug)]
enum CompressedLineData {
    Plain(String, u16),
    Encoded(Vec<u8>),
}

impl ScrollbackLine {
    #[inline]
    pub fn columns(&self) -> usize {
        self.cols as usize
    }

    pub fn compress(cells: &[TerminalCell], is_wrapped: bool) -> Self {
        let cols = cells.len() as u16;
        let reflow_content_len = cells
            .iter()
            .rposition(|cell| !cell.is_reflow_trimmable_blank())
            .map_or(0, |index| index + 1);
        let reflow_wide_continuations = cells[..reflow_content_len]
            .iter()
            .enumerate()
            .filter_map(|(column, cell)| {
                if cell.flags.wide_continuation() {
                    u16::try_from(column).ok()
                } else {
                    None
                }
            })
            .collect();
        let reflow_content_len = reflow_content_len as u16;
        let trailing_blanks = cells
            .iter()
            .rev()
            .take_while(|c| {
                c.character == ' '
                    && c.foreground == Color::Default
                    && c.background == Color::Default
                    && c.flags.is_default_style()
                    && !c.flags.wide()
                    && !c.flags.wide_continuation()
                    && c.hyperlink_id.is_none()
            })
            .count();

        let active_len = cells.len() - trailing_blanks;
        let all_default_attrs = cells[..active_len].iter().all(|c| {
            c.foreground == Color::Default
                && c.background == Color::Default
                && c.flags.is_default_style()
                && !c.flags.wide()
                && !c.flags.wide_continuation()
                && c.hyperlink_id.is_none()
        });

        if all_default_attrs {
            let text: String = cells[..active_len].iter().map(|c| c.character).collect();
            ScrollbackLine {
                data: CompressedLineData::Plain(text, trailing_blanks as u16),
                is_wrapped,
                cols,
                reflow_content_len,
                reflow_wide_continuations,
                raw_row_id: RawRowId::UNTRACKED,
            }
        } else {
            let encoded = Self::encode_cells(&cells[..active_len]);
            ScrollbackLine {
                data: CompressedLineData::Encoded(encoded),
                is_wrapped,
                cols,
                reflow_content_len,
                reflow_wide_continuations,
                raw_row_id: RawRowId::UNTRACKED,
            }
        }
    }

    #[inline]
    pub fn reflow_content_len(&self) -> usize {
        self.reflow_content_len as usize
    }

    #[inline]
    pub fn reflow_wide_continuations(&self) -> &[u16] {
        &self.reflow_wide_continuations
    }

    #[inline]
    pub fn raw_row_id(&self) -> RawRowId {
        self.raw_row_id
    }

    #[inline]
    pub fn set_raw_row_id(&mut self, id: RawRowId) {
        self.raw_row_id = id;
    }

    pub fn decompress(&self) -> Vec<TerminalCell> {
        match &self.data {
            CompressedLineData::Plain(text, trailing) => {
                let mut cells: Vec<TerminalCell> = text
                    .chars()
                    .map(|ch| TerminalCell {
                        character: ch,
                        ..Default::default()
                    })
                    .collect();
                cells.resize(cells.len() + *trailing as usize, TerminalCell::default());
                cells
            }
            CompressedLineData::Encoded(data) => Self::decode_cells(data, self.cols as usize),
        }
    }

    /// Search-only admission cost. Reading it does not decode or scan text.
    pub fn search_encoded_bytes(&self) -> usize {
        match &self.data {
            CompressedLineData::Plain(text, _) => text.len(),
            CompressedLineData::Encoded(data) => data.len(),
        }
    }

    /// Whole-row decode with checked allocation/run limits. Existing render
    /// and copy decoders remain untouched. The caller reserves the entire
    /// row's cells and encoded bytes BEFORE invoking this method.
    pub fn decode_search_row(
        &self,
        max_cells: usize,
        max_encoded: usize,
    ) -> Option<Vec<TerminalCell>> {
        let cols = self.columns();
        if cols > super::MAX_TERMINAL_COLS
            || cols > max_cells
            || self.search_encoded_bytes() > max_encoded
        {
            return None;
        }
        let mut cells = Vec::with_capacity(cols);
        match &self.data {
            CompressedLineData::Plain(text, trailing) => {
                for character in text.chars() {
                    if cells.len() == cols {
                        return None;
                    }
                    cells.push(TerminalCell {
                        character,
                        ..Default::default()
                    });
                }
                if cells.len().checked_add(*trailing as usize)? != cols {
                    return None;
                }
            }
            CompressedLineData::Encoded(data) => {
                let mut pos = 0;
                while pos < data.len() {
                    let ch_len = *data.get(pos)? as usize;
                    pos += 1;
                    if !(1..=4).contains(&ch_len) {
                        return None;
                    }
                    let text =
                        std::str::from_utf8(data.get(pos..pos.checked_add(ch_len)?)?).ok()?;
                    let mut chars = text.chars();
                    let character = chars.next()?;
                    if chars.next().is_some() {
                        return None;
                    }
                    pos += ch_len;
                    let foreground = Self::decode_color(data, &mut pos);
                    let background = Self::decode_color(data, &mut pos);
                    let mut flags = Self::decode_flags(*data.get(pos)?);
                    pos += 1;
                    let wide = *data.get(pos)?;
                    pos += 1;
                    flags.set_wide(wide & 1 != 0);
                    flags.set_wide_continuation(wide & 2 != 0);
                    let hyperlink_id = HyperlinkId::from_raw(u16::from_le_bytes([
                        *data.get(pos)?,
                        *data.get(pos + 1)?,
                    ]));
                    pos += 2;
                    let run = (*data.get(pos)? as usize).max(1);
                    pos += 1;
                    if cells.len().checked_add(run)? > cols {
                        return None;
                    }
                    cells.resize(
                        cells.len() + run,
                        TerminalCell {
                            character,
                            foreground,
                            background,
                            flags,
                            hyperlink_id,
                        },
                    );
                }
            }
        }
        cells.resize(cols, TerminalCell::default());
        Some(cells)
    }

    /// 供搜索使用的行文本视图,避免对 Plain 行做 decompress → 重建字符串的
    /// 往返。返回(文本, 列映射, 可搜索列数):
    /// - Plain 行只含窄字符(压缩时已排除 wide/wide_continuation),直接借用
    ///   内部字符串,列映射为恒等(None)。
    /// - Encoded 行解码后与活动网格行走同一条 `searchable_line_text` 路径,
    ///   列映射跳过宽字符续接单元。
    pub fn search_text(&self) -> (std::borrow::Cow<'_, str>, Option<Vec<usize>>, usize) {
        match &self.data {
            CompressedLineData::Plain(text, _) => (
                std::borrow::Cow::Borrowed(text.as_str()),
                None,
                text.chars().count(),
            ),
            CompressedLineData::Encoded(_) => {
                let cells = self.decompress();
                let (text, col_map, total_cols) = searchable_line_text(&cells);
                (std::borrow::Cow::Owned(text), Some(col_map), total_cols)
            }
        }
    }

    fn encode_color(color: &Color, buf: &mut Vec<u8>) {
        match color {
            Color::Default => buf.push(0),
            Color::Black => buf.push(1),
            Color::Red => buf.push(2),
            Color::Green => buf.push(3),
            Color::Yellow => buf.push(4),
            Color::Blue => buf.push(5),
            Color::Magenta => buf.push(6),
            Color::Cyan => buf.push(7),
            Color::White => buf.push(8),
            Color::BrightBlack => buf.push(9),
            Color::BrightRed => buf.push(10),
            Color::BrightGreen => buf.push(11),
            Color::BrightYellow => buf.push(12),
            Color::BrightBlue => buf.push(13),
            Color::BrightMagenta => buf.push(14),
            Color::BrightCyan => buf.push(15),
            Color::BrightWhite => buf.push(16),
            Color::Indexed(i) => {
                buf.push(17);
                buf.push(*i);
            }
            Color::Rgb(r, g, b) => {
                buf.push(18);
                buf.push(*r);
                buf.push(*g);
                buf.push(*b);
            }
        }
    }

    fn decode_color(data: &[u8], pos: &mut usize) -> Color {
        if *pos >= data.len() {
            return Color::Default;
        }
        let tag = data[*pos];
        *pos += 1;
        match tag {
            0 => Color::Default,
            1 => Color::Black,
            2 => Color::Red,
            3 => Color::Green,
            4 => Color::Yellow,
            5 => Color::Blue,
            6 => Color::Magenta,
            7 => Color::Cyan,
            8 => Color::White,
            9 => Color::BrightBlack,
            10 => Color::BrightRed,
            11 => Color::BrightGreen,
            12 => Color::BrightYellow,
            13 => Color::BrightBlue,
            14 => Color::BrightMagenta,
            15 => Color::BrightCyan,
            16 => Color::BrightWhite,
            17 => {
                let i = data.get(*pos).copied().unwrap_or(0);
                *pos += 1;
                Color::Indexed(i)
            }
            18 => {
                let r = data.get(*pos).copied().unwrap_or(0);
                let g = data.get(*pos + 1).copied().unwrap_or(0);
                let b = data.get(*pos + 2).copied().unwrap_or(0);
                *pos += 3;
                Color::Rgb(r, g, b)
            }
            _ => Color::Default,
        }
    }

    fn encode_flags(flags: &StyleFlags) -> u8 {
        let mut f = 0u8;
        if flags.bold() {
            f |= 1;
        }
        if flags.italic() {
            f |= 2;
        }
        match flags.underline() {
            UnderlineStyle::None => {}
            UnderlineStyle::Single => f |= 4,
            UnderlineStyle::Double => f |= 8,
            UnderlineStyle::Curly => f |= 12,
            UnderlineStyle::Dotted => f |= 16,
            UnderlineStyle::Dashed => f |= 20,
        }
        if flags.inverse() {
            f |= 32;
        }
        if flags.dim() {
            f |= 64;
        }
        if flags.strikethrough() {
            f |= 128;
        }
        f
    }

    fn decode_flags(f: u8) -> StyleFlags {
        let underline = match (f >> 2) & 0x7 {
            0 => UnderlineStyle::None,
            1 => UnderlineStyle::Single,
            2 => UnderlineStyle::Double,
            3 => UnderlineStyle::Curly,
            4 => UnderlineStyle::Dotted,
            5 => UnderlineStyle::Dashed,
            _ => UnderlineStyle::None,
        };
        let mut flags = StyleFlags::new();
        flags.set_bold(f & 1 != 0);
        flags.set_italic(f & 2 != 0);
        flags.set_underline(underline);
        flags.set_inverse(f & 32 != 0);
        flags.set_dim(f & 64 != 0);
        flags.set_strikethrough(f & 128 != 0);
        flags
    }

    fn encode_cells(cells: &[TerminalCell]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(cells.len() * 3);
        let mut i = 0;
        while i < cells.len() {
            let cell = &cells[i];
            let ch_str = cell.character.to_string();
            let ch_bytes = ch_str.as_bytes();

            // RLE: count consecutive identical cells (packed flags comparison is a single u16 ==)
            let mut run = 1u8;
            while (run as u16) < 255 && (i + run as usize) < cells.len() {
                let next = &cells[i + run as usize];
                if next.character == cell.character
                    && next.foreground == cell.foreground
                    && next.background == cell.background
                    && next.flags == cell.flags
                    && next.hyperlink_id == cell.hyperlink_id
                {
                    run += 1;
                } else {
                    break;
                }
            }

            // Format: [char_len:1][char_bytes][fg][bg][flags:1]
            //         [wide_bits:1][hyperlink_id:2][run:1]
            buf.push(ch_bytes.len() as u8);
            buf.extend_from_slice(ch_bytes);
            Self::encode_color(&cell.foreground, &mut buf);
            Self::encode_color(&cell.background, &mut buf);
            let f = Self::encode_flags(&cell.flags);
            buf.push(f);
            let wide_bits = if cell.flags.wide() { 1u8 } else { 0 }
                | if cell.flags.wide_continuation() { 2 } else { 0 };
            buf.push(wide_bits);
            buf.extend_from_slice(&cell.hyperlink_id.as_raw().to_le_bytes());
            buf.push(run);

            i += run as usize;
        }
        buf
    }

    fn decode_cells(data: &[u8], cols: usize) -> Vec<TerminalCell> {
        let mut cells = Vec::with_capacity(cols);
        let mut pos = 0;
        while pos < data.len() {
            let ch_len = data[pos] as usize;
            pos += 1;
            if pos + ch_len > data.len() {
                break;
            }
            let ch = std::str::from_utf8(&data[pos..pos + ch_len])
                .ok()
                .and_then(|s| s.chars().next())
                .unwrap_or(' ');
            pos += ch_len;

            let fg = Self::decode_color(data, &mut pos);
            let bg = Self::decode_color(data, &mut pos);
            let f = data.get(pos).copied().unwrap_or(0);
            pos += 1;
            let wide_bits = data.get(pos).copied().unwrap_or(0);
            pos += 1;
            let hyperlink_id = HyperlinkId::from_raw(u16::from_le_bytes([
                data.get(pos).copied().unwrap_or(0),
                data.get(pos + 1).copied().unwrap_or(0),
            ]));
            pos += 2;
            let run = data.get(pos).copied().unwrap_or(1).max(1);
            pos += 1;

            let mut flags = Self::decode_flags(f);
            flags.set_wide(wide_bits & 1 != 0);
            flags.set_wide_continuation(wide_bits & 2 != 0);

            let cell = TerminalCell {
                character: ch,
                foreground: fg,
                background: bg,
                flags,
                hyperlink_id,
            };
            for _ in 0..run {
                cells.push(cell);
            }
        }
        // Pad to cols
        cells.resize(cols, TerminalCell::default());
        cells
    }
}

/// 将网格行转换为字符串,并返回每个字符对应的网格列号。
/// 跳过宽字符的续接单元(否则相邻宽字符间会被插入空格导致匹配失败),
/// 因此字符索引与字节偏移都不再等于列号,需经 col_map 映射。
pub fn searchable_line_text(line: &[TerminalCell]) -> (String, Vec<usize>, usize) {
    let mut searchable_len = line.len();
    while searchable_len > 0 {
        let cell = &line[searchable_len - 1];
        if cell.character == ' '
            && cell.background == Color::Default
            && !cell.flags.wide()
            && !cell.flags.wide_continuation()
        {
            searchable_len -= 1;
        } else {
            break;
        }
    }
    let line = &line[..searchable_len];
    let mut s = String::with_capacity(searchable_len);
    let mut col_map = Vec::with_capacity(searchable_len);
    for (col, cell) in line.iter().enumerate() {
        if cell.flags.wide_continuation() {
            continue;
        }
        s.push(cell.character);
        col_map.push(col);
    }
    (s, col_map, searchable_len)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalCell {
    pub character: char,
    pub foreground: Color,
    pub background: Color,
    pub flags: StyleFlags,
    pub hyperlink_id: HyperlinkId,
}

impl Default for TerminalCell {
    fn default() -> Self {
        TerminalCell {
            character: ' ',
            foreground: Color::Default,
            background: Color::Default,
            flags: StyleFlags::new(),
            hyperlink_id: HyperlinkId::NONE,
        }
    }
}

impl TerminalCell {
    /// Match the historical reflow rule exactly.  Foreground-only styling on
    /// a space has never kept that space in a joined logical line; changing
    /// that here would move existing search/selection coordinates.
    #[inline]
    pub fn is_reflow_trimmable_blank(&self) -> bool {
        self.character == ' '
            && self.background == Color::Default
            && !self.flags.wide()
            && !self.flags.wide_continuation()
            && self.hyperlink_id.is_none()
    }
}

const _: () = assert!(std::mem::size_of::<TerminalCell>() == 16);

/// 追踪改变的行和列区间（脏矩形）
#[derive(Clone, Debug)]
pub struct DirtyRegion {
    pub rows: Vec<(usize, usize)>, // (row_start, row_end)，包含端点
}

impl Default for DirtyRegion {
    fn default() -> Self {
        Self::new()
    }
}

impl DirtyRegion {
    pub fn new() -> Self {
        DirtyRegion { rows: Vec::new() }
    }

    /// 标记某一行为脏
    pub fn mark_row(&mut self, row: usize) {
        if let Some(last) = self.rows.last_mut() {
            if row > 0 && last.1 == row - 1 {
                // 合并相邻的行
                last.1 = row;
                return;
            }
        }
        self.rows.push((row, row));
    }

    /// 标记行范围为脏
    pub fn mark_rows(&mut self, start: usize, end: usize) {
        for row in start..=end {
            self.mark_row(row);
        }
    }

    /// 标记整个网格为脏
    pub fn mark_all(&mut self, rows: usize) {
        self.rows.clear();
        self.rows.push((0, rows.saturating_sub(1)));
    }

    /// 清除脏标记
    pub fn clear(&mut self) {
        self.rows.clear();
    }
}

#[cfg(test)]
mod search_chunk_decode_tests {
    use super::*;
    #[test]
    fn search_chunk_decode_admission_and_rle_expansion_are_bounded() {
        let cells = vec![
            TerminalCell {
                character: 'x',
                foreground: Color::Red,
                ..Default::default()
            };
            1024
        ];
        let mut line = ScrollbackLine::compress(&cells, false);
        let bytes = line.search_encoded_bytes();
        assert!(line.decode_search_row(1023, bytes).is_none());
        assert!(line.decode_search_row(1024, bytes - 1).is_none());
        assert_eq!(line.decode_search_row(1024, bytes).unwrap().len(), 1024);
        // Corrupted compressed input cannot allocate a run beyond cols before
        // its final resize, unlike the general-purpose historical decoder.
        if let CompressedLineData::Encoded(data) = &mut line.data {
            *data.last_mut().unwrap() = 255;
        } else {
            panic!("encoded fixture");
        }
        assert!(line.decode_search_row(1024, bytes).is_none());
    }
    #[test]
    fn search_chunk_plain_padding_has_whole_row_cell_cost() {
        let cells = vec![TerminalCell::default(); 1024];
        let line = ScrollbackLine::compress(&cells, false);
        assert_eq!(line.columns(), 1024);
        assert!(line.decode_search_row(1000, 4096).is_none());
        assert_eq!(line.decode_search_row(1024, 4096).unwrap().len(), 1024);
    }
}
