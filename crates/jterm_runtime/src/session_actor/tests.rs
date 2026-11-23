use super::*;
use std::os::fd::BorrowedFd;

#[test]
fn a_blocking_descriptor_is_refused_before_workers_and_still_cleaned_up() {
    use std::os::unix::net::UnixStream;

    struct Transport {
        descriptor: UnixStream,
        cleanup: Sender<&'static str>,
    }
    impl AsFd for Transport {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.descriptor.as_fd()
        }
    }
    impl SessionTransport for Transport {
        fn read(&mut self, _: &mut [u8]) -> Result<ReadOutcome, String> {
            panic!("reader must not start")
        }
        fn write(&mut self, _: &[u8]) -> Result<WriteOutcome, String> {
            panic!("writer must not start")
        }
        fn resize(&mut self, _: usize, _: usize) -> Result<(), String> {
            panic!("resize is not part of startup")
        }
        fn is_alive(&mut self) -> bool {
            panic!("reader must not start")
        }
        fn try_reap(&mut self) -> Option<i32> {
            panic!("reader must not start")
        }
        fn wait_timeout(&mut self, _: u64) -> Result<i32, String> {
            panic!("reader must not start")
        }
        fn signal_terminate(&mut self) {
            self.cleanup.send("terminate").unwrap();
        }
        fn force_kill_and_reap(&mut self) {
            self.cleanup.send("reap").unwrap();
        }
    }
    let (descriptor, _peer) = UnixStream::pair().unwrap();
    descriptor.set_nonblocking(false).unwrap();
    let (cleanup, witness) = bounded(2);
    let result = SessionActor::start(
        Transport {
            descriptor,
            cleanup,
        },
        WakeSink::new(|| {}),
    );
    assert!(matches!(result, Err(message) if message == "PTY transport must be nonblocking"));
    assert_eq!(
        witness.recv_timeout(Duration::from_secs(2)).unwrap(),
        "terminate"
    );
    assert_eq!(
        witness.recv_timeout(Duration::from_secs(2)).unwrap(),
        "reap"
    );
}

#[test]
fn wake_only_follows_publication_and_stop_releases_a_full_event_queue() {
    use std::sync::atomic::AtomicUsize;

    let wake_count = Arc::new(AtomicUsize::new(0));
    let observed = wake_count.clone();
    let wake = WakeSink::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    });
    let (event_tx, event_rx) = bounded(1);
    let (stop_tx, stop) = bounded::<()>(0);
    let events = WorkerEvents {
        tx: event_tx,
        stop,
        wake,
    };
    assert!(events.send(SessionEvent::Exit(7)));
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
    let (started_tx, started_rx) = bounded(0);
    let (finished_tx, finished_rx) = bounded(1);
    let worker = thread::spawn(move || {
        started_tx.send(()).unwrap();
        finished_tx
            .send(events.send(SessionEvent::Exit(8)))
            .unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(stop_tx);
    assert!(!finished_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    worker.join().unwrap();
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
    assert!(matches!(event_rx.recv().unwrap(), SessionEvent::Exit(7)));
    assert!(event_rx.is_empty());
}

enum ExitDrainStep {
    Bytes(Vec<u8>),
    Stop(ReadOutcome),
    Error(&'static str),
}

fn exercise_exit_drain(
    steps: Vec<ExitDrainStep>,
    buffer_size: usize,
) -> (bool, Vec<SessionEvent>, usize) {
    let mut steps: std::collections::VecDeque<_> = steps.into();
    let mut events = Vec::new();
    let mut read_calls = 0;
    let result = drain_exited_pty_output(
        &mut OutputSource::default(),
        &mut vec![0; buffer_size],
        |buffer| {
            read_calls += 1;
            match steps.pop_front().expect("unexpected extra tail read") {
                ExitDrainStep::Bytes(mut bytes) => {
                    let count = buffer.len().min(bytes.len());
                    buffer[..count].copy_from_slice(&bytes[..count]);
                    if count < bytes.len() {
                        bytes.drain(..count);
                        steps.push_front(ExitDrainStep::Bytes(bytes));
                    }
                    Ok(ReadOutcome::Data(count))
                }
                ExitDrainStep::Stop(outcome) => Ok(outcome),
                ExitDrainStep::Error(error) => Err(error.to_string()),
            }
        },
        |event| {
            events.push(event);
            true
        },
        || false,
    );
    (result, events, read_calls)
}

#[test]
fn exited_tail_keeps_the_same_reader_epoch_and_sequence_allocator() {
    let mut source = OutputSource::default();
    let read_at = std::time::Instant::now();
    let ordinary = source.coalesced(b"before".to_vec(), Some(read_at));
    let first = ordinary.fragments().next().unwrap().origin.unwrap();
    let mut read_once = false;
    let mut events = Vec::new();
    let finish = drain_exited_pty_output(
        &mut source,
        &mut [0; 8],
        |buffer| {
            if read_once {
                return Ok(ReadOutcome::WouldBlock);
            }
            read_once = true;
            buffer[..4].copy_from_slice(b"tail");
            Ok(ReadOutcome::Data(4))
        },
        |event| {
            events.push(event);
            true
        },
        || false,
    );
    assert!(finish);
    let SessionEvent::Output(tail) = &events[0] else {
        panic!("missing tail");
    };
    let last = tail.fragments().next().unwrap().origin.unwrap();
    assert_eq!(last.source, first.source);
    assert_eq!(last.sequence, first.sequence + 1);
    assert!(last.read_at >= first.read_at);
}

#[test]
fn exited_tail_stops_immediately_at_would_block_eof_or_hangup() {
    for stop in [
        ReadOutcome::WouldBlock,
        ReadOutcome::Eof,
        ReadOutcome::Hangup,
    ] {
        let (finish, events, reads) = exercise_exit_drain(
            vec![
                ExitDrainStep::Bytes(b"tail".to_vec()),
                ExitDrainStep::Stop(stop),
                ExitDrainStep::Error("must never poll or wait after this stop"),
            ],
            8,
        );
        assert!(finish);
        assert_eq!(reads, 2);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SessionEvent::Output(bytes) if bytes.as_bytes() == b"tail"));
    }
}

#[test]
fn exited_tail_error_follows_retained_output_before_exit_permission() {
    let (finish, events, reads) = exercise_exit_drain(
        vec![
            ExitDrainStep::Bytes(b"tail".to_vec()),
            ExitDrainStep::Error("read fixture failure"),
        ],
        8,
    );
    assert!(finish);
    assert_eq!(reads, 2);
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], SessionEvent::Output(bytes) if bytes.as_bytes() == b"tail"));
    assert!(matches!(
        &events[1],
        SessionEvent::ExitTail(ExitTail::ReadFailed)
    ));
}

#[test]
fn exited_tail_byte_cap_is_explicit_and_never_overread() {
    use crate::pty_reader::EXIT_TAIL_BYTES;

    let (finish, events, reads) = exercise_exit_drain(
        vec![ExitDrainStep::Bytes(vec![b'x'; EXIT_TAIL_BYTES + 1])],
        65_536,
    );
    assert!(finish);
    assert_eq!(reads, EXIT_TAIL_BYTES / 65_536);
    let bytes = events
        .iter()
        .map(|event| match event {
            SessionEvent::Output(bytes) => bytes.len(),
            _ => 0,
        })
        .sum::<usize>();
    assert_eq!(bytes, EXIT_TAIL_BYTES);
    assert!(matches!(
        events.last(),
        Some(SessionEvent::ExitTail(ExitTail::LimitReached))
    ));
}

#[test]
fn exited_tail_cancellation_and_receiver_loss_stop_without_exit_permission() {
    let finish = drain_exited_pty_output(
        &mut OutputSource::default(),
        &mut [0; 8],
        |_| panic!("cancelled tail must not read"),
        |_| panic!("cancelled tail must not publish"),
        || true,
    );
    assert!(!finish);

    let mut reads = 0;
    let finish = drain_exited_pty_output(
        &mut OutputSource::default(),
        &mut [0; 8],
        |buffer| {
            reads += 1;
            buffer[0] = b'x';
            Ok(ReadOutcome::Data(1))
        },
        |_| false,
        || false,
    );
    assert!(!finish);
    assert_eq!(reads, 1);
}

#[test]
fn exited_tail_zero_data_reports_error_instead_of_spinning() {
    let (finish, events, reads) =
        exercise_exit_drain(vec![ExitDrainStep::Stop(ReadOutcome::Data(0))], 8);
    assert!(finish);
    assert_eq!(reads, 1);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        SessionEvent::ExitTail(ExitTail::ReadFailed)
    ));
}

fn tiny_limits() -> WriteQueueLimits {
    WriteQueueLimits {
        byte_capacity: 10,
        message_capacity: 4,
        max_message_bytes: 8,
        interactive_reserve_bytes: 2,
        interactive_reserve_messages: 1,
        interactive_max_bytes: 2,
    }
}

fn test_sender(limits: WriteQueueLimits) -> (Arc<WriteQueue>, WriteSender) {
    let queue = Arc::new(WriteQueue::new(limits));
    let sender = WriteSender {
        queue: Arc::clone(&queue),
    };
    (queue, sender)
}

#[test]
fn native_action_barrier_waits_for_inflight_write_and_rejects_closed_writer() {
    let (queue, sender) = test_sender(tiny_limits());
    assert_eq!(queue.drained(), Ok(true));
    sender.try_send(b"edit".to_vec()).unwrap();
    assert_eq!(queue.drained(), Ok(false));
    let bytes = queue.take().unwrap();
    // No pending message remains, but the OS write has not completed.
    assert!(queue.state.lock().pending.is_empty());
    assert_eq!(queue.drained(), Ok(false));
    queue.finish(&bytes);
    assert_eq!(queue.drained(), Ok(true));
    queue.close();
    assert_eq!(queue.drained(), Err(WriteError::Closed));
}

#[test]
fn write_queue_is_fifo_byte_bounded_and_keeps_inflight_accounted() {
    let (queue, sender) = test_sender(tiny_limits());

    sender.try_send(vec![1; 8]).unwrap();
    let first = queue.take().unwrap();
    assert_eq!(first.bytes, vec![1; 8]);

    // pop 后仍计入 8 bytes；bulk 保留的 2-byte 交互区仍可接收短输入。
    sender.try_send(vec![2; 2]).unwrap();
    let error = sender.try_send(vec![3]).unwrap_err();
    assert!(matches!(
        error,
        WriteError::Full {
            requested_bytes: 1,
            queued_bytes: 10,
            ..
        }
    ));

    queue.finish(&first);
    let second = queue.take().unwrap();
    assert_eq!(second.bytes, vec![2; 2]);
    queue.finish(&second);

    let state = queue.state.lock();
    assert_eq!(state.accounted_bytes, 0);
    assert_eq!(state.accounted_messages, 0);
}

#[test]
fn write_queue_distinguishes_too_large_full_and_closed() {
    let (queue, sender) = test_sender(tiny_limits());

    assert!(matches!(
        sender.try_send(vec![0; 9]),
        Err(WriteError::TooLarge {
            requested_bytes: 9,
            max_message_bytes: 8
        })
    ));

    sender.try_send(vec![0; 8]).unwrap();
    assert!(matches!(
        sender.try_send(vec![0; 3]),
        Err(WriteError::Full { .. })
    ));
    queue.close();
    assert!(matches!(sender.try_send(vec![1]), Err(WriteError::Closed)));
    // Empty writes never allocate or enqueue and are harmless even after close.
    sender.try_send(Vec::new()).unwrap();
}

#[test]
fn closing_queue_drops_pending_but_keeps_inflight_accounted_until_finish() {
    let limits = WriteQueueLimits {
        byte_capacity: 5,
        message_capacity: 2,
        max_message_bytes: 4,
        interactive_reserve_bytes: 0,
        interactive_reserve_messages: 0,
        interactive_max_bytes: 4,
    };
    let (queue, sender) = test_sender(limits);
    sender.try_send(vec![1; 4]).unwrap();
    let in_flight = queue.take().unwrap();

    sender.try_send(vec![2]).unwrap();
    queue.close();
    assert!(matches!(sender.try_send(vec![3]), Err(WriteError::Closed)));

    {
        let state = queue.state.lock();
        assert_eq!(state.accounted_bytes, in_flight.bytes.len());
        assert_eq!(state.accounted_messages, 1);
        assert!(state.pending.is_empty());
    }
    queue.finish(&in_flight);
    let state = queue.state.lock();
    assert_eq!(state.accounted_bytes, 0);
    assert_eq!(state.accounted_messages, 0);
}

#[test]
fn write_queue_caps_message_count_as_well_as_bytes() {
    let limits = WriteQueueLimits {
        byte_capacity: 100,
        message_capacity: 2,
        max_message_bytes: 50,
        interactive_reserve_bytes: 0,
        interactive_reserve_messages: 0,
        interactive_max_bytes: 50,
    };
    let (_queue, sender) = test_sender(limits);
    sender.try_send(vec![1]).unwrap();
    sender.try_send(vec![2]).unwrap();
    assert!(matches!(
        sender.try_send(vec![3]),
        Err(WriteError::Full {
            queued_messages: 2,
            message_capacity: 2,
            ..
        })
    ));
}

#[test]
fn split_write_queue_keeps_a_shared_fifo_with_independent_byte_caps() {
    let mut queue = WriteQueue::new(WriteQueueLimits::PRODUCTION);
    queue.split = Some(SplitWriteLimits {
        user_bytes: 5,
        response_bytes: 7,
        entries: 3,
        coalesce_bytes: 4,
    });
    queue.try_enqueue(b"ab".to_vec()).unwrap();
    queue.try_enqueue(b"c".to_vec()).unwrap();
    let user = queue.take().unwrap();
    assert_eq!(user.bytes, b"abc");
    queue
        .try_enqueue_input(WriteOrigin::Reply, b"1234567".as_slice().into())
        .unwrap();
    queue.try_enqueue(b"de".to_vec()).unwrap();
    assert!(matches!(
        queue.try_enqueue(vec![0]),
        Err(WriteError::Full { .. })
    ));
    queue.finish(&user);
    let response = queue.take().unwrap();
    assert_eq!(response.bytes, b"1234567");
    assert!(matches!(
        queue.try_enqueue_input(WriteOrigin::Reply, b"x".as_slice().into()),
        Err(WriteError::Full { .. })
    ));
    queue.finish(&response);
    let last = queue.take().unwrap();
    assert_eq!(last.bytes, b"de");
    queue.finish(&last);
    assert_eq!(queue.drained(), Ok(true));
}

#[test]
fn reserved_reply_entries_survive_user_saturation_and_keep_fifo_order() {
    let mut queue = WriteQueue::new(WriteQueueLimits::PRODUCTION);
    queue.split = Some(SplitWriteLimits {
        user_bytes: 6,
        response_bytes: 3,
        entries: 3,
        coalesce_bytes: 0,
    });
    queue.reply_entries = 1;
    queue.try_enqueue(b"paste-".to_vec()).unwrap();
    let in_flight = queue.take().unwrap();
    assert!(matches!(
        queue.try_enqueue(vec![b'x']),
        Err(WriteError::Full { .. })
    ));
    queue.finish(&in_flight);

    queue.try_enqueue(b"A".to_vec()).unwrap();
    let in_flight = queue.take().unwrap();
    queue.try_enqueue(b"B".to_vec()).unwrap();
    assert!(matches!(
        queue.try_enqueue(vec![b'C']),
        Err(WriteError::Full { .. })
    ));
    queue
        .try_enqueue_input(WriteOrigin::Reply, b"rep".as_slice().into())
        .unwrap();
    assert!(matches!(
        queue.try_enqueue_input(WriteOrigin::Reply, b"x".as_slice().into()),
        Err(WriteError::Full { .. })
    ));
    assert_eq!(in_flight.bytes, b"A");
    queue.finish(&in_flight);
    let user = queue.take().unwrap();
    assert_eq!(user.bytes, b"B");
    queue.finish(&user);
    let reply = queue.take().unwrap();
    assert_eq!(reply.bytes, b"rep");
    queue.finish(&reply);
    assert_eq!(queue.drained(), Ok(true));
}

#[test]
fn retiring_admission_allows_an_inflight_message_to_finish() {
    let queue = WriteQueue::new(tiny_limits());
    queue.try_enqueue(b"release".to_vec()).unwrap();
    let message = queue.take().unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let start = std::time::Instant::now();
            while !queue.state.lock().retiring {
                assert!(start.elapsed() < Duration::from_secs(1));
                std::thread::yield_now();
            }
            assert_eq!(queue.try_enqueue(vec![0]), Err(WriteError::Closed));
            queue.finish(&message);
        });
        queue.retire_and_drain(Duration::from_secs(1));
        assert_eq!(queue.drained(), Ok(true));
    });
    assert_eq!(queue.try_enqueue(vec![0]), Err(WriteError::Closed));
}

#[test]
fn osc5522_maximum_legal_response_fits_single_message_limit() {
    const RAW_BYTES: usize = 32 * 1024 * 1024;
    const CHUNK_BYTES: usize = 4096;
    // Base64 plus a deliberately generous 256 bytes of OSC framing per chunk.
    let base64_bytes = RAW_BYTES.div_ceil(3) * 4;
    let chunks = RAW_BYTES.div_ceil(CHUNK_BYTES);
    let conservative_response_bytes = base64_bytes + chunks * 256 + 1024;
    assert!(conservative_response_bytes <= WRITE_MESSAGE_BYTE_CAP);
}
