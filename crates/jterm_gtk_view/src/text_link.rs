use gtk4::{gdk, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use crate::view::PresentedLink;

struct Press {
    buffer: gtk4::TextBuffer,
    revision: u64,
    target: PresentedLink,
    point: (f64, f64),
}

pub(crate) fn link_modifier(modifiers: gdk::ModifierType) -> bool {
    modifiers.contains(gdk::ModifierType::CONTROL_MASK)
        && !modifiers.intersects(
            gdk::ModifierType::SHIFT_MASK
                | gdk::ModifierType::ALT_MASK
                | gdk::ModifierType::META_MASK
                | gdk::ModifierType::SUPER_MASK
                | gdk::ModifierType::HYPER_MASK,
        )
}

pub(crate) fn connect_links(
    view: &gtk4::TextView,
    resolve: impl Fn(f64, f64) -> Option<PresentedLink> + 'static,
    open: impl Fn(&str) + 'static,
) {
    let gesture = gtk4::GestureClick::builder()
        .button(gdk::BUTTON_PRIMARY)
        .propagation_phase(gtk4::PropagationPhase::Capture)
        .build();
    let pressed = Rc::new(RefCell::new(None));
    let revision = Rc::new(Cell::new(0u64));
    let observed = revision.clone();
    let observe_buffer = move |view: &gtk4::TextView| {
        observed.set(observed.get().wrapping_add(1));
        let changed = observed.clone();
        view.buffer()
            .connect_changed(move |_| changed.set(changed.get().wrapping_add(1)));
    };
    observe_buffer(view);
    view.connect_buffer_notify(observe_buffer);
    let resolve = Rc::new(resolve);
    let weak = view.downgrade();
    let for_press = Rc::clone(&pressed);
    let resolve_press = resolve.clone();
    let revision_press = revision.clone();
    gesture.connect_pressed(move |gesture, count, x, y| {
        for_press.borrow_mut().take();
        if count != 1 || !link_modifier(gesture.current_event_state()) {
            return;
        }
        let Some(view) = weak.upgrade() else { return };
        if let Some(target) = resolve_press(x, y) {
            *for_press.borrow_mut() = Some(Press {
                buffer: view.buffer(),
                revision: revision_press.get(),
                target,
                point: (x, y),
            });
            gesture.set_state(gtk4::EventSequenceState::Claimed);
        }
    });
    let for_stop = Rc::clone(&pressed);
    gesture.connect_stopped(move |_| {
        for_stop.borrow_mut().take();
    });
    let weak = view.downgrade();
    gesture.connect_released(move |gesture, count, x, y| {
        let Some(press) = pressed.borrow_mut().take() else {
            return;
        };
        let Some(view) = weak.upgrade() else { return };
        if count == 1
            && view.is_mapped()
            && link_modifier(gesture.current_event_state())
            && (x - press.point.0).abs() <= 4.0
            && (y - press.point.1).abs() <= 4.0
            && press.buffer == view.buffer()
            && press.revision == revision.get()
            && resolve(x, y).as_ref() == Some(&press.target)
        {
            open(&press.target.target);
        }
    });
    view.add_controller(gesture);
}
