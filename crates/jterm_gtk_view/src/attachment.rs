use jterm_protocol::terminal::Attachment;
use std::sync::atomic::{AtomicU64, Ordering};

/// One writable view generation, independent of the retained actor lifetime.
pub(crate) struct ViewAttachment {
    ticket: Attachment,
    attached: bool,
}

impl ViewAttachment {
    pub(crate) fn new(epoch: u32) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            ticket: Attachment {
                session: format!("gtk-runtime-{}-{id}", std::process::id()),
                epoch,
                generation: 1,
            },
            attached: true,
        }
    }

    pub(crate) fn ticket(&self) -> Attachment {
        self.ticket.clone()
    }
    pub(crate) fn attached(&self) -> bool {
        self.attached
    }
    pub(crate) fn admits(&self, ticket: &Attachment) -> bool {
        self.attached && &self.ticket == ticket
    }
    pub(crate) fn switch(
        &mut self,
        expected: &Attachment,
        attach: bool,
    ) -> Result<Attachment, String> {
        if expected != &self.ticket || self.attached == attach {
            return Err("stale terminal view attachment".into());
        }
        let next = self
            .ticket
            .generation
            .checked_add(1)
            .ok_or("terminal view generation exhausted")?;
        self.ticket.generation = next;
        self.attached = attach;
        Ok(self.ticket())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_actor_rejects_detached_stale_and_peer_input() {
        let mut a = ViewAttachment::new(7);
        let b = ViewAttachment::new(7);
        let initial = a.ticket();
        assert!(a.admits(&initial));
        assert!(!a.admits(&b.ticket()));
        let detached = a.switch(&initial, false).unwrap();
        assert!(!a.admits(&initial));
        assert!(!a.admits(&detached));
        assert!(a.switch(&initial, true).is_err());
        let resumed = a.switch(&detached, true).unwrap();
        assert_eq!(initial.session, resumed.session);
        assert_eq!(initial.epoch, resumed.epoch);
        assert!(a.admits(&resumed));
        assert!(!a.admits(&initial));
        assert!(!a.admits(&detached));
        a.ticket.generation = u32::MAX;
        let last = a.ticket();
        assert!(a.switch(&last, false).is_err());
        assert!(a.admits(&last));
    }
}
