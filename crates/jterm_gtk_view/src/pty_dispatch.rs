use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, TryRecvError},
};

pub enum PtyEvent {
    Data(Vec<u8>),
    Exit(i32),
}

/// Main-context receiver for ordered PTY output and the final exit event.
/// The host retains the reader, the child, and the GLib scheduling source.
pub struct PtyDispatch<F, E> {
    receiver: Receiver<PtyEvent>,
    output: F,
    exited: Option<E>,
}

impl<F: FnMut(Vec<u8>), E: FnOnce(i32)> PtyDispatch<F, E> {
    pub fn new(receiver: Receiver<PtyEvent>, output: F, exited: E) -> Self {
        Self {
            receiver,
            output,
            exited: Some(exited),
        }
    }

    pub fn on_poll(&mut self) -> bool {
        let event = self.receiver.try_recv();
        self.deliver(event, || {})
    }

    /// Consume one ready event at the existing GLib source's priority. The
    /// second receive closes the race with a producer that published just
    /// before the consumer released its coalesced wake flag.
    pub fn on_wake(
        &mut self,
        pending: &AtomicBool,
        acknowledge: impl Fn(),
        rearm: impl FnOnce(),
    ) -> bool {
        acknowledge();
        let event = match self.receiver.try_recv() {
            Err(TryRecvError::Empty) => {
                pending.store(false, Ordering::Release);
                let event = self.receiver.try_recv();
                if event.is_ok() {
                    pending.store(true, Ordering::Release);
                    acknowledge();
                }
                event
            }
            event => event,
        };
        self.deliver(event, rearm)
    }

    fn deliver(
        &mut self,
        event: Result<PtyEvent, TryRecvError>,
        after_output: impl FnOnce(),
    ) -> bool {
        if self.exited.is_none() {
            return false;
        }
        match event {
            Ok(PtyEvent::Data(bytes)) => {
                (self.output)(bytes);
                after_output();
                true
            }
            Ok(PtyEvent::Exit(code)) => {
                if let Some(exited) = self.exited.take() {
                    exited(code);
                }
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, sync::mpsc::sync_channel};

    #[test]
    fn polling_delivers_one_batch_per_dispatch_then_exit_once() {
        let (tx, rx) = sync_channel(3);
        tx.send(PtyEvent::Data(b"first".to_vec())).unwrap();
        tx.send(PtyEvent::Data(b"last".to_vec())).unwrap();
        tx.send(PtyEvent::Exit(7)).unwrap();
        let events = RefCell::new(Vec::new());
        let mut dispatch = PtyDispatch::new(
            rx,
            |data| events.borrow_mut().push(data),
            |code| {
                events.borrow_mut().push(vec![code as u8]);
            },
        );
        assert!(dispatch.on_poll());
        assert_eq!(*events.borrow(), [b"first".to_vec()]);
        assert!(dispatch.on_poll());
        assert!(!dispatch.on_poll());
        assert!(!dispatch.on_poll());
        assert_eq!(
            *events.borrow(),
            [b"first".to_vec(), b"last".to_vec(), vec![7]]
        );
    }

    #[test]
    fn coalesced_wake_rearms_after_data_and_releases_on_empty_or_disconnect() {
        use std::cell::Cell;
        let (tx, rx) = sync_channel(1);
        let pending = AtomicBool::new(true);
        let acknowledgements = Cell::new(0);
        let rearmed = Cell::new(0);
        let mut dispatch =
            PtyDispatch::new(rx, |data| assert_eq!(data, b"tail"), |_| panic!("no exit"));
        let acknowledge = || acknowledgements.set(acknowledgements.get() + 1);
        let rearm = || rearmed.set(rearmed.get() + 1);
        tx.send(PtyEvent::Data(b"tail".to_vec())).unwrap();
        assert!(dispatch.on_wake(&pending, acknowledge, rearm));
        assert!(pending.load(Ordering::Acquire));
        assert_eq!(rearmed.get(), 1);
        assert!(dispatch.on_wake(&pending, acknowledge, rearm));
        assert!(!pending.load(Ordering::Acquire));
        drop(tx);
        assert!(!dispatch.on_wake(&pending, acknowledge, rearm));
        assert_eq!(rearmed.get(), 1);
        assert_eq!(acknowledgements.get(), 3);
    }
}
