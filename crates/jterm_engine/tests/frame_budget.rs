//! A receiver's tightened frame budget must reject before allocating a copy or
//! scratch table. Expectations are allocation counts, not a reference encoder.
use jterm_engine::{
    frame::*,
    grid::{CursorShape, TerminalCell},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

struct CountingAllocator;
thread_local! {
    static COUNT: Cell<Option<usize>> = const { Cell::new(None) };
}
fn allocated() {
    let _ = COUNT.try_with(|count| {
        if let Some(n) = count.get() {
            count.set(Some(n + 1));
        }
    });
}
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocated();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocated();
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn measured<T>(f: impl FnOnce() -> T) -> (T, usize) {
    COUNT.with(|count| count.set(Some(0)));
    let result = f();
    let count = COUNT.with(|count| count.replace(None).unwrap());
    (result, count)
}
const LIMITS: Limits = Limits {
    max_rows: 3,
    max_cols: 4,
    max_cells: 12,
};
fn base() -> CellFrame {
    CellFrame::new(
        Stamp {
            epoch: 1,
            revision: 1,
            layout: 1,
            screen: Screen::Primary,
        },
        4,
        vec![
            Row {
                cells: vec![TerminalCell::default(); 4].into(),
                wrapped: false
            };
            3
        ]
        .into(),
        Cursor {
            row: 0,
            col: 0,
            pending_wrap: false,
            visible: true,
            shape: CursorShape::Block,
        },
        LIMITS,
    )
    .unwrap()
}

#[test]
fn tightened_budgets_reject_without_allocating_or_advancing_receiver() {
    let base = base();
    for rows in [vec![], vec![(1, base.rows()[1].clone())]] {
        let delta = Delta {
            base: base.stamp(),
            next: Stamp {
                revision: 2,
                ..base.stamp()
            },
            cursor: Cursor {
                col: 3,
                pending_wrap: true,
                ..base.cursor()
            },
            rows,
        };
        // Positive control: the allocation observer sees a successful copy.
        let (accepted, allocations) = measured(|| base.apply(&delta, LIMITS));
        assert!(allocations > 0);
        assert_eq!(accepted.unwrap().stamp(), delta.next);
        for limits in [
            Limits {
                max_rows: 2,
                ..LIMITS
            },
            Limits {
                max_cols: 3,
                ..LIMITS
            },
            Limits {
                max_cells: 11,
                ..LIMITS
            },
            Limits {
                max_rows: 0,
                max_cols: 0,
                max_cells: 0,
            },
        ] {
            let (result, allocations) = measured(|| base.apply(&delta, limits));
            assert_eq!(result, Err(Error::Bounds));
            assert_eq!(allocations, 0, "over-budget apply must not allocate");
            let mut receiver = Receiver::new(1);
            receiver.full(base.clone()).unwrap();
            let (result, allocations) = measured(|| receiver.delta(&delta, limits));
            assert_eq!(result, Err(Error::Bounds));
            assert_eq!(allocations, 0, "over-budget receiver must not allocate");
            assert_eq!(receiver.frame(), Some(&base));
            // The same revision is still admissible once the budget permits it.
            receiver.delta(&delta, LIMITS).unwrap();
            let next = receiver.frame().unwrap();
            assert_eq!(next.stamp(), delta.next);
            assert_eq!(next.cursor(), delta.cursor);
            assert_eq!(next.rows(), base.rows());
        }
    }
}
