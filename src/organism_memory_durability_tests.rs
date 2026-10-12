//! Pure fault injection through the production worker, never filesystem I/O.
use super::*;
use std::sync::atomic::AtomicUsize;

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    path: PathBuf,
    disk: Mutex<DiskMemory>,
    mode: AtomicUsize,
    fail_sync: AtomicBool,
    writes: AtomicUsize,
    syncs: AtomicUsize,
}
impl Fixture {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            path: PathBuf::from(format!(
                "/fixture/organism-durability-{}",
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            )),
            disk: Mutex::new(DiskMemory::default()),
            mode: AtomicUsize::new(0),
            fail_sync: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
            syncs: AtomicUsize::new(0),
        })
    }
    fn enqueue(&self, id: u64) {
        assert!(matches!(
            enqueue_event_with_scheduler(self.path.clone(), &event(id), |_| Ok(())),
            EventEnqueue::Retained(Ok(()))
        ));
    }
    fn publish(self: &Arc<Self>, events: &[MemoryEvent]) -> MemoryPublication {
        if self.mode.load(Ordering::SeqCst) == 1 {
            return MemoryPublication::NotPublished(io::Error::other("fixture before publication"));
        }
        {
            let mut disk = self.disk.lock().unwrap();
            for event in events {
                apply_event(&mut disk, event);
            }
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.mode.load(Ordering::SeqCst) == 2 {
            // Capture only the sync controls, not Fixture: Drop must be able to
            // remove a pending receipt without an Arc cycle through the map.
            let weak = Arc::downgrade(self);
            MemoryPublication::PublishedDurabilityPending {
                error: io::Error::other("fixture after publication"),
                retry: Arc::new(move || {
                    let f = weak.upgrade().expect("fixture alive during job");
                    f.syncs.fetch_add(1, Ordering::SeqCst);
                    if f.fail_sync.load(Ordering::SeqCst) {
                        Err(io::Error::other("fixture sync failed"))
                    } else {
                        Ok(())
                    }
                }),
            }
        } else {
            MemoryPublication::Durable
        }
    }
    fn run(self: &Arc<Self>) -> io::Result<()> {
        run_queued_events_with(&self.path, |_, events| self.publish(events))
    }
    fn count(&self) -> u32 {
        self.disk
            .lock()
            .unwrap()
            .stats(123, "/fixture/repo")
            .unwrap()
            .build_failures
    }
    fn queued(&self) -> usize {
        event_queues()
            .lock()
            .unwrap()
            .get(&self.path)
            .map_or(0, VecDeque::len)
    }
    fn pending(&self) -> bool {
        pending_durability()
            .lock()
            .unwrap()
            .contains_key(&self.path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        event_queues().lock().unwrap().remove(&self.path);
        pending_durability().lock().unwrap().remove(&self.path);
        acknowledged_events().lock().unwrap().remove(&self.path);
        // Lookup prunes dead weak entries, without retaining a per-test key.
        drop(path_worker_gate(Path::new("/fixture/gate-prune")));
    }
}
fn event(id: u64) -> MemoryEvent {
    MemoryEvent {
        id: format!("durability-{id:08}"),
        at_ms: 100000 + id,
        day: 123,
        activity_bucket: 0,
        repo: Some("/fixture/repo".into()),
        kind: CommandKind::BuildOrTest,
        exit_code: Some(1),
        duration_ms: None,
        life: LifeSnapshot::default(),
    }
}

#[test]
fn unpublished_failure_retains_batch_and_ack_absence() {
    let f = Fixture::new();
    f.mode.store(1, Ordering::SeqCst);
    f.enqueue(0);
    assert!(f.run().is_err());
    assert_eq!(f.queued(), 1);
    assert!(!f.pending());
    assert!(!acknowledged_events().lock().unwrap().contains_key(&f.path));
    f.mode.store(0, Ordering::SeqCst);
    f.run().unwrap();
    assert_eq!(f.count(), 1);
    assert_eq!(f.queued(), 0);
}
#[test]
fn published_error_never_replays_after_512_other_events() {
    let f = Fixture::new();
    f.mode.store(2, Ordering::SeqCst);
    f.enqueue(0);
    assert!(f.run().is_err());
    assert_eq!(f.queued(), 0);
    assert!(f.pending());
    assert!(acknowledged_events().lock().unwrap()[&f.path].contains(&event(0).id));
    {
        let mut disk = f.disk.lock().unwrap();
        for id in 1..=512 {
            apply_event(&mut disk, &event(id));
        }
        assert!(!disk.has_event_id(&event(0).id));
    }
    f.fail_sync.store(true, Ordering::SeqCst);
    f.enqueue(513);
    for _ in 0..4 {
        assert!(f.run().is_err());
    }
    assert_eq!(f.count(), 513);
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
    assert_eq!(f.queued(), 1);
    f.fail_sync.store(false, Ordering::SeqCst);
    f.mode.store(0, Ordering::SeqCst);
    f.run().unwrap();
    assert_eq!(f.count(), 514);
    assert!(!f.pending());
    assert_eq!(f.queued(), 0);
}
#[test]
fn receipt_only_work_survives_scheduling_failure_and_retains_queue_bound() {
    let f = Fixture::new();
    f.mode.store(2, Ordering::SeqCst);
    f.enqueue(0);
    assert!(f.run().is_err());
    assert!(pending_paths().contains(&f.path));
    assert_eq!(f.queued(), 0);
    assert!(matches!(
        enqueue_event_with_scheduler(f.path.clone(), &event(1), |_| Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "fixture saturation"
        ))),
        EventEnqueue::Retained(Err(_))
    ));
    for id in 2..=256 {
        f.enqueue(id);
    }
    assert!(
        matches!(enqueue_event_with_scheduler(f.path.clone(), &event(257), |_| Ok(())), EventEnqueue::Rejected(e) if e.kind()==io::ErrorKind::WouldBlock)
    );
    f.fail_sync.store(true, Ordering::SeqCst);
    assert!(f.run().is_err());
    assert_eq!(f.queued(), 256);
}
#[test]
fn simultaneous_workers_publish_one_batch_once() {
    let f = Fixture::new();
    f.enqueue(0);
    let a = f.clone();
    let b = f.clone();
    let a = std::thread::spawn(move || a.run());
    let b = std::thread::spawn(move || b.run());
    a.join().unwrap().unwrap();
    b.join().unwrap().unwrap();
    assert_eq!(f.count(), 1);
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
}
#[test]
fn enqueue_and_flush_snapshot_progress_during_worker_io() {
    let f = Fixture::new();
    f.enqueue(0);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let worker_fixture = f.clone();
    let worker = std::thread::spawn(move || {
        run_queued_events_with(&worker_fixture.path, |_, events| {
            entered_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            worker_fixture.publish(events)
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    f.enqueue(1);
    assert!(pending_paths().contains(&f.path));
    assert_eq!(f.queued(), 2);
    resume_tx.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert_eq!(f.queued(), 1);
    f.run().unwrap();
    assert_eq!(f.count(), 2);
}
#[test]
fn atomic_failure_conversion_preserves_error_and_success() {
    let outcome: MemoryPublication = crate::atomic_file::AtomicWriteOutcome::NotPublished(
        io::Error::new(io::ErrorKind::PermissionDenied, "fixture"),
    )
    .into();
    assert!(
        matches!(outcome,MemoryPublication::NotPublished(e) if e.kind()==io::ErrorKind::PermissionDenied)
    );
    assert!(matches!(
        MemoryPublication::from(crate::atomic_file::AtomicWriteOutcome::Durable),
        MemoryPublication::Durable
    ));
}

#[test]
fn receipt_only_retry_completes_without_republishing() {
    let f = Fixture::new();
    f.mode.store(2, Ordering::SeqCst);
    f.enqueue(0);
    assert!(f.run().is_err());
    assert_eq!(f.queued(), 0);
    assert!(pending_paths().contains(&f.path));
    f.fail_sync.store(true, Ordering::SeqCst);
    assert!(f.run().is_err());
    assert!(f.pending());
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
    f.fail_sync.store(false, Ordering::SeqCst);
    f.run().unwrap();
    assert!(!f.pending());
    assert_eq!(f.queued(), 0);
    assert!(!pending_paths().contains(&f.path));
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
    assert_eq!(f.syncs.load(Ordering::SeqCst), 2);
    assert_eq!(f.count(), 1);
}
