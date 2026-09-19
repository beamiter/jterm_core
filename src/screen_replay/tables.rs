//! Interning tables shared by every cell: attribute pens, OSC 8 hyperlinks
//! and combining-character clusters.
//!
//! A cell stores 32-bit indices into these tables instead of the values, which
//! keeps a cell at 8 bytes. The tables only grow when the stream selects
//! something new, but a hostile or merely unusual stream (a truecolour image
//! dump, one auto-id hyperlink per redraw) can select something new millions
//! of times, so they are compacted — rebuilt from what the grid still
//! references — whenever one doubles past its last live size. That bounds
//! them by the cell budget instead of by the input length.

use super::grid::Row;
use super::pen::Pen;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};

/// Tables start compacting once they hold this many entries.
const COMPACT_FLOOR: usize = 4096;

/// A small multiplicative hasher for the hot interning maps. SipHash's DoS
/// resistance buys nothing here (the maps are bounded by compaction), and a
/// pen lookup happens on every SGR change that reaches the grid.
#[derive(Default)]
pub(super) struct FxHasher(u64);

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }

    fn write_u8(&mut self, n: u8) {
        self.write_u64(u64::from(n));
    }

    fn write_u16(&mut self, n: u16) {
        self.write_u64(u64::from(n));
    }

    fn write_u32(&mut self, n: u32) {
        self.write_u64(u64::from(n));
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type FxMap<K> = HashMap<K, u32, BuildHasherDefault<FxHasher>>;

/// One interning table: values by index plus the reverse lookup.
struct Interner<T> {
    values: Vec<T>,
    ids: FxMap<T>,
    /// Compact when `values` grows past this.
    limit: usize,
}

impl<T: Clone + Eq + std::hash::Hash> Interner<T> {
    fn with_first(first: T) -> Self {
        let mut ids = FxMap::default();
        ids.insert(first.clone(), 0);
        Interner {
            values: vec![first],
            ids,
            limit: COMPACT_FLOOR,
        }
    }

    fn intern(&mut self, value: &T) -> u32 {
        if let Some(&id) = self.ids.get(value) {
            return id;
        }
        let id = self.values.len() as u32;
        self.values.push(value.clone());
        self.ids.insert(value.clone(), id);
        id
    }

    fn over_limit(&self) -> bool {
        self.values.len() > self.limit
    }

    /// Keeps the entries marked in `live` (entry 0 always survives) and
    /// returns the old→new index map.
    fn retain(&mut self, live: &[bool]) -> Vec<u32> {
        let mut remap = vec![0u32; self.values.len()];
        let mut kept = Vec::with_capacity(live.iter().filter(|&&l| l).count() + 1);
        self.ids.clear();
        for (old, value) in std::mem::take(&mut self.values).into_iter().enumerate() {
            if old == 0 || live[old] {
                remap[old] = kept.len() as u32;
                self.ids.insert(value.clone(), kept.len() as u32);
                kept.push(value);
            }
        }
        self.values = kept;
        self.limit = COMPACT_FLOOR.max(self.values.len() * 2);
        remap
    }
}

/// Pens, hyperlink targets and clusters, indexed by what cells store.
pub(super) struct Tables {
    pens: Interner<Pen>,
    /// Entry 0 is "no link"; the others hold the OSC 8 parameters exactly as
    /// `to_ansi` writes them back (`id=…;uri` or `;uri`).
    links: Interner<String>,
    clusters: Interner<String>,
}

impl Default for Tables {
    fn default() -> Self {
        Tables {
            pens: Interner::with_first(Pen::default()),
            links: Interner::with_first(String::new()),
            clusters: Interner::with_first(String::new()),
        }
    }
}

/// What a compaction pass must be told about: every place outside the grid
/// that holds a pen (with its link index).
pub(super) struct CompactRoots<'a> {
    pub(super) pens: Vec<&'a mut Pen>,
}

impl Tables {
    pub(super) fn pen(&self, id: u32) -> &Pen {
        &self.pens.values[id as usize]
    }

    pub(super) fn intern_pen(&mut self, pen: &Pen) -> u32 {
        self.pens.intern(pen)
    }

    pub(super) fn link(&self, id: u32) -> &str {
        &self.links.values[id as usize]
    }

    pub(super) fn intern_link(&mut self, link: String) -> u32 {
        self.links.intern(&link)
    }

    pub(super) fn cluster(&self, id: u32) -> &str {
        &self.clusters.values[id as usize]
    }

    pub(super) fn intern_cluster(&mut self, text: String) -> u32 {
        self.clusters.intern(&text)
    }

    /// Whether any table has doubled past its last live size.
    pub(super) fn needs_compaction(&self) -> bool {
        self.pens.over_limit() || self.links.over_limit() || self.clusters.over_limit()
    }

    /// Rebuilds all three tables from what `grids` and `roots` still use and
    /// rewrites every reference. Callers must drop cached pen ids afterwards.
    pub(super) fn compact(&mut self, grids: &mut [&mut VecDeque<Row>], roots: CompactRoots<'_>) {
        use super::grid::{CLUSTER, FRAGMENT, VALUE_MASK};

        let mut live_pens = vec![false; self.pens.values.len()];
        let mut live_clusters = vec![false; self.clusters.values.len()];
        for row in grids.iter().flat_map(|grid| grid.iter()) {
            for cell in &row.cells {
                live_pens[cell.pen as usize] = true;
                if cell.code & (CLUSTER | FRAGMENT) == CLUSTER {
                    live_clusters[(cell.code & VALUE_MASK) as usize] = true;
                }
            }
        }
        let mut live_links = vec![false; self.links.values.len()];
        for (id, pen) in self.pens.values.iter().enumerate() {
            if live_pens[id] {
                live_links[pen.link as usize] = true;
            }
        }
        for pen in &roots.pens {
            live_links[pen.link as usize] = true;
        }

        let link_remap = self.links.retain(&live_links);
        for pen in &mut self.pens.values {
            pen.link = link_remap[pen.link as usize];
        }
        for pen in roots.pens {
            pen.link = link_remap[pen.link as usize];
        }
        // Remapping links can make two pens equal; `retain` re-inserts every
        // kept pen, and the first index wins in the reverse map, which is fine
        // because cells are rewritten through `pen_remap` below.
        let pen_remap = self.pens.retain(&live_pens);
        let cluster_remap = self.clusters.retain(&live_clusters);
        for row in grids.iter_mut().flat_map(|grid| grid.iter_mut()) {
            for cell in &mut row.cells {
                cell.pen = pen_remap[cell.pen as usize];
                if cell.code & (CLUSTER | FRAGMENT) == CLUSTER {
                    let old = (cell.code & VALUE_MASK) as usize;
                    cell.code = (cell.code & !VALUE_MASK) | cluster_remap[old];
                }
            }
        }
    }
}
