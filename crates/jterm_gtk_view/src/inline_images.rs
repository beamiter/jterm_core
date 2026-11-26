use gtk4::{gdk, glib, graphene, prelude::*, subclass::prelude::*};
use jterm_engine::{
    frame::{CellFrame, Screen},
    retained_bytes::{RetainedBytes, RetainedBytesBudget},
    terminal::{
        HistoryPage, InlineImageId, InlineImageRow, PlacedInlineImage, PreparedInlineImage,
        Terminal,
    },
};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, OnceLock},
};

const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;
const PROCESS_IMAGE_BYTES: usize = 8 * MAX_RETAINED_BYTES;
const MAX_IMAGES: usize = 64;
const IMAGE_BOOKKEEPING_BYTES: usize = 64 * 1024;

/// Static primary-screen image geometry. Height is authoritative and a
/// missing width uses the texture's aspect ratio at the actual cell size.
pub struct InlineImageRequest {
    pub columns: Option<u32>,
    pub rows: u32,
    pub move_cursor: bool,
    pub cursor: (i64, i64),
}

/// Texture and engine reservation checked before a protocol reply is queued.
pub struct PendingInlineImage {
    owner: glib::WeakRef<Canvas>,
    reservation: PreparedInlineImage,
    texture: gdk::Texture,
    columns: u16,
    rows: u16,
    charge: usize,
    lease: RetainedBytes,
}

struct Image {
    source: PlacedInlineImage,
    texture: gdk::Texture,
    columns: u16,
    rows: u16,
    charge: usize,
    _lease: RetainedBytes,
}

#[derive(Clone)]
struct PaintSlice {
    image: Rc<Image>,
    bounds: graphene::Rect,
    strip: graphene::Rect,
}

impl PartialEq for PaintSlice {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.image, &other.image)
            && self.bounds == other.bounds
            && self.strip == other.strip
    }
}

fn process_image_budget() -> &'static Arc<RetainedBytesBudget> {
    static PROCESS: OnceLock<Arc<RetainedBytesBudget>> = OnceLock::new();
    PROCESS.get_or_init(|| Arc::new(RetainedBytesBudget::new(PROCESS_IMAGE_BYTES)))
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Canvas {
        pub(super) slices: RefCell<Vec<PaintSlice>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Canvas {
        const NAME: &'static str = "JtermInlineImageCanvas";
        type Type = super::Canvas;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for Canvas {}

    impl WidgetImpl for Canvas {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            for slice in self.slices.borrow().iter() {
                snapshot.push_clip(&slice.strip);
                snapshot.append_texture(&slice.image.texture, &slice.bounds);
                snapshot.pop();
            }
        }
    }
}

glib::wrapper! {
    pub struct Canvas(ObjectSubclass<imp::Canvas>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

pub(crate) struct InlineImages {
    canvas: Canvas,
    retained: HashMap<InlineImageId, Rc<Image>>,
    retained_bytes: usize,
    archived: Vec<(usize, usize, InlineImageRow)>,
    process: Arc<RetainedBytesBudget>,
}

impl InlineImages {
    pub(crate) fn attach(viewport: &gtk4::Overlay) -> Self {
        Self::with_budget(viewport, Arc::clone(process_image_budget()))
    }

    fn with_budget(viewport: &gtk4::Overlay, process: Arc<RetainedBytesBudget>) -> Self {
        let canvas = glib::Object::builder::<Canvas>()
            .property("can-target", false)
            .property("focusable", false)
            .property("hexpand", true)
            .property("vexpand", true)
            .property("visible", false)
            .build();
        canvas.update_state(&[gtk4::accessible::State::Hidden(true)]);
        viewport.add_overlay(&canvas);
        viewport.set_measure_overlay(&canvas, false);
        viewport.set_clip_overlay(&canvas, true);
        Self {
            canvas,
            retained: HashMap::new(),
            retained_bytes: 0,
            archived: Vec::new(),
            process,
        }
    }

    #[cfg(test)]
    pub(crate) fn painted_slices(&self) -> usize {
        self.canvas.imp().slices.borrow().len()
    }

    pub(crate) fn overlaps(&self, screen: &gtk4::TextView, rect: graphene::Rect) -> bool {
        let slices = self.canvas.imp().slices.borrow();
        if slices.is_empty() {
            return false;
        }
        let Some(origin) = self.canvas.parent().and_then(|viewport| {
            screen.compute_point(&viewport, &graphene::Point::new(rect.x(), rect.y()))
        }) else {
            return true;
        };
        let footprint = graphene::Rect::new(origin.x(), origin.y(), rect.width(), rect.height());
        slices
            .iter()
            .any(|slice| slice.strip.intersection(&footprint).is_some())
    }

    pub(crate) fn prepare(
        &self,
        terminal: &Terminal,
        texture: gdk::Texture,
        encoded_source_bytes: usize,
        request: InlineImageRequest,
        cell: (i32, i32),
    ) -> Option<PendingInlineImage> {
        let pixels = (usize::try_from(texture.width()).ok()?)
            .checked_mul(usize::try_from(texture.height()).ok()?)?
            .checked_mul(4)?;
        let charge = pixels
            .checked_add(encoded_source_bytes)?
            .checked_add(IMAGE_BOOKKEEPING_BYTES)?;
        if self.retained.len() >= MAX_IMAGES
            || self.retained_bytes.checked_add(charge)? > MAX_RETAINED_BYTES
        {
            return None;
        }

        let rows = u16::try_from(request.rows).ok()?;
        let columns = request
            .columns
            .filter(|&width| width != 0)
            .or_else(|| infer_columns(rows, cell, (texture.width(), texture.height())))?;
        let columns = u16::try_from(columns).ok()?;
        let cursor = (
            usize::try_from(request.cursor.0).ok()?,
            usize::try_from(request.cursor.1).ok()?,
        );
        let reservation =
            terminal.prepare_inline_image(cursor, columns, rows, request.move_cursor)?;
        let lease = self.process.reserve(charge)?;
        Some(PendingInlineImage {
            owner: self.canvas.downgrade(),
            reservation,
            texture,
            columns,
            rows,
            charge,
            lease,
        })
    }

    pub(crate) fn admit(
        &mut self,
        prepared: PendingInlineImage,
        place: impl FnOnce(PreparedInlineImage) -> Option<PlacedInlineImage>,
    ) -> bool {
        if prepared.owner.upgrade().as_ref() != Some(&self.canvas) {
            return false;
        }
        let Some(source) = place(prepared.reservation) else {
            return false;
        };
        self.retained_bytes += prepared.charge;
        self.retained.insert(
            source.id(),
            Rc::new(Image {
                source,
                texture: prepared.texture,
                columns: prepared.columns,
                rows: prepared.rows,
                charge: prepared.charge,
                _lease: prepared.lease,
            }),
        );
        true
    }

    pub(crate) fn missing_backing(&self, frame: &CellFrame) -> bool {
        (0..frame.rows().len()).any(|row| {
            frame
                .inline_image_rows(row)
                .any(|(_, image)| !self.retained.contains_key(&image.image))
        })
    }

    pub(crate) fn clear(&mut self) {
        self.retained.clear();
        self.retained_bytes = 0;
        self.archived.clear();
        self.canvas.imp().slices.borrow_mut().clear();
        self.canvas.set_visible(false);
    }

    /// Share already admitted backing; no image reservation or raster copy.
    pub(crate) fn share_from(&mut self, source: &Self) {
        self.retained.clone_from(&source.retained);
        self.retained_bytes = source.retained_bytes;
    }

    pub(crate) fn present(
        &mut self,
        frame: &CellFrame,
        history: Option<(usize, Option<&HistoryPage>)>,
        metrics: impl FnOnce() -> (i32, i32, i32, i32),
    ) {
        if self.retained.is_empty() {
            return;
        }
        // Leave the old textures charged through a synchronized update or
        // selection hold. This method runs only when the matching frame paints.
        let mut removed_bytes = 0;
        self.retained.retain(|_, image| {
            let keep = image.source.is_retained();
            if !keep {
                removed_bytes += image.charge;
            }
            keep
        });
        self.retained_bytes -= removed_bytes;

        let mut slices = Vec::new();
        if frame.stamp().screen == Screen::Primary {
            let metrics = metrics();
            if let Some((_, Some(page))) = history {
                self.archived = (0..page.row_count)
                    .flat_map(|row| {
                        page.inline_image_rows(row)
                            .map(move |(column, image)| (row, column, image))
                    })
                    .collect();
            } else if history.is_none() {
                self.archived.clear();
            }
            let archived_rows = history.map_or(0, |(rows, _)| rows);
            let live_rows = frame.rows().len().saturating_sub(archived_rows);
            let visible = self
                .archived
                .iter()
                .copied()
                .chain((0..live_rows).flat_map(|row| {
                    frame
                        .inline_image_rows(row)
                        .map(move |(column, image)| (archived_rows + row, column, image))
                }));
            slices.extend(visible.filter_map(|(row, column, source)| {
                self.retained.get(&source.image)?.slice(
                    row,
                    column,
                    source.slice,
                    frame.cols(),
                    metrics,
                )
            }));
        }
        let visible = !slices.is_empty();
        let mut painted = self.canvas.imp().slices.borrow_mut();
        if *painted != slices {
            *painted = slices;
            drop(painted);
            self.canvas.set_visible(visible);
            self.canvas.queue_draw();
        }
    }
}

impl Image {
    fn slice(
        self: &Rc<Self>,
        row: usize,
        column: usize,
        slice: u16,
        viewport_columns: usize,
        (left, top, cell_width, cell_height): (i32, i32, i32, i32),
    ) -> Option<PaintSlice> {
        if slice >= self.rows
            || column.checked_add(usize::from(self.columns))? > viewport_columns
            || cell_width <= 0
            || cell_height <= 0
        {
            return None;
        }
        let x = (left + column as i32 * cell_width) as f32;
        let y = (top + row as i32 * cell_height) as f32;
        let width = (i32::from(self.columns) * cell_width) as f32;
        let height = (i32::from(self.rows) * cell_height) as f32;
        Some(PaintSlice {
            image: Rc::clone(self),
            bounds: graphene::Rect::new(
                x,
                y - f32::from(slice) * cell_height as f32,
                width,
                height,
            ),
            strip: graphene::Rect::new(x, y, width, cell_height as f32),
        })
    }
}

fn infer_columns(
    rows: u16,
    (cell_width, cell_height): (i32, i32),
    (image_width, image_height): (i32, i32),
) -> Option<u32> {
    let numerator = u128::from(rows)
        .checked_mul(u128::try_from(cell_height).ok()?)?
        .checked_mul(u128::try_from(image_width).ok()?)?;
    let denominator = u128::try_from(image_height)
        .ok()?
        .checked_mul(u128::try_from(cell_width).ok()?)?;
    if numerator == 0 || denominator == 0 {
        return None;
    }
    u32::try_from(numerator.div_ceil(denominator)).ok()
}

#[cfg(test)]
mod observer_tests {
    use super::*;
    #[test]
    #[ignore = "requires a GTK display and a fresh process"]
    fn observer_shares_admitted_image_backing_and_releases_it_on_detach() {
        gtk4::init().unwrap();
        let mut terminal = Terminal::new(4, 8).unwrap();
        let charge = 16 + IMAGE_BOOKKEEPING_BYTES;
        let process = Arc::new(RetainedBytesBudget::new(charge));
        let mut owner = InlineImages::with_budget(&gtk4::Overlay::new(), Arc::clone(&process));
        let peer = InlineImages::with_budget(&gtk4::Overlay::new(), Arc::clone(&process));
        let texture: gdk::Texture = gdk::MemoryTexture::new(
            2,
            2,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_static(&[255; 16]),
            8,
        )
        .upcast();
        let request = || InlineImageRequest {
            columns: Some(3),
            rows: 2,
            move_cursor: false,
            cursor: (0, 0),
        };
        let prepared = owner
            .prepare(&terminal, texture.clone(), 0, request(), (8, 16))
            .unwrap();
        assert_eq!(process.usage().retained_bytes, charge);
        assert!(peer
            .prepare(&terminal, texture.clone(), 0, request(), (8, 16))
            .is_none());
        assert!(owner.admit(prepared, |r| terminal.place_inline_image(r)));
        let frame = terminal.frame().unwrap();
        let mut observer = InlineImages::with_budget(&gtk4::Overlay::new(), Arc::clone(&process));
        assert!(observer.missing_backing(&frame));
        observer.share_from(&owner);
        assert!(!observer.missing_backing(&frame));
        assert!(Rc::ptr_eq(
            owner.retained.values().next().unwrap(),
            observer.retained.values().next().unwrap()
        ));
        owner.clear();
        assert_eq!(process.usage().retained_bytes, charge);
        drop(terminal);
        observer.present(&frame, None, || (0, 0, 8, 16));
        assert_eq!(observer.painted_slices(), 2);
        let painted = observer.canvas.imp().slices.borrow()[0].clone();
        observer.clear();
        assert_eq!(observer.painted_slices(), 0);
        assert!(observer.retained.is_empty());
        assert_eq!(process.usage().retained_bytes, charge);
        drop(painted);
        assert_eq!(process.usage().retained_bytes, 0);
        let terminal = Terminal::new(4, 8).unwrap();
        let admitted_after_clear = peer
            .prepare(&terminal, texture, 0, request(), (8, 16))
            .unwrap();
        assert_eq!(process.usage().retained_bytes, charge);
        drop(admitted_after_clear);
        assert_eq!(process.usage().retained_bytes, 0);
    }
}
