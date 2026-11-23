//! Bounded in-process session I/O for Unix PTY adapters.
//!
//! A session owns one native transport and is the only place that schedules its
//! reads, input and protocol-write FIFO, resize, and termination. Consumers
//! apply ordered output batches to their existing terminal owner.

use crate::{
    exit_status::ExitTail,
    output_batch::{OutputBuffer, OutputSource},
    pty_reader::{
        drain_exit_tail, poll_reader, run_observed, shutdown_pipe, PtyReadEvent, ReaderPoll,
    },
    runtime_wake::WakeSink,
};
use crossbeam_channel::{bounded, select_biased, Receiver, Sender};
use parking_lot::{Condvar, Mutex};
use std::{
    collections::VecDeque,
    fmt, io,
    os::fd::{AsFd, AsRawFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

pub use crate::pty_reader::ReadOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Written(usize),
    WouldBlock,
}

/// Platform transport and exit policy for a single owned session.
/// Read/write operations are nonblocking. Data and Written lengths must be
/// positive and bounded by the supplied slice. Hangup can be transient.
pub trait SessionTransport: AsFd + Send + 'static {
    fn read(&mut self, buffer: &mut [u8]) -> Result<ReadOutcome, String>;
    fn write(&mut self, bytes: &[u8]) -> Result<WriteOutcome, String>;
    fn resize(&mut self, cols: usize, rows: usize) -> Result<(), String>;
    fn is_alive(&mut self) -> bool;
    fn try_reap(&mut self) -> Option<i32>;
    fn wait_timeout(&mut self, timeout_ms: u64) -> Result<i32, String>;
    /// If present, retain the leader's wait status through the exit tail.
    /// The adapter's try_reap is called only after tail delivery.
    fn observe_before_reap_pid(&self) -> Option<i32> {
        None
    }
    fn control_input_available(&mut self) -> bool {
        self.is_alive()
    }
    fn input_admitted() -> bool {
        true
    }
    /// Write once under the actor's immediate-control fence. An adapter may
    /// add a native input admission fence around this single syscall.
    fn write_control_byte(&mut self, byte: u8) -> bool {
        // SAFETY: the actor holds this transport and confirms nonblocking mode.
        unsafe { libc::write(self.as_fd().as_raw_fd(), (&byte as *const u8).cast(), 1) == 1 }
    }
    fn termination_grace(&self) -> Duration {
        Duration::from_millis(30)
    }
    /// Transfer teardown to an existing bounded cleanup owner, if the
    /// transport reserved one before launch. The actor has stopped I/O.
    fn handoff_cleanup(&mut self) -> bool {
        false
    }
    fn signal_terminate(&mut self);
    fn force_kill_and_reap(&mut self);
}

#[derive(Clone)]
struct WorkerEvents {
    tx: Sender<SessionEvent>,
    stop: Receiver<()>,
    wake: WakeSink,
}

impl WorkerEvents {
    fn send(&self, event: SessionEvent) -> bool {
        select_biased! {
            recv(self.stop) -> _ => false,
            send(self.tx, event) -> result => {
                if result.is_err() {
                    return false;
                }
                self.wake.wake();
                true
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    Output(OutputBuffer),
    ExitTail(ExitTail),
    Exit(i32),
    Error(String),
}

fn drain_exited_pty_output(
    source: &mut OutputSource,
    buffer: &mut [u8],
    read: impl FnMut(&mut [u8]) -> Result<ReadOutcome, String>,
    mut emit: impl FnMut(SessionEvent) -> bool,
    cancelled: impl FnMut() -> bool,
) -> bool {
    let tail = drain_exit_tail(
        buffer,
        read,
        |bytes, read_at| emit(SessionEvent::Output(source.batch(bytes.to_vec(), read_at))),
        cancelled,
    );
    tail.is_some_and(|tail| tail == ExitTail::Complete || emit(SessionEvent::ExitTail(tail)))
}

/// 事件 channel 容量上限。每个事件最多 ~128KB(BATCH_SIZE_THRESHOLD),
/// 256 * 128KB ≈ 32MB 为内存上界,既能吸收突发又能阻止无限堆积。
const EVENT_CHANNEL_CAP: usize = 256;

/// PTY writer 的硬上限包含等待队列和正在执行的 OS write。OSC 5522 会把
/// 32 MiB 剪贴板内容编码为约 43 MiB 的单条响应，因此单消息上限需留足空间。
const WRITE_QUEUE_BYTE_CAP: usize = 64 * 1024 * 1024;
const WRITE_MESSAGE_BYTE_CAP: usize = 48 * 1024 * 1024;
const WRITE_QUEUE_MESSAGE_CAP: usize = 8192;

/// bulk 写入不能占用最后这部分容量，尽量保证它阻塞时短按键、DSR 回复仍可入队。
const WRITE_INTERACTIVE_RESERVE_BYTES: usize = 64 * 1024;
const WRITE_INTERACTIVE_RESERVE_MESSAGES: usize = 256;
const WRITE_INTERACTIVE_MAX_BYTES: usize = 64;

#[derive(Clone, Copy)]
struct WriteQueueLimits {
    byte_capacity: usize,
    message_capacity: usize,
    max_message_bytes: usize,
    interactive_reserve_bytes: usize,
    interactive_reserve_messages: usize,
    interactive_max_bytes: usize,
}

impl WriteQueueLimits {
    const PRODUCTION: Self = Self {
        byte_capacity: WRITE_QUEUE_BYTE_CAP,
        message_capacity: WRITE_QUEUE_MESSAGE_CAP,
        max_message_bytes: WRITE_MESSAGE_BYTE_CAP,
        interactive_reserve_bytes: WRITE_INTERACTIVE_RESERVE_BYTES,
        interactive_reserve_messages: WRITE_INTERACTIVE_RESERVE_MESSAGES,
        interactive_max_bytes: WRITE_INTERACTIVE_MAX_BYTES,
    };
}

/// 非阻塞 PTY 写入的可判别失败原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// 当前按字节或消息计数的队列已满；调用方可保留输入并在后续帧重试。
    Full {
        requested_bytes: usize,
        queued_bytes: usize,
        byte_capacity: usize,
        queued_messages: usize,
        message_capacity: usize,
    },
    /// 单条消息永远不可能进入队列；拆分协议消息或拒绝该操作。
    TooLarge {
        requested_bytes: usize,
        max_message_bytes: usize,
    },
    /// writer 已停止，后续数据不会被接受。
    Closed,
}

impl WriteError {
    /// Full is transient and guarantees zero-byte enqueue, so callers can
    /// retain their exact input and retry. TooLarge/Closed cannot recover
    /// merely by waiting for queue capacity.
    pub fn is_backpressure(&self) -> bool {
        matches!(self, Self::Full { .. })
    }
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full {
                requested_bytes,
                queued_bytes,
                byte_capacity,
                queued_messages,
                message_capacity,
            } => write!(
                f,
                "PTY writer queue full: requested {requested_bytes} bytes, \
                 {queued_bytes}/{byte_capacity} bytes and \
                 {queued_messages}/{message_capacity} messages are accounted"
            ),
            Self::TooLarge {
                requested_bytes,
                max_message_bytes,
            } => write!(
                f,
                "PTY write message is too large: {requested_bytes} bytes exceeds the \
                 {max_message_bytes}-byte limit"
            ),
            Self::Closed => f.write_str("PTY writer has stopped"),
        }
    }
}

impl std::error::Error for WriteError {}

struct WriteQueueState {
    pending: VecDeque<WriteMessage>,
    /// 包含 pending 和已 pop、尚未完成/失败的 in-flight 消息。
    accounted_bytes: usize,
    accounted_messages: usize,
    reply_bytes: usize,
    retiring: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteOrigin {
    User,
    Reply,
}

struct WriteMessage {
    origin: WriteOrigin,
    bytes: Vec<u8>,
}

/// Independent byte allowances for the user and the terminal parser, sharing
/// one FIFO and allocation-count limit.
#[derive(Clone, Copy)]
pub struct SplitWriteLimits {
    pub user_bytes: usize,
    pub response_bytes: usize,
    pub entries: usize,
    pub coalesce_bytes: usize,
}

#[derive(Clone, Copy)]
pub struct InputBacklog {
    pub entries: usize,
    pub user_bytes: usize,
    pub response_bytes: usize,
}

struct WriteQueue {
    state: Mutex<WriteQueueState>,
    ready: Condvar,
    limits: WriteQueueLimits,
    split: Option<SplitWriteLimits>,
    input_admitted: fn() -> bool,
    owner_writes: bool,
    closed: AtomicBool,
}

impl WriteQueue {
    fn new(limits: WriteQueueLimits) -> Self {
        debug_assert!(limits.max_message_bytes <= limits.byte_capacity);
        debug_assert!(limits.interactive_reserve_bytes < limits.byte_capacity);
        debug_assert!(limits.interactive_reserve_messages < limits.message_capacity);
        Self {
            state: Mutex::new(WriteQueueState {
                pending: VecDeque::new(),
                accounted_bytes: 0,
                accounted_messages: 0,
                reply_bytes: 0,
                retiring: false,
            }),
            ready: Condvar::new(),
            limits,
            split: None,
            input_admitted: || true,
            owner_writes: false,
            closed: AtomicBool::new(false),
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn effective_capacity(
        &self,
        state: &WriteQueueState,
        origin: WriteOrigin,
        message_bytes: usize,
    ) -> (usize, usize, usize) {
        if let Some(split) = self.split {
            let (used, capacity) = match origin {
                WriteOrigin::User => (state.accounted_bytes - state.reply_bytes, split.user_bytes),
                WriteOrigin::Reply => (state.reply_bytes, split.response_bytes),
            };
            return (used, capacity, split.entries);
        }
        if message_bytes <= self.limits.interactive_max_bytes {
            (
                state.accounted_bytes,
                self.limits.byte_capacity,
                self.limits.message_capacity,
            )
        } else {
            (
                state.accounted_bytes,
                self.limits
                    .byte_capacity
                    .saturating_sub(self.limits.interactive_reserve_bytes),
                self.limits
                    .message_capacity
                    .saturating_sub(self.limits.interactive_reserve_messages),
            )
        }
    }

    fn admission(
        &self,
        state: &WriteQueueState,
        origin: WriteOrigin,
        requested_bytes: usize,
    ) -> Result<bool, WriteError> {
        let (queued_bytes, byte_capacity, message_capacity) =
            self.effective_capacity(state, origin, requested_bytes);
        let max_message_bytes = if self.split.is_some() {
            byte_capacity
        } else {
            self.limits.max_message_bytes
        };
        if requested_bytes > max_message_bytes {
            return Err(WriteError::TooLarge {
                requested_bytes,
                max_message_bytes,
            });
        }
        if self.owner_writes || self.is_closed() || state.retiring || !(self.input_admitted)() {
            return Err(WriteError::Closed);
        }
        let coalesce = self.split.is_some_and(|split| {
            state.pending.back().is_some_and(|back| {
                back.origin == origin
                    && requested_bytes <= split.coalesce_bytes.saturating_sub(back.bytes.len())
            })
        });
        if requested_bytes > byte_capacity.saturating_sub(queued_bytes)
            || (!coalesce && state.accounted_messages >= message_capacity)
        {
            return Err(WriteError::Full {
                requested_bytes,
                queued_bytes,
                byte_capacity,
                queued_messages: state.accounted_messages,
                message_capacity,
            });
        }
        Ok(coalesce)
    }

    fn enqueue_locked(
        &self,
        state: &mut WriteQueueState,
        origin: WriteOrigin,
        data: std::borrow::Cow<'_, [u8]>,
    ) -> Result<(), WriteError> {
        let coalesce = self.admission(state, origin, data.len())?;
        state.accounted_bytes += data.len();
        if origin == WriteOrigin::Reply {
            state.reply_bytes += data.len();
        }
        if coalesce {
            state
                .pending
                .back_mut()
                .unwrap()
                .bytes
                .extend_from_slice(&data);
        } else {
            state.accounted_messages += 1;
            state.pending.push_back(WriteMessage {
                origin,
                bytes: data.into_owned(),
            });
        }
        self.ready.notify_one();
        Ok(())
    }

    fn try_enqueue(&self, data: Vec<u8>) -> Result<(), WriteError> {
        self.try_enqueue_input(WriteOrigin::User, data.into())
    }

    fn try_enqueue_input(
        &self,
        origin: WriteOrigin,
        data: std::borrow::Cow<'_, [u8]>,
    ) -> Result<(), WriteError> {
        if data.is_empty() {
            return Ok(());
        }
        self.enqueue_locked(&mut self.state.lock(), origin, data)
    }

    fn take(&self) -> Option<WriteMessage> {
        let mut state = self.state.lock();
        loop {
            if let Some(data) = state.pending.pop_front() {
                // accounted_* 在 OS write 完成或失败前保持不变。
                return Some(data);
            }
            if self.is_closed() {
                return None;
            }
            self.ready.wait(&mut state);
        }
    }

    fn finish(&self, message: &WriteMessage) {
        let mut state = self.state.lock();
        debug_assert!(state.accounted_bytes >= message.bytes.len());
        debug_assert!(state.accounted_messages > 0);
        state.accounted_bytes -= message.bytes.len();
        state.accounted_messages = state.accounted_messages.saturating_sub(1);
        if message.origin == WriteOrigin::Reply {
            state.reply_bytes -= message.bytes.len();
        }
        if state.retiring && state.accounted_messages == 0 {
            self.ready.notify_all();
        }
    }

    fn retire_and_drain(&self, wait: Duration) {
        let start = std::time::Instant::now();
        let mut state = self.state.lock();
        state.retiring = true;
        while state.accounted_messages != 0 && !self.is_closed() {
            let remaining = wait.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                break;
            }
            self.ready.wait_for(&mut state, remaining);
        }
    }

    fn drained(&self) -> Result<bool, WriteError> {
        let state = self.state.lock();
        if self.is_closed() {
            Err(WriteError::Closed)
        } else {
            // `take()` does not release this accounting: an empty pending
            // deque alone would allow a native command to overtake OS writes.
            Ok(state.accounted_bytes == 0 && state.accounted_messages == 0)
        }
    }

    fn close(&self) {
        let mut state = self.state.lock();
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        while let Some(message) = state.pending.pop_front() {
            state.accounted_bytes -= message.bytes.len();
            state.accounted_messages -= 1;
            if message.origin == WriteOrigin::Reply {
                state.reply_bytes -= message.bytes.len();
            }
        }
        self.ready.notify_all();
    }
}

/// 可克隆的统一 PTY 写入口。成功返回表示整条消息已按字节计入 FIFO；
/// 不表示内核已经写完。任何错误都保证消息没有部分入队。
#[derive(Clone)]
pub struct WriteSender {
    queue: Arc<WriteQueue>,
}

impl WriteSender {
    /// UI/输入路径使用：从不等待容量。
    pub fn try_send(&self, data: Vec<u8>) -> Result<(), WriteError> {
        self.queue.try_enqueue(data)
    }
}

/// Owns the transport and serializes a session's input and lifecycle.
pub struct SessionActor<T: SessionTransport> {
    source: Option<u64>,
    pty: Arc<Mutex<T>>,
    event_rx: Receiver<SessionEvent>,
    shutdown: Arc<AtomicBool>,
    leader_exited: Arc<AtomicBool>,
    teardown_started: AtomicBool,
    stop: Mutex<WorkerShutdown>,
    write_tx: WriteSender,
}

struct WorkerShutdown {
    events: Option<Sender<()>>,
    reader: Option<OwnedFd>,
}

impl<T: SessionTransport> SessionActor<T> {
    pub fn start(transport: T, wake: WakeSink) -> Result<Self, String> {
        Self::start_with_limits(transport, wake, None, false)
    }

    pub fn start_with_split_writes(
        transport: T,
        wake: WakeSink,
        limits: SplitWriteLimits,
    ) -> Result<Self, String> {
        if limits.entries == 0
            || limits.user_bytes == 0
            || limits.response_bytes == 0
            || limits
                .user_bytes
                .checked_add(limits.response_bytes)
                .is_none()
        {
            return Err("invalid PTY write queue limits".into());
        }
        Self::start_with_limits(transport, wake, Some(limits), false)
    }

    /// The parser owner drives the bounded input FIFO when its callers need
    /// the exact prefix accepted by the kernel. All syscalls remain fenced by
    /// the actor, and the dedicated reader works independently of the owner.
    #[cfg(all(target_os = "linux", feature = "linux-pty"))]
    pub(crate) fn start_with_owner_writes(transport: T, wake: WakeSink) -> Result<Self, String> {
        Self::start_with_limits(transport, wake, None, true)
    }

    fn start_with_limits(
        transport: T,
        wake: WakeSink,
        limits: Option<SplitWriteLimits>,
        owner_writes: bool,
    ) -> Result<Self, String> {
        let child_pid = transport.observe_before_reap_pid();
        // The observe-before-reap reader may emit a whole MiB per event.
        let event_capacity = if child_pid.is_some() {
            4
        } else {
            EVENT_CHANNEL_CAP
        };
        let (event_tx, event_rx) = bounded(event_capacity);
        let (stop_tx, stop_rx) = bounded::<()>(0);
        let events = WorkerEvents {
            tx: event_tx,
            stop: stop_rx,
            wake,
        };
        let source = OutputSource::default();
        let mut queue = WriteQueue::new(WriteQueueLimits::PRODUCTION);
        queue.split = limits;
        queue.input_admitted = T::input_admitted;
        queue.owner_writes = owner_writes;
        let mut actor = Self {
            source: source.epoch(),
            pty: Arc::new(Mutex::new(transport)),
            event_rx,
            shutdown: Arc::new(AtomicBool::new(false)),
            leader_exited: Arc::new(AtomicBool::new(false)),
            teardown_started: AtomicBool::new(false),
            stop: Mutex::new(WorkerShutdown {
                events: Some(stop_tx),
                reader: None,
            }),
            write_tx: WriteSender {
                queue: Arc::new(queue),
            },
        };
        // The actor owns cleanup before the first fallible startup step.
        // Workers can poll the duplicate without retaining the I/O mutex.
        let descriptor = {
            let transport = actor.pty.lock();
            let fd = transport.as_fd();
            // SAFETY: transport retains the descriptor under this lock.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
            if flags < 0 {
                return Err(format!(
                    "Failed to inspect PTY descriptor: {}",
                    io::Error::last_os_error()
                ));
            }
            if flags & libc::O_NONBLOCK == 0 {
                return Err("PTY transport must be nonblocking".into());
            }
            Arc::new(
                fd.try_clone_to_owned()
                    .map_err(|error| format!("Failed to duplicate PTY poll descriptor: {error}"))?,
            )
        };

        let (shutdown_read, shutdown_write) =
            shutdown_pipe().map_err(|error| format!("Failed to create PTY wake pipe: {error}"))?;
        actor.stop.get_mut().reader = Some(shutdown_write);
        let reader_pty = actor.pty.clone();
        let reader_events = events.clone();
        let reader_descriptor = descriptor.clone();
        let reader_shutdown = actor.shutdown.clone();
        let reader_queue = actor.write_tx.queue.clone();
        let reader_exited = actor.leader_exited.clone();
        thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || match child_pid {
                Some(pid) => Self::run_anchored_reader(
                    reader_pty,
                    reader_events,
                    reader_descriptor,
                    reader_shutdown,
                    shutdown_read,
                    reader_queue,
                    source,
                    pid,
                    reader_exited,
                ),
                None => Self::io_loop(
                    reader_pty,
                    reader_events,
                    reader_descriptor,
                    reader_shutdown,
                    shutdown_read,
                    reader_queue,
                    source,
                ),
            })
            .map_err(|error| format!("Failed to spawn PTY reader: {error}"))?;

        if !owner_writes {
            let writer_pty = actor.pty.clone();
            let queue = actor.write_tx.queue.clone();
            thread::Builder::new()
                .name("pty-writer".into())
                .spawn(move || {
                    Self::writer_loop(writer_pty, events, descriptor, queue);
                })
                .map_err(|error| format!("Failed to spawn PTY writer: {error}"))?;
        }
        Ok(actor)
    }

    fn writer_loop(
        pty: Arc<Mutex<T>>,
        events: WorkerEvents,
        descriptor: Arc<OwnedFd>,
        queue: Arc<WriteQueue>,
    ) {
        while let Some(message) = queue.take() {
            if queue.is_closed() {
                queue.finish(&message);
                break;
            }
            if let Err(error) = Self::write_to_pty(&pty, &descriptor, &queue, &message.bytes) {
                // No later message may follow an incomplete logical input.
                queue.close();
                queue.finish(&message);
                drop(message);
                log::error!("Failed to write to PTY: {error}");
                events.send(SessionEvent::Error(format!(
                    "PTY writer stopped after a partial or failed write: {error}"
                )));
                return;
            }
            queue.finish(&message);
        }
        queue.close();
    }

    #[allow(clippy::too_many_arguments)]
    fn run_anchored_reader(
        transport: Arc<Mutex<T>>,
        events: WorkerEvents,
        descriptor: Arc<OwnedFd>,
        shutdown: Arc<AtomicBool>,
        shutdown_read: OwnedFd,
        queue: Arc<WriteQueue>,
        mut source: OutputSource,
        child_pid: i32,
        leader_exited: Arc<AtomicBool>,
    ) {
        let retire_input = |exiting| {
            // Serialize retirement against the native writer's last syscall.
            let _guard = transport.lock();
            if exiting {
                leader_exited.store(true, Ordering::Release);
            }
            queue.close();
        };
        run_observed(
            descriptor.as_fd(),
            shutdown_read.as_fd(),
            child_pid,
            || retire_input(true),
            |event| match event {
                PtyReadEvent::Output { bytes, read_at } => {
                    events.send(SessionEvent::Output(source.batch(bytes, read_at)))
                }
                PtyReadEvent::Error(error) => {
                    retire_input(false);
                    events.send(SessionEvent::Error(format!("PTY reader stopped: {error}")))
                }
                PtyReadEvent::Exit { tail, .. } => {
                    while !shutdown.load(Ordering::Acquire) {
                        let reaped = transport.lock().try_reap();
                        if let Some(code) = reaped {
                            if tail != ExitTail::Complete
                                && !events.send(SessionEvent::ExitTail(tail))
                            {
                                return false;
                            }
                            return events.send(SessionEvent::Exit(code));
                        }
                        if !matches!(
                            poll_reader(-1, shutdown_read.as_raw_fd(), 25),
                            Ok(ReaderPoll::Timeout)
                        ) {
                            break;
                        }
                    }
                    false
                }
            },
        );
    }

    pub fn output_source(&self) -> Option<u64> {
        self.source
    }

    pub fn events(&self) -> &Receiver<SessionEvent> {
        &self.event_rx
    }

    #[cfg(all(target_os = "linux", feature = "linux-pty"))]
    pub(crate) fn leader_exited(&self) -> bool {
        self.leader_exited.load(Ordering::Acquire)
    }

    #[cfg(all(target_os = "linux", feature = "linux-pty"))]
    pub(crate) fn try_write_owner_prefix(&self, bytes: &[u8]) -> Result<usize, String> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let Some(mut transport) = self.pty.try_lock() else {
            return Ok(0);
        };
        let queue = &self.write_tx.queue;
        if !queue.owner_writes || queue.is_closed() || !(queue.input_admitted)() {
            return Err("PTY input is unavailable".into());
        }
        match transport.write(bytes)? {
            WriteOutcome::Written(written) if (1..=bytes.len()).contains(&written) => Ok(written),
            WriteOutcome::WouldBlock => Ok(0),
            WriteOutcome::Written(_) => Err("invalid PTY write count".into()),
        }
    }

    /// Synchronously observe platform-specific activity without issuing I/O.
    pub fn inspect_transport<R>(&self, inspect: impl FnOnce(&T) -> R) -> R {
        inspect(&self.pty.lock())
    }

    pub fn try_inspect_transport<R>(&self, inspect: impl FnOnce(&T) -> R) -> Option<R> {
        self.pty.try_lock().map(|transport| inspect(&transport))
    }

    /// Publish the final event only after any already-readable exit tail.
    /// Ordinary read errors retain their existing behavior; only a confirmed
    /// Exit enters this byte-bounded drain using the existing nonblocking read.
    /// Sends remain outside locks and retain existing channel backpressure;
    /// neither sends nor read EINTR retries have a new wall-clock deadline.
    fn send_final_io_event(
        pty: &Arc<Mutex<T>>,
        events: &WorkerEvents,
        shutdown: &AtomicBool,
        buffer: &mut [u8],
        source: &mut OutputSource,
        event: SessionEvent,
    ) {
        if matches!(&event, SessionEvent::Exit(_))
            && !drain_exited_pty_output(
                source,
                buffer,
                |buffer| pty.lock().read(buffer).map_err(|error| error.to_string()),
                |output| events.send(output),
                || shutdown.load(Ordering::Relaxed),
            )
        {
            return;
        }
        let _ = events.send(event);
    }

    /// 后台 I/O 循环 - 阻塞等待 PTY 可读，避免忙轮询
    /// P3 优化：批量读取 PTY 数据，累积后一次性发送事件
    fn io_loop(
        pty: Arc<Mutex<T>>,
        events: WorkerEvents,
        descriptor: Arc<OwnedFd>,
        shutdown: Arc<AtomicBool>,
        shutdown_read: OwnedFd,
        write_queue: Arc<WriteQueue>,
        mut source: OutputSource,
    ) {
        const BUFFER_SIZE: usize = 65536;
        const BATCH_SIZE_THRESHOLD: usize = 131072;
        const BATCH_TIMEOUT_MS: u64 = 2;
        const ALIVE_CHECK_MS: u64 = 250;

        let mut buf = vec![0u8; BUFFER_SIZE];
        let mut accumulated = Vec::with_capacity(BATCH_SIZE_THRESHOLD);
        let mut first_read_at = None;
        let mut last_alive_check = std::time::Instant::now();
        let mut last_batch_time = std::time::Instant::now();
        // EOF/EIO means the slave stream is closed, not necessarily that the
        // child has exited. Once set, stop polling the permanently-ready master
        // fd and reap at a bounded cadence instead of emitting a fabricated exit.
        let mut read_closed = false;
        let mut hangup_probe_delay = Duration::from_millis(50);
        // 非 hangup 的重复读取错误只报告一次，直到再次读到数据。
        let mut read_error_reported = false;

        loop {
            // 检查 shutdown 标志
            if shutdown.load(Ordering::Relaxed) {
                return;
            }

            if read_closed {
                if !accumulated.is_empty() {
                    let data = std::mem::take(&mut accumulated);
                    if !events.send(SessionEvent::Output(
                        source.coalesced(data, first_read_at.take()),
                    )) {
                        return;
                    }
                }
                // Hangup can be transient: a live child may later reopen
                // /dev/tty. Probe non-blockingly before reaping so that Data or
                // WouldBlock returns the loop to normal poll-based reading.
                let recovery_probe = {
                    let mut pty_guard = pty.lock();
                    pty_guard.read(&mut buf)
                };
                match recovery_probe {
                    Ok(ReadOutcome::Data(n)) => {
                        if n > 0 && accumulated.is_empty() {
                            first_read_at = Some(std::time::Instant::now());
                        }
                        accumulated.extend_from_slice(&buf[..n]);
                        read_error_reported = false;
                        read_closed = false;
                        hangup_probe_delay = Duration::from_millis(50);
                        continue;
                    }
                    Ok(ReadOutcome::WouldBlock) => {
                        read_error_reported = false;
                        read_closed = false;
                        hangup_probe_delay = Duration::from_millis(50);
                        continue;
                    }
                    Ok(ReadOutcome::Eof | ReadOutcome::Hangup) => {}
                    Err(error) if !read_error_reported => {
                        read_error_reported = true;
                        if !events.send(SessionEvent::Error(format!(
                            "Read error while waiting for PTY recovery: {error}"
                        ))) {
                            return;
                        }
                    }
                    Err(_) => {}
                }
                let exit_code = {
                    let mut pty = pty.lock();
                    let exit = pty.try_reap();
                    if exit.is_some() {
                        write_queue.close();
                    }
                    exit
                };
                if let Some(exit_code) = exit_code {
                    Self::send_final_io_event(
                        &pty,
                        &events,
                        &shutdown,
                        &mut buf,
                        &mut source,
                        SessionEvent::Exit(exit_code),
                    );
                    return;
                }
                // POLLHUP/EIO remains immediately ready forever. Back off to
                // four probes/second for a child that intentionally keeps the
                // slave closed, while retaining fast transient recovery.
                match poll_reader(
                    -1,
                    shutdown_read.as_raw_fd(),
                    hangup_probe_delay.as_millis() as i32,
                ) {
                    Ok(ReaderPoll::Shutdown) => return,
                    Ok(_) => {}
                    Err(error) => {
                        events.send(SessionEvent::Error(format!(
                            "PTY shutdown poll error: {error}"
                        )));
                        return;
                    }
                }
                hangup_probe_delay = std::cmp::min(
                    hangup_probe_delay.saturating_mul(2),
                    Duration::from_millis(250),
                );
                continue;
            }

            // 动态计算超时：累积数据时快速超时，空闲时正常超时
            let timeout_ms = if !accumulated.is_empty() {
                // 已有累积数据，快速超时以便发送
                let elapsed_ms = last_batch_time.elapsed().as_millis() as i32;
                let remaining_ms = BATCH_TIMEOUT_MS as i32 - elapsed_ms;
                remaining_ms.clamp(1, 100)
            } else {
                // 无累积数据，使用标准超时（减去 alive_check 耗时）
                (ALIVE_CHECK_MS as i32)
                    .saturating_sub(last_alive_check.elapsed().as_millis() as i32)
                    .max(1)
            };

            match poll_reader(
                descriptor.as_raw_fd(),
                shutdown_read.as_raw_fd(),
                timeout_ms,
            ) {
                Ok(ReaderPoll::Shutdown) => return,
                Ok(ReaderPoll::Data) => {
                    // 锁内仅读取/累积数据并捕获终止动作;所有 send 移到锁外,
                    // 避免有界 channel 满时持锁阻塞,与 UI 线程 write() 形成死锁。
                    enum After {
                        Continue,
                        ContinueWith(SessionEvent),
                        Stop(SessionEvent),
                        // 流关闭与进程退出是两件事；锁外进入低频 try_reap 状态。
                        ReadClosed,
                    }
                    let after = {
                        let mut pty_guard = pty.lock();
                        let mut after = After::Continue;
                        loop {
                            match pty_guard.read(&mut buf) {
                                Ok(ReadOutcome::Data(n)) => {
                                    read_error_reported = false;
                                    if n > 0 && accumulated.is_empty() {
                                        first_read_at = Some(std::time::Instant::now());
                                    }
                                    accumulated.extend_from_slice(&buf[..n]);
                                    if accumulated.len() >= BATCH_SIZE_THRESHOLD {
                                        break;
                                    }
                                }
                                Ok(ReadOutcome::WouldBlock) => break,
                                Ok(ReadOutcome::Eof) => {
                                    after = After::ReadClosed;
                                    break;
                                }
                                Ok(ReadOutcome::Hangup) => {
                                    // Linux PTY master 的 EIO 表示当前没有 slave fd。
                                    // 子进程仍可能活着，绝不能在这里伪造 Exit(-1)。
                                    after = After::ReadClosed;
                                    break;
                                }
                                Err(e) => {
                                    if !pty_guard.is_alive() {
                                        write_queue.close();
                                        after = After::Stop(match pty_guard.wait_timeout(0) {
                                            Ok(exit_code) => SessionEvent::Exit(exit_code),
                                            Err(e) => SessionEvent::Error(format!(
                                                "Process exit error: {}",
                                                e
                                            )),
                                        });
                                    } else if !read_error_reported {
                                        // 仅首次报告，避免异常 fd 向 UI 持续刷屏。
                                        read_error_reported = true;
                                        after = After::ContinueWith(SessionEvent::Error(format!(
                                            "Read error: {}",
                                            e
                                        )));
                                    }
                                    break;
                                }
                            }
                        }
                        after
                    };
                    // 锁已释放,以下 send 可安全阻塞(背压生效点)
                    if !accumulated.is_empty()
                        && (accumulated.len() >= BATCH_SIZE_THRESHOLD
                            || last_batch_time.elapsed().as_millis() >= BATCH_TIMEOUT_MS as u128
                            || !matches!(after, After::Continue))
                    {
                        let data = std::mem::take(&mut accumulated);
                        if !events.send(SessionEvent::Output(
                            source.coalesced(data, first_read_at.take()),
                        )) {
                            return;
                        }
                        last_batch_time = std::time::Instant::now();
                    }
                    match after {
                        After::Continue => {}
                        After::ContinueWith(ev) => {
                            if !events.send(ev) {
                                return;
                            }
                        }
                        After::Stop(ev) => {
                            Self::send_final_io_event(
                                &pty,
                                &events,
                                &shutdown,
                                &mut buf,
                                &mut source,
                                ev,
                            );
                            return;
                        }
                        After::ReadClosed => {
                            read_closed = true;
                        }
                    }
                }
                Ok(ReaderPoll::Timeout) => {
                    // 超时，检查是否需要发送累积的数据
                    if !accumulated.is_empty()
                        && last_batch_time.elapsed().as_millis() >= BATCH_TIMEOUT_MS as u128
                    {
                        let data = std::mem::take(&mut accumulated);
                        if !events.send(SessionEvent::Output(
                            source.coalesced(data, first_read_at.take()),
                        )) {
                            return;
                        }
                        last_batch_time = std::time::Instant::now();
                    }
                }
                Err(e) => {
                    if !events.send(SessionEvent::Error(format!("Poll error: {}", e))) {
                        return;
                    }
                }
            }

            // 检查进程是否存活（250ms 检查一次，空闲时降低唤醒频率）
            if last_alive_check.elapsed() >= Duration::from_millis(ALIVE_CHECK_MS) {
                // 在锁内判定退出并取退出码,锁外再发送事件(同样为避免持锁阻塞)
                let exit_event = {
                    let mut pty_guard = pty.lock();
                    if !pty_guard.is_alive() {
                        // Retire input while the transport is still locked,
                        // before buffered output can back up on the UI.
                        write_queue.close();
                        Some(match pty_guard.wait_timeout(0) {
                            Ok(exit_code) => SessionEvent::Exit(exit_code),
                            Err(e) => SessionEvent::Error(format!("Process exit error: {}", e)),
                        })
                    } else {
                        None
                    }
                };
                if let Some(exit_event) = exit_event {
                    if !accumulated.is_empty() {
                        let data = std::mem::take(&mut accumulated);
                        let _ = events.send(SessionEvent::Output(
                            source.coalesced(data, first_read_at.take()),
                        ));
                    }
                    Self::send_final_io_event(
                        &pty,
                        &events,
                        &shutdown,
                        &mut buf,
                        &mut source,
                        exit_event,
                    );
                    return;
                }
                last_alive_check = std::time::Instant::now();
            }
        }
    }

    /// 处理大数据写入：循环写入并在 poll 等待时释放锁，避免与 io_loop 死锁。
    /// Normal FIFO writes run only on the writer thread. The optional immediate
    /// one-byte control path excludes queued/in-flight work under the same
    /// queue lock, so it cannot interleave with this loop.
    fn write_to_pty(
        pty: &Arc<Mutex<T>>,
        descriptor: &OwnedFd,
        queue: &WriteQueue,
        data: &[u8],
    ) -> Result<(), String> {
        let mut offset = 0;
        let mut last_progress = std::time::Instant::now();
        let stall_timeout = std::time::Duration::from_secs(10);

        while offset < data.len() {
            if queue.is_closed() {
                return Ok(());
            }
            if last_progress.elapsed() > stall_timeout {
                return Err(format!(
                    "PTY write stalled for 10 seconds, wrote {}/{} bytes",
                    offset,
                    data.len()
                ));
            }

            // 获取锁，尝试写入，然后立即释放锁
            {
                let mut pty = pty.lock();
                if queue.is_closed() {
                    return Ok(());
                }
                match pty.write(&data[offset..]) {
                    Ok(WriteOutcome::Written(n)) if n > 0 => {
                        offset += n;
                        last_progress = std::time::Instant::now();
                        continue; // 写成功(可能 partial)，立刻尝试写剩余部分
                    }
                    Ok(WriteOutcome::Written(_)) => {
                        return Err(format!(
                            "PTY write returned zero after {offset}/{} bytes",
                            data.len()
                        ));
                    }
                    Ok(WriteOutcome::WouldBlock) => {
                        // 缓冲区满，需要 poll 等待可写 — 释放锁（落到下面）
                    }
                    Err(e) => {
                        return Err(format!("Write error: {}", e));
                    }
                }
            }
            // 锁已释放！io_loop 可以读取 PTY 输出，vim 可以排空缓冲区
            // poll 等待 PTY 可写
            let mut pfd = libc::pollfd {
                fd: descriptor.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            loop {
                // SAFETY: pfd 是有效的栈变量，fd 在 Pty Arc 存活期间保持打开。
                let result = unsafe { libc::poll(&mut pfd, 1, 50) };
                if result >= 0 || queue.is_closed() {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(format!("poll(POLLOUT) failed: {error}"));
                }
            }
            if pfd.revents & libc::POLLOUT == 0
                && pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
            {
                return Err(format!(
                    "PTY became unavailable during write after {offset}/{} bytes (revents={:#x})",
                    data.len(),
                    pfd.revents
                ));
            }
        }
        Ok(())
    }

    /// 向 shell 非阻塞发送输入。Ok 表示整条消息已入 FIFO；Full/TooLarge/Closed
    /// 都保证一个字节也没有入队，调用方可以安全保留原输入后重试。
    pub fn write(&self, data: &[u8]) -> Result<(), WriteError> {
        self.write_tx
            .queue
            .try_enqueue_input(WriteOrigin::User, data.into())
    }

    pub fn write_response(&self, data: Vec<u8>) -> Result<(), WriteError> {
        self.write_tx
            .queue
            .try_enqueue_input(WriteOrigin::Reply, data.into())
    }

    pub fn can_queue_user_input(&self, len: usize) -> Result<(), WriteError> {
        let queue = &self.write_tx.queue;
        queue
            .admission(&queue.state.lock(), WriteOrigin::User, len)
            .map(|_| ())
    }

    pub fn input_backlog(&self) -> Result<InputBacklog, WriteError> {
        let queue = &self.write_tx.queue;
        let state = queue.state.lock();
        if queue.is_closed() {
            return Err(WriteError::Closed);
        }
        Ok(InputBacklog {
            entries: state.accounted_messages,
            user_bytes: state.accounted_bytes - state.reply_bytes,
            response_bytes: state.reply_bytes,
        })
    }

    /// Permanently reject pending and future writes without dropping output
    /// or transferring child ownership.
    pub fn close_input(&self) {
        self.write_tx.queue.close();
    }

    /// True only after every queued and in-flight PTY write has completed.
    pub fn input_drained(&self) -> Result<bool, WriteError> {
        self.write_tx.queue.drained()
    }

    /// Observe the actual PTY foreground owner for a rendered Chat gesture.
    /// Missing or contended native state disables the optional control.
    pub fn foreground_pgid(&self) -> Option<i32> {
        let pty = self.pty.try_lock()?;
        // SAFETY: the live Pty guard pins the master descriptor.
        let pgid = unsafe { libc::tcgetpgrp(pty.as_fd().as_raw_fd()) };
        (pgid > 0).then_some(pgid)
    }

    /// Single-byte, non-retrying control admission on the existing writer's
    /// synchronization boundary. Holding queue state excludes every producer
    /// and proves there is no queued/in-flight writer. Never enqueue a Chat
    /// control for later delivery, and never partially send an escape sequence.
    pub fn try_write_control_byte(&self, byte: u8, validate: impl FnOnce(i32) -> bool) -> bool {
        let Some(state) = self.write_tx.queue.state.try_lock() else {
            return false;
        };
        if self.write_tx.queue.owner_writes
            || self.write_tx.queue.is_closed()
            || state.retiring
            || state.accounted_messages != 0
            || !(self.write_tx.queue.input_admitted)()
        {
            return false;
        }
        let Some(mut pty) = self.pty.try_lock() else {
            return false;
        };
        if !pty.control_input_available() {
            return false;
        }
        // The constructor's best-effort flag setup is not sufficient for an
        // immediate UI-thread write. Fail closed unless this live descriptor
        // can be confirmed nonblocking, before invoking lifecycle validation.
        // SAFETY: the live Pty guard pins the descriptor through the write.
        let flags = unsafe { libc::fcntl(pty.as_fd().as_raw_fd(), libc::F_GETFL) };
        if flags < 0 || flags & libc::O_NONBLOCK == 0 {
            return false;
        }
        // SAFETY: the live Pty guard pins the master descriptor. Foreground
        // validation and the one-byte nonblocking write share this boundary.
        let foreground = unsafe { libc::tcgetpgrp(pty.as_fd().as_raw_fd()) };
        if foreground <= 0 || !validate(foreground) {
            return false;
        }
        pty.write_control_byte(byte)
    }

    /// 返回写入队列的发送端,供需要先做阻塞 I/O(如读剪贴板)再写 PTY 的后台线程使用。
    /// 经由它入队的数据同样被 writer 线程串行写入,保证不与其他写入交错。
    pub fn write_sender(&self) -> WriteSender {
        self.write_tx.clone()
    }

    /// Stop every producer after the child exit event has been consumed while
    /// keeping the already-rendered terminal transcript alive. Closing the
    /// queue wakes an idle writer thread and makes every outstanding sender
    /// fail closed; draining discards only events ordered after the observed
    /// `Exit` (for example, a racing writer error).
    pub fn freeze_after_exit(&self) {
        self.stop_workers();
        while self.event_rx.try_recv().is_ok() {}
    }

    pub fn resize(&self, cols: usize, rows: usize) -> std::result::Result<(), String> {
        let mut pty = self.pty.lock();
        if self.shutdown.load(Ordering::Acquire) {
            return Err("PTY session has stopped".into());
        }
        pty.resize(cols, rows)
            .map_err(|e| format!("Resize error: {}", e))
    }

    fn stop_workers(&self) {
        self.write_tx.queue.close();
        self.shutdown.store(true, Ordering::Release);
        let mut stop = self.stop.lock();
        stop.events.take();
        stop.reader.take();
    }

    /// Retire I/O immediately and let the actor finish the native shutdown.
    pub fn stop(&self) {
        self.stop_after_pending_input(Duration::ZERO);
    }

    /// Retire further admission and allow a bounded final write attempt.
    /// Used for input such as a pane's closing mouse release.
    pub fn stop_after_pending_input(&self, wait: Duration) {
        if self.teardown_started.swap(true, Ordering::AcqRel) {
            return;
        }
        self.write_tx.queue.retire_and_drain(wait);
        self.stop_workers();
        // Deliver the initial native termination signal before returning so a
        // host about to exit cannot take it down with the cleanup worker.
        let grace = {
            let mut transport = self.pty.lock();
            if transport.handoff_cleanup() {
                return;
            }
            transport.signal_terminate();
            transport.termination_grace()
        };
        let pty = self.pty.clone();
        if let Err(error) = thread::Builder::new()
            .name("pty-reaper".into())
            .spawn(move || {
                thread::sleep(grace);
                pty.lock().force_kill_and_reap();
            })
        {
            log::error!("Failed to start PTY cleanup worker: {error}");
            self.pty.lock().force_kill_and_reap();
        }
    }

    #[cfg(feature = "session-actor-test-hooks")]
    #[doc(hidden)]
    pub fn test_transport(&self) -> parking_lot::MutexGuard<'_, T> {
        self.pty.lock()
    }

    #[cfg(feature = "session-actor-test-hooks")]
    #[doc(hidden)]
    pub fn test_input_counts(&self) -> (usize, usize) {
        let state = self.write_tx.queue.state.lock();
        (state.pending.len(), state.accounted_messages)
    }

    #[cfg(feature = "session-actor-test-hooks")]
    #[doc(hidden)]
    pub fn test_close_input(&self) {
        self.write_tx.queue.close();
    }
}

impl<T: SessionTransport> Drop for SessionActor<T> {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
