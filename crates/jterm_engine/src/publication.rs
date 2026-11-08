//! Pure per-subscriber delivery state. The adapter owns transport and scheduling.
use crate::frame::{CellFrame, Delta, Stamp};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Epoch,
    Stale,
    Ack,
    Exhausted,
    Layout,
}
/// Allocate publication revisions without wrapping. A new epoch is an explicit host decision.
pub struct Sequence {
    epoch: u64,
    revision: u64,
}
impl Sequence {
    pub fn new(epoch: u64) -> Result<Self, Error> {
        if epoch == 0 {
            return Err(Error::Epoch);
        }
        Ok(Self { epoch, revision: 0 })
    }
    pub fn after(epoch: u64, revision: u64) -> Result<Self, Error> {
        let mut s = Self::new(epoch)?;
        s.revision = revision;
        Ok(s)
    }
    pub fn next_revision(&mut self) -> Result<(u64, u64), Error> {
        let next = self.revision.checked_add(1).ok_or(Error::Exhausted)?;
        self.revision = next;
        Ok((self.epoch, next))
    }
}
#[derive(Clone, Debug)]
pub enum Update {
    Full(CellFrame),
    Delta(Delta),
}
impl Update {
    pub fn stamp(&self) -> Stamp {
        match self {
            Self::Full(f) => f.stamp(),
            Self::Delta(d) => d.next,
        }
    }
}
/// At most one acknowledged baseline, one in-flight frame and one latest desired frame.
/// No queue grows with PTY throughput. All cell rows are immutable/shared.
pub struct Subscriber {
    epoch: u64,
    latest_stamp: Option<Stamp>,
    base: Option<CellFrame>,
    flight: Option<CellFrame>,
    desired: Option<CellFrame>,
}
impl Subscriber {
    pub fn new(epoch: u64) -> Result<Self, Error> {
        if epoch == 0 {
            return Err(Error::Epoch);
        }
        Ok(Self {
            epoch,
            latest_stamp: None,
            base: None,
            flight: None,
            desired: None,
        })
    }
    pub fn offer(&mut self, frame: CellFrame) -> Result<(), Error> {
        let stamp = frame.stamp();
        if stamp.epoch != self.epoch {
            return Err(Error::Epoch);
        }
        if let Some(old) = self.latest_stamp {
            if stamp.revision <= old.revision {
                return Err(Error::Stale);
            }
            if stamp.layout < old.layout {
                return Err(Error::Layout);
            }
        }
        self.latest_stamp = Some(stamp);
        self.desired = Some(frame);
        Ok(())
    }
    /// No next update until exact ACK/rejection. Calling twice cannot duplicate a send.
    pub fn take(&mut self) -> Option<Update> {
        if self.flight.is_some() {
            return None;
        }
        let next = self.desired.take()?;
        let update = match self
            .base
            .as_ref()
            .and_then(|base| base.delta_to(&next).ok())
        {
            Some(delta) => Update::Delta(delta),
            None => Update::Full(next.clone()),
        };
        self.flight = Some(next);
        Some(update)
    }
    pub fn acknowledge(&mut self, stamp: Stamp) -> Result<(), Error> {
        if self.flight.as_ref().map(CellFrame::stamp) != Some(stamp) {
            return Err(Error::Ack);
        }
        self.base = self.flight.take();
        Ok(())
    }
    /// Receiver rejected the exact flight. Drop dependency and wait for a newer
    /// desired publication (or a newly captured one); never resend a stale revision.
    pub fn reject(&mut self, stamp: Stamp) -> Result<(), Error> {
        if self.flight.as_ref().map(CellFrame::stamp) != Some(stamp) {
            return Err(Error::Ack);
        }
        self.flight = None;
        self.base = None;
        Ok(())
    }
    pub fn retained_frames(&self) -> usize {
        usize::from(self.base.is_some())
            + usize::from(self.flight.is_some())
            + usize::from(self.desired.is_some())
    }
}
