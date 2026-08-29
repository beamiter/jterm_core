//! Toolkit-independent multi-chat runtime state for the family's AI panels.
//!
//! Every jterm terminal grew its own copy of this state machine on top of the
//! shared [`ConversationSnapshot`] schema: anvil `src/dialogs/ai_chat_store.rs`,
//! forge `src/ui/ai_chat_store.rs`, ember `src/ai_chat_store.rs` and frost
//! `src/ai_chat_store.rs`. The copies contained no toolkit code at all — the
//! only thing they ever imported was `jterm_core::ai` — so they were free to
//! drift, and they did. This module is their union, and the four apps keep
//! only a thin shim.
//!
//! The store owns each chat's provider history, Block context, draft, archive
//! state, streamed partial and request token. Completion is keyed by
//! `(chat_id, epoch)`, so a background reply can never cross into whichever
//! chat the user happens to be viewing.
//!
//! # What each lineage contributed
//!
//! forge's copy was the hardened one and forms the base: a global live-history
//! byte budget with real compaction, persistence that compacts *before*
//! serialising and syncs truncation markers back, typed archive/delete
//! outcomes, an at-capacity guard so archiving cannot mutate-then-fail, and
//! draft merges that report what they dropped.
//!
//! anvil's copy (which ember and frost were ported from) contributed the
//! in-store streaming state — [`ChatStore::push_delta`] and
//! [`ChatStore::active_partial`] — plus query filtering on the chat library
//! and the prefix-idempotence rule in draft merging, without which a recovered
//! retry duplicates its own question every time it is persisted.
//!
//! # Bounds
//!
//! Live state is bounded per message (64 KiB), per assistant reply (256 KiB),
//! per chat (100 turns) and across the whole library (8 MiB). The library-wide
//! bound is the one that matters for persistence: without it the live state can
//! reach a size [`ConversationSnapshot::from_chats`] refuses, at which point
//! nothing can be saved at all.

use crate::ai::{
    BlockContext, ChatSnapshot, ConversationSnapshot, Role, Turn, MAX_PERSISTED_CHATS,
};

pub const DEFAULT_CHAT_TITLE: &str = "New chat";
/// Budget for one user message, one draft, and a merged draft.
pub const MAX_LIVE_MESSAGE_BYTES: usize = 64 * 1024;
/// Budget for one assistant reply, live and streamed.
pub const MAX_LIVE_ASSISTANT_MESSAGE_BYTES: usize = 256 * 1024;
const MAX_LIVE_TURNS_PER_CHAT: usize = 100;
const MAX_LIVE_ALL_HISTORY_BYTES: usize = 8 * 1024 * 1024;
const MAX_CHAT_TITLE_BYTES: usize = 256;
const MAX_CHAT_TITLE_CHARS: usize = 80;
/// Titles derived from the first message are shortened further than a title
/// the user typed, so the library rows stay scannable.
const DERIVED_TITLE_CHARS: usize = 52;
const PREVIEW_CHARS: usize = 72;
/// Untrusted preview text is sanitised before this much of it is inspected.
const PREVIEW_SOURCE_BYTES: usize = 16 * 1024;

/// What archiving or deleting a chat does while that chat has a request in
/// flight.
///
/// The apps genuinely differ here and both behaviours are correct for their
/// panel, so this is a construction-time choice rather than a silent default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BusyChatPolicy {
    /// Refuse with [`ChatStoreError::Busy`]. Correct when the panel has no
    /// cancel-then-mutate step of its own (anvil, ember, frost).
    #[default]
    Refuse,
    /// Proceed; the caller has already cancelled the in-flight request. The
    /// request's late reply is still discarded, because its epoch no longer
    /// matches (forge).
    Allow,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ChatStatus {
    #[default]
    Idle,
    Thinking(String),
    Info(String),
    Error(String),
}

/// Identity of one request. A reply is applied only while the owning chat is
/// still on exactly this epoch.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct RequestToken {
    pub chat_id: u64,
    pub epoch: u64,
}

/// Everything the caller needs to issue the request it just started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestStart {
    pub token: RequestToken,
    pub history: Vec<Turn>,
    pub effective_context: Option<BlockContext>,
}

/// One row of the chat library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatSummary {
    pub id: u64,
    pub title: String,
    pub preview: String,
    pub archived: bool,
    pub active: bool,
    pub busy: bool,
    pub unread: bool,
    pub error: bool,
    pub history_truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatStoreError {
    LimitReached,
    Archived,
    Busy,
    EmptyMessage,
    MessageTooLarge,
    SnapshotInvalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveOutcome {
    pub archived: bool,
    pub active_chat_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeleteOutcome {
    pub deleted_chat_id: u64,
    pub active_chat_id: u64,
}

#[derive(Clone, Debug)]
struct ChatRuntime {
    id: u64,
    title: String,
    archived: bool,
    history: Vec<Turn>,
    block_context: Option<BlockContext>,
    draft: String,
    history_truncated: bool,
    epoch: u64,
    active_epoch: Option<u64>,
    pending_user: Option<String>,
    restore_pending_as_draft: bool,
    /// `None` means the active request did not replace context. `Some` keeps
    /// the last durable context (which can itself be absent) until success.
    previous_context: Option<Option<BlockContext>>,
    /// Streamed assistant text for the in-flight request. Never persisted:
    /// a partial reply is not a turn.
    partial: String,
    status: ChatStatus,
    unread: bool,
}

impl ChatRuntime {
    fn empty(id: u64) -> Self {
        Self {
            id,
            title: DEFAULT_CHAT_TITLE.to_string(),
            archived: false,
            history: Vec::new(),
            block_context: None,
            draft: String::new(),
            history_truncated: false,
            epoch: 0,
            active_epoch: None,
            pending_user: None,
            restore_pending_as_draft: false,
            previous_context: None,
            partial: String::new(),
            status: ChatStatus::Idle,
            unread: false,
        }
    }

    fn from_snapshot(snapshot: ChatSnapshot) -> Self {
        let (id, title, archived, history, block_context, draft, history_truncated) =
            snapshot.into_parts();
        Self {
            id,
            title,
            archived,
            history,
            block_context,
            draft: bounded_live_message(draft),
            history_truncated,
            ..Self::empty(id)
        }
    }

    fn is_busy(&self) -> bool {
        self.active_epoch.is_some()
    }
}

/// Multi-chat runtime state over the durable [`ConversationSnapshot`] schema.
#[derive(Clone, Debug)]
pub struct ChatStore {
    /// Oldest to newest. Persistence compacts payload from the front while the
    /// library presents this vector in reverse order.
    chats: Vec<ChatRuntime>,
    active_chat_id: u64,
    next_id: u64,
    busy_policy: BusyChatPolicy,
}

impl Default for ChatStore {
    fn default() -> Self {
        Self {
            chats: vec![ChatRuntime::empty(1)],
            active_chat_id: 1,
            next_id: 2,
            busy_policy: BusyChatPolicy::default(),
        }
    }
}

impl ChatStore {
    /// An empty library whose archive/delete follow `policy`.
    pub fn with_busy_policy(policy: BusyChatPolicy) -> Self {
        Self {
            busy_policy: policy,
            ..Self::default()
        }
    }

    /// Restore a persisted library. The snapshot's structural invariants
    /// (non-empty, unique ids, an `active_chat_id` that is present) are
    /// guaranteed by [`ConversationSnapshot`] itself, which is what lets the
    /// active-chat accessors be infallible.
    pub fn restore(snapshot: ConversationSnapshot) -> Self {
        Self::restore_with_busy_policy(snapshot, BusyChatPolicy::default())
    }

    pub fn restore_with_busy_policy(
        snapshot: ConversationSnapshot,
        policy: BusyChatPolicy,
    ) -> Self {
        let (active_chat_id, snapshots) = snapshot.into_parts();
        let chats: Vec<_> = snapshots
            .into_iter()
            .map(ChatRuntime::from_snapshot)
            .collect();
        let next_id = next_available_id(&chats);
        let mut store = Self {
            chats,
            active_chat_id,
            next_id,
            busy_policy: policy,
        };
        store.active_mut().unread = false;
        store
    }

    pub fn busy_policy(&self) -> BusyChatPolicy {
        self.busy_policy
    }

    /// Compact live state to what persistence can accept, then serialise it.
    ///
    /// Takes `&mut self` deliberately: compacting first is what keeps the
    /// store out of a state where nothing can be saved. The returned flag says
    /// whether a chat gained a truncation marker, so the caller can re-render
    /// the library.
    pub fn snapshot_for_persistence(
        &mut self,
        redact: bool,
    ) -> Result<(ConversationSnapshot, bool), ChatStoreError> {
        self.compact_live_histories();
        let chats = self
            .chats
            .iter()
            .map(|chat| {
                let mut title = chat.title.clone();
                let mut history = chat.history.clone();
                let mut context = durable_context(chat);
                let (mut draft, draft_truncated) = durable_draft(chat);
                if redact {
                    title = crate::redact::redact_secrets(&title);
                    draft = crate::redact::redact_secrets(&draft);
                    for turn in &mut history {
                        turn.text = crate::redact::redact_secrets(&turn.text);
                    }
                    if let Some(context) = context.as_mut() {
                        context.cmd = crate::redact::redact_secrets(&context.cmd);
                        context.output = crate::redact::redact_secrets(&context.output);
                        context.cwd = context
                            .cwd
                            .take()
                            .map(|cwd| crate::redact::redact_secrets(&cwd));
                    }
                }
                ChatSnapshot::from_completed_history(
                    chat.id,
                    &title,
                    chat.archived,
                    &history,
                    context.as_ref(),
                    &draft,
                )
                .with_history_truncated(chat.history_truncated || draft_truncated)
            })
            .collect();
        let snapshot = ConversationSnapshot::from_chats(self.active_chat_id, chats)
            .ok_or(ChatStoreError::SnapshotInvalid)?;
        let truncation_changed = self.sync_truncation_markers(&snapshot);
        Ok((snapshot, truncation_changed))
    }

    /// Carry truncation markers a persistence pass applied back into the live
    /// chats, so the library shows what was dropped.
    pub fn sync_truncation_markers(&mut self, snapshot: &ConversationSnapshot) -> bool {
        let mut changed = false;
        for persisted in snapshot.chats() {
            if !persisted.history_truncated() {
                continue;
            }
            if let Some(chat) = self.chat_mut(persisted.id()) {
                changed |= !chat.history_truncated;
                chat.history_truncated = true;
            }
        }
        changed
    }

    pub fn active_id(&self) -> u64 {
        self.active_chat_id
    }

    pub fn active_title(&self) -> &str {
        &self.active().title
    }

    pub fn active_archived(&self) -> bool {
        self.active().archived
    }

    pub fn active_history(&self) -> &[Turn] {
        &self.active().history
    }

    pub fn active_context(&self) -> Option<&BlockContext> {
        self.active().block_context.as_ref()
    }

    pub fn active_draft(&self) -> &str {
        &self.active().draft
    }

    /// Streamed text for the active chat's in-flight request, empty when no
    /// request is streaming.
    pub fn active_partial(&self) -> &str {
        &self.active().partial
    }

    pub fn active_status(&self) -> &ChatStatus {
        &self.active().status
    }

    pub fn active_history_truncated(&self) -> bool {
        self.active().history_truncated
    }

    pub fn is_active_busy(&self) -> bool {
        self.active().is_busy()
    }

    pub fn active_request_token(&self) -> Option<RequestToken> {
        let chat = self.active();
        chat.active_epoch.map(|epoch| RequestToken {
            chat_id: chat.id,
            epoch,
        })
    }

    /// Tokens for every chat with a request in flight, so a caller tearing the
    /// panel down can cancel all of them.
    pub fn in_flight_tokens(&self) -> Vec<RequestToken> {
        self.chats
            .iter()
            .filter_map(|chat| {
                chat.active_epoch.map(|epoch| RequestToken {
                    chat_id: chat.id,
                    epoch,
                })
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.chats.len()
    }

    pub fn is_empty(&self) -> bool {
        // A library always holds at least one chat; the accessor exists so
        // `len` does not trip Clippy's `len_without_is_empty`.
        self.chats.is_empty()
    }

    pub fn at_capacity(&self) -> bool {
        self.chats.len() >= MAX_PERSISTED_CHATS
    }

    pub fn set_active_draft(&mut self, draft: String) -> bool {
        let draft = bounded_live_message(draft);
        if self.active().draft == draft {
            return false;
        }
        self.active_mut().draft = draft;
        true
    }

    pub fn clear_active_context(&mut self) -> Result<bool, ChatStoreError> {
        if self.active().is_busy() {
            return Err(ChatStoreError::Busy);
        }
        Ok(self.active_mut().block_context.take().is_some())
    }

    pub fn new_chat(&mut self) -> Result<u64, ChatStoreError> {
        if self.at_capacity() {
            return Err(ChatStoreError::LimitReached);
        }
        let id = self.allocate_id();
        self.chats.push(ChatRuntime::empty(id));
        self.active_chat_id = id;
        Ok(id)
    }

    pub fn select_chat(&mut self, id: u64) -> bool {
        if self.active_chat_id == id || !self.chats.iter().any(|chat| chat.id == id) {
            return false;
        }
        self.active_chat_id = id;
        self.active_mut().unread = false;
        true
    }

    pub fn rename_active(&mut self, title: &str) -> bool {
        let title = normalise_title(title);
        if self.active().title == title {
            return false;
        }
        self.active_mut().title = title;
        true
    }

    /// Archive the active chat (or un-archive it), selecting a writable
    /// replacement.
    ///
    /// Every refusal is checked *before* the chat is mutated, so a rejected
    /// archive leaves no half-applied state behind.
    pub fn toggle_archive_active(&mut self) -> Result<ArchiveOutcome, ChatStoreError> {
        if self.active().archived {
            self.active_mut().archived = false;
            return Ok(ArchiveOutcome {
                archived: false,
                active_chat_id: self.active_chat_id,
            });
        }
        if self.busy_policy == BusyChatPolicy::Refuse && self.active().is_busy() {
            return Err(ChatStoreError::Busy);
        }

        let archived_id = self.active_chat_id;
        let replacement = self
            .chats
            .iter()
            .rev()
            .find(|chat| chat.id != archived_id && !chat.archived)
            .map(|chat| chat.id);
        // Refuse before mutating: archiving the last writable chat at capacity
        // cannot allocate a replacement, and reporting that after setting
        // `archived` would leave the library with no writable chat.
        if replacement.is_none() && self.at_capacity() {
            return Err(ChatStoreError::LimitReached);
        }

        self.active_mut().archived = true;
        if let Some(replacement) = replacement {
            self.active_chat_id = replacement;
            self.active_mut().unread = false;
        } else {
            self.new_chat()?;
        }
        Ok(ArchiveOutcome {
            archived: true,
            active_chat_id: self.active_chat_id,
        })
    }

    /// Delete the active chat, always leaving a writable one selected.
    pub fn delete_active(&mut self) -> Result<DeleteOutcome, ChatStoreError> {
        if self.busy_policy == BusyChatPolicy::Refuse && self.active().is_busy() {
            return Err(ChatStoreError::Busy);
        }
        let deleted_chat_id = self.active_chat_id;
        self.chats.retain(|chat| chat.id != deleted_chat_id);

        if let Some(replacement) = self
            .chats
            .iter()
            .rev()
            .find(|chat| !chat.archived)
            .map(|chat| chat.id)
        {
            self.active_chat_id = replacement;
            self.active_mut().unread = false;
        } else {
            // Deletion always frees one slot, so a writable replacement is
            // guaranteed even if every surviving chat is archived.
            let id = self.allocate_id();
            self.chats.push(ChatRuntime::empty(id));
            self.active_chat_id = id;
        }

        Ok(DeleteOutcome {
            deleted_chat_id,
            active_chat_id: self.active_chat_id,
        })
    }

    pub fn begin_turn(
        &mut self,
        text: String,
        context: Option<BlockContext>,
        thinking_message: String,
        restore_pending_as_draft: bool,
    ) -> Result<RequestStart, ChatStoreError> {
        if text.trim().is_empty() {
            return Err(ChatStoreError::EmptyMessage);
        }
        if text.len() > MAX_LIVE_MESSAGE_BYTES {
            return Err(ChatStoreError::MessageTooLarge);
        }
        if self.active().archived {
            return Err(ChatStoreError::Archived);
        }
        if self.active().is_busy() {
            return Err(ChatStoreError::Busy);
        }

        let chat = self.active_mut();
        chat.previous_context = context.as_ref().map(|_| chat.block_context.clone());
        if let Some(context) = context {
            chat.block_context = Some(context);
        }
        let effective_context = chat.block_context.clone();
        if chat.title == DEFAULT_CHAT_TITLE && chat.history.is_empty() {
            chat.title = title_from_text(&text);
        }
        chat.epoch = chat.epoch.wrapping_add(1);
        let token = RequestToken {
            chat_id: chat.id,
            epoch: chat.epoch,
        };
        chat.history.push(Turn {
            role: Role::User,
            text: text.clone(),
        });
        chat.active_epoch = Some(token.epoch);
        chat.pending_user = Some(text);
        chat.restore_pending_as_draft = restore_pending_as_draft;
        chat.partial.clear();
        chat.status = ChatStatus::Thinking(thinking_message);
        chat.unread = false;

        Ok(RequestStart {
            token,
            history: chat.history.clone(),
            effective_context,
        })
    }

    /// Append streamed assistant text to the owning chat's partial reply.
    ///
    /// Returns `None` when the token is stale — the chat was deleted, or a
    /// newer request replaced this one — and `Some(visible)` telling the
    /// caller whether the owning chat is the one on screen.
    pub fn push_delta(&mut self, token: RequestToken, text: &str) -> Option<bool> {
        let active_chat_id = self.active_chat_id;
        let chat = self.chat_mut(token.chat_id)?;
        if chat.active_epoch != Some(token.epoch) {
            return None;
        }
        let room = MAX_LIVE_ASSISTANT_MESSAGE_BYTES.saturating_sub(chat.partial.len());
        if room > 0 {
            let mut end = text.len().min(room);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            chat.partial.push_str(&text[..end]);
        }
        Some(chat.id == active_chat_id)
    }

    /// Returns whether the owner chat is still the visible chat.
    pub fn complete_success(&mut self, token: RequestToken, text: String) -> Option<bool> {
        let active_chat_id = self.active_chat_id;
        let response_truncated = text.len() > MAX_LIVE_ASSISTANT_MESSAGE_BYTES;
        let text = bounded_live_assistant_message(text);
        let owner_active = {
            let chat = self.chat_mut(token.chat_id)?;
            if chat.active_epoch != Some(token.epoch) {
                return None;
            }
            chat.active_epoch = None;
            chat.pending_user = None;
            chat.restore_pending_as_draft = false;
            chat.previous_context = None;
            chat.partial.clear();
            chat.history.push(Turn {
                role: Role::Assistant,
                text,
            });
            chat.history_truncated |= response_truncated;
            chat.status = ChatStatus::Idle;
            chat.unread = chat.id != active_chat_id;
            chat.id == active_chat_id
        };
        self.compact_live_histories();
        Some(owner_active)
    }

    /// Roll back only the request owner's trailing user turn.
    pub fn complete_error(&mut self, token: RequestToken, message: String) -> Option<bool> {
        self.rollback(token, message, ChatStatus::Error)
    }

    pub fn cancel_request(&mut self, token: RequestToken, message: String) -> Option<bool> {
        self.rollback(token, message, ChatStatus::Info)
    }

    fn rollback(
        &mut self,
        token: RequestToken,
        message: String,
        status: fn(String) -> ChatStatus,
    ) -> Option<bool> {
        let active_chat_id = self.active_chat_id;
        let chat = self.chat_mut(token.chat_id)?;
        if chat.active_epoch != Some(token.epoch) {
            return None;
        }
        let draft_truncated = rollback_pending_request(chat);
        let message = if draft_truncated {
            format!("{message} Some recovered draft text was omitted at the 64 KiB limit.")
        } else {
            message
        };
        chat.status = status(message);
        chat.unread = chat.id != active_chat_id;
        Some(chat.id == active_chat_id)
    }

    fn compact_live_histories(&mut self) {
        for chat in &mut self.chats {
            while chat.history.len() > MAX_LIVE_TURNS_PER_CHAT {
                if !drop_oldest_live_pair(chat) {
                    break;
                }
            }
        }
        while live_history_bytes(&self.chats) > MAX_LIVE_ALL_HISTORY_BYTES {
            let active_id = self.active_chat_id;
            let candidate = self
                .chats
                .iter()
                .position(|chat| chat.id != active_id && has_oldest_complete_pair(chat))
                .or_else(|| {
                    self.chats
                        .iter()
                        .position(|chat| chat.id == active_id && has_oldest_complete_pair(chat))
                });
            let Some(index) = candidate else {
                break;
            };
            // The drop must run in release builds too — wrapping the call in
            // `debug_assert!` compiled it away and left this loop spinning
            // forever once the byte budget was exceeded.
            if !drop_oldest_live_pair(&mut self.chats[index]) {
                debug_assert!(false, "candidate chat lost its complete pair");
                break;
            }
        }
    }

    /// Materialise a memory-only retry as durable draft/context state.
    ///
    /// Refuses while the chat is busy. Use
    /// [`ChatStore::recover_retry_payload_detaching`] on a clone at shutdown,
    /// when the in-flight request is about to die with the process.
    pub fn recover_retry_payload(
        &mut self,
        chat_id: u64,
        user_text: &str,
        context: Option<BlockContext>,
    ) -> bool {
        let Some(chat) = self.chat_mut(chat_id) else {
            return false;
        };
        if chat.is_busy() {
            return false;
        }
        Self::apply_retry_payload(chat, user_text, context);
        true
    }

    /// Detach any in-flight request and materialise the retry regardless.
    ///
    /// Intended for a clone taken at teardown: the live composer keeps its own
    /// unrelated draft while a selected-Block request is still running, and
    /// only the clone is persisted.
    pub fn recover_retry_payload_detaching(
        &mut self,
        chat_id: u64,
        user_text: &str,
        context: Option<BlockContext>,
    ) -> bool {
        let Some(chat) = self.chat_mut(chat_id) else {
            return false;
        };
        if chat.is_busy()
            && chat
                .history
                .last()
                .is_some_and(|turn| turn.role == Role::User)
        {
            chat.history.pop();
        }
        chat.active_epoch = None;
        chat.pending_user = None;
        chat.previous_context = None;
        chat.partial.clear();
        chat.restore_pending_as_draft = false;
        Self::apply_retry_payload(chat, user_text, context);
        true
    }

    fn apply_retry_payload(chat: &mut ChatRuntime, user_text: &str, context: Option<BlockContext>) {
        if !user_text.trim().is_empty() {
            let (draft, truncated) = merge_drafts_bounded(user_text, &chat.draft);
            chat.draft = draft;
            chat.history_truncated |= truncated;
        }
        if let Some(context) = context {
            chat.block_context = Some(context);
        }
    }

    pub fn set_active_info(&mut self, message: impl Into<String>) {
        self.active_mut().status = ChatStatus::Info(message.into());
    }

    pub fn set_active_error(&mut self, message: impl Into<String>) {
        self.active_mut().status = ChatStatus::Error(message.into());
    }

    pub fn clear_active_status(&mut self) {
        if !self.active().is_busy() {
            self.active_mut().status = ChatStatus::Idle;
        }
    }

    /// The whole library, newest first.
    pub fn summaries(&self) -> Vec<ChatSummary> {
        self.summaries_filtered("")
    }

    /// The library filtered by a case-insensitive substring over title and
    /// preview, newest first. An empty or whitespace-only query matches every
    /// chat.
    pub fn summaries_filtered(&self, query: &str) -> Vec<ChatSummary> {
        let query = query.trim().to_lowercase();
        self.chats
            .iter()
            .rev()
            .filter_map(|chat| {
                let preview = chat_preview(chat);
                if !query.is_empty()
                    && !chat.title.to_lowercase().contains(&query)
                    && !preview.to_lowercase().contains(&query)
                {
                    return None;
                }
                Some(ChatSummary {
                    id: chat.id,
                    title: chat.title.clone(),
                    preview,
                    archived: chat.archived,
                    active: chat.id == self.active_chat_id,
                    busy: chat.is_busy(),
                    unread: chat.unread,
                    error: matches!(chat.status, ChatStatus::Error(_)),
                    history_truncated: chat.history_truncated,
                })
            })
            .collect()
    }

    fn active(&self) -> &ChatRuntime {
        self.chats
            .iter()
            .find(|chat| chat.id == self.active_chat_id)
            .expect("active chat invariant")
    }

    fn active_mut(&mut self) -> &mut ChatRuntime {
        let id = self.active_chat_id;
        self.chat_mut(id).expect("active chat invariant")
    }

    fn chat_mut(&mut self, id: u64) -> Option<&mut ChatRuntime> {
        self.chats.iter_mut().find(|chat| chat.id == id)
    }

    fn allocate_id(&mut self) -> u64 {
        let mut candidate = self.next_id.max(1);
        while self.chats.iter().any(|chat| chat.id == candidate) {
            candidate = candidate.wrapping_add(1).max(1);
        }
        self.next_id = candidate.wrapping_add(1).max(1);
        candidate
    }
}

fn next_available_id(chats: &[ChatRuntime]) -> u64 {
    let mut candidate = chats
        .iter()
        .map(|chat| chat.id)
        .max()
        .unwrap_or(0)
        .wrapping_add(1)
        .max(1);
    while chats.iter().any(|chat| chat.id == candidate) {
        candidate = candidate.wrapping_add(1).max(1);
    }
    candidate
}

fn normalise_title(title: &str) -> String {
    let collapsed = title
        .chars()
        .map(|ch| {
            if ch.is_control() || ch.is_whitespace() {
                ' '
            } else if crate::review_input::is_visual_spoofing_character(ch) {
                '\u{fffd}'
            } else {
                ch
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut bounded = String::new();
    for ch in collapsed.chars().take(MAX_CHAT_TITLE_CHARS) {
        if bounded.len().saturating_add(ch.len_utf8()) > MAX_CHAT_TITLE_BYTES {
            break;
        }
        bounded.push(ch);
    }
    if bounded.is_empty() {
        DEFAULT_CHAT_TITLE.into()
    } else {
        bounded
    }
}

fn title_from_text(text: &str) -> String {
    let title = normalise_title(text);
    let mut chars = title.chars();
    let short: String = chars.by_ref().take(DERIVED_TITLE_CHARS).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

/// The library row's second line. The source is model or terminal output, so
/// it is sanitised before it can reach a widget.
fn chat_preview(chat: &ChatRuntime) -> String {
    let source = chat
        .history
        .last()
        .map(|turn| turn.text.as_str())
        .filter(|text| !text.trim().is_empty())
        .or_else(|| (!chat.draft.trim().is_empty()).then_some(chat.draft.as_str()))
        .unwrap_or("Empty conversation");
    let source = crate::review_input::safe_inline_display(source, PREVIEW_SOURCE_BYTES);
    let collapsed = source.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let preview: String = chars.by_ref().take(PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn durable_draft(chat: &ChatRuntime) -> (String, bool) {
    if chat.restore_pending_as_draft {
        if let Some(pending_user) = chat.pending_user.as_deref() {
            return merge_drafts_bounded(pending_user, &chat.draft);
        }
    }
    (chat.draft.clone(), false)
}

fn durable_context(chat: &ChatRuntime) -> Option<BlockContext> {
    chat.previous_context
        .as_ref()
        .cloned()
        .unwrap_or_else(|| chat.block_context.clone())
}

fn rollback_pending_request(chat: &mut ChatRuntime) -> bool {
    let mut draft_truncated = false;
    chat.active_epoch = None;
    chat.partial.clear();
    let popped_user = if chat
        .history
        .last()
        .is_some_and(|turn| turn.role == Role::User)
    {
        chat.history.pop().map(|turn| turn.text)
    } else {
        None
    };
    let pending_user = chat.pending_user.take().or(popped_user);
    if chat.restore_pending_as_draft {
        if let Some(pending_user) = pending_user {
            let (draft, truncated) = merge_drafts_bounded(&pending_user, &chat.draft);
            chat.draft = draft;
            chat.history_truncated |= truncated;
            draft_truncated = truncated;
        }
    }
    chat.restore_pending_as_draft = false;
    if let Some(previous_context) = chat.previous_context.take() {
        chat.block_context = previous_context;
    }
    draft_truncated
}

fn has_oldest_complete_pair(chat: &ChatRuntime) -> bool {
    matches!(
        chat.history.as_slice(),
        [
            Turn {
                role: Role::User,
                ..
            },
            Turn {
                role: Role::Assistant,
                ..
            },
            ..
        ]
    )
}

fn drop_oldest_live_pair(chat: &mut ChatRuntime) -> bool {
    if !has_oldest_complete_pair(chat) {
        return false;
    }
    chat.history.drain(..2);
    chat.history_truncated = true;
    if chat.history.is_empty() {
        chat.block_context = None;
    }
    true
}

fn live_history_bytes(chats: &[ChatRuntime]) -> usize {
    chats.iter().fold(0_usize, |total, chat| {
        chat.history
            .iter()
            .fold(total, |total, turn| total.saturating_add(turn.text.len()))
    })
}

/// Trim to the live message budget on a UTF-8 boundary.
pub fn bounded_live_message(mut text: String) -> String {
    if text.len() <= MAX_LIVE_MESSAGE_BYTES {
        return text;
    }
    let mut end = MAX_LIVE_MESSAGE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

fn bounded_live_assistant_message(mut text: String) -> String {
    const NOTICE: &str = "\n\n[Response truncated to the 256 KiB live message limit.]";
    if text.len() <= MAX_LIVE_ASSISTANT_MESSAGE_BYTES {
        return text;
    }
    let mut end = MAX_LIVE_ASSISTANT_MESSAGE_BYTES.saturating_sub(NOTICE.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(NOTICE);
    text
}

fn append_bounded(target: &mut String, text: &str) {
    let remaining = MAX_LIVE_MESSAGE_BYTES.saturating_sub(target.len());
    if remaining == 0 {
        return;
    }
    let mut end = text.len().min(remaining);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&text[..end]);
}

/// Merge a recovered request back into the composer draft.
///
/// The prefix rule is what makes recovery idempotent: once `first` has been
/// merged in front of `second`, merging again must not append it a second
/// time, or a failed question multiplies itself every time the draft is
/// persisted. Returns the merged draft and whether the budget dropped text.
fn merge_drafts_bounded(first: &str, second: &str) -> (String, bool) {
    let already_merged = second
        .strip_prefix(first)
        .is_some_and(|suffix| suffix.starts_with("\n\n"));
    if first.is_empty() || first == second || already_merged {
        let merged = bounded_live_message(second.to_string());
        return (merged, second.len() > MAX_LIVE_MESSAGE_BYTES);
    }
    if second.is_empty() {
        let merged = bounded_live_message(first.to_string());
        return (merged, first.len() > MAX_LIVE_MESSAGE_BYTES);
    }
    let full_len = first.len().saturating_add(2).saturating_add(second.len());
    let capacity = full_len.min(MAX_LIVE_MESSAGE_BYTES);
    let mut merged = String::with_capacity(capacity);
    append_bounded(&mut merged, first);
    let second_budget = MAX_LIVE_MESSAGE_BYTES
        .saturating_sub(merged.len())
        .saturating_sub(2);
    let mut second_end = second.len().min(second_budget);
    while !second.is_char_boundary(second_end) {
        second_end -= 1;
    }
    if second_end > 0 {
        merged.push_str("\n\n");
        merged.push_str(&second[..second_end]);
    }
    (merged, full_len > MAX_LIVE_MESSAGE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_context(cmd: &str) -> BlockContext {
        BlockContext {
            cmd: cmd.to_string(),
            output: format!("{cmd} output"),
            cwd: Some("/tmp".to_string()),
            exit_code: 1,
            truncated: false,
        }
    }

    fn start(store: &mut ChatStore, text: &str) -> RequestToken {
        store
            .begin_turn(text.to_string(), None, "Thinking…".to_string(), true)
            .expect("request starts")
            .token
    }

    fn finish(store: &mut ChatStore, text: &str, answer: &str) {
        let token = start(store, text);
        store.complete_success(token, answer.to_string());
    }

    // ---- identity and epoch ------------------------------------------------

    #[test]
    fn chats_keep_independent_history_drafts_and_titles() {
        let mut store = ChatStore::default();
        finish(&mut store, "first question", "first answer");
        store.set_active_draft("draft one".to_string());
        let first = store.active_id();

        let second = store.new_chat().expect("second chat");
        assert_ne!(first, second);
        assert!(store.active_history().is_empty());
        assert_eq!(store.active_draft(), "");

        store.set_active_draft("draft two".to_string());
        assert!(store.select_chat(first));
        assert_eq!(store.active_draft(), "draft one");
        assert_eq!(store.active_history().len(), 2);
        assert_eq!(store.active_title(), "first question");
    }

    #[test]
    fn late_results_are_owned_by_chat_and_epoch() {
        let mut store = ChatStore::default();
        let first_token = start(&mut store, "background question");
        let second = store.new_chat().expect("second chat");
        let second_token = start(&mut store, "foreground question");

        // The background reply lands on its own chat, not the visible one.
        assert_eq!(
            store.complete_success(first_token, "background answer".into()),
            Some(false)
        );
        assert_eq!(store.active_id(), second);
        assert_eq!(store.active_history().len(), 1);

        // Replaying the same token is a no-op: the epoch is spent.
        assert_eq!(store.complete_success(first_token, "replay".into()), None);

        assert_eq!(
            store.complete_success(second_token, "foreground answer".into()),
            Some(true)
        );
        assert_eq!(store.active_history().len(), 2);
    }

    #[test]
    fn background_replies_and_errors_update_only_the_request_owner() {
        let mut store = ChatStore::default();
        let background = start(&mut store, "slow question");
        let owner = store.active_id();
        store.new_chat().expect("second chat");

        assert_eq!(
            store.complete_error(background, "Error: provider refused".into()),
            Some(false)
        );
        let rows = store.summaries();
        let row = rows.iter().find(|row| row.id == owner).expect("owner row");
        assert!(row.error, "the owner row reports the failure");
        assert!(row.unread, "and is marked unread while off screen");
        assert!(!store.summaries().iter().any(|r| r.active && r.error));
    }

    #[test]
    fn deleting_an_inflight_chat_makes_late_completion_a_noop() {
        let mut store = ChatStore::with_busy_policy(BusyChatPolicy::Allow);
        let token = start(&mut store, "question");
        store.delete_active().expect("delete succeeds");
        assert_eq!(store.complete_success(token, "answer".into()), None);
    }

    #[test]
    fn cancelling_restores_the_draft_and_invalidates_late_completion() {
        let mut store = ChatStore::default();
        let token = start(&mut store, "question");
        assert_eq!(store.cancel_request(token, "Cancelled.".into()), Some(true));
        assert_eq!(store.active_draft(), "question");
        assert!(store.active_history().is_empty());
        assert!(!store.is_active_busy());
        assert_eq!(store.complete_success(token, "late".into()), None);
    }

    // ---- streaming ---------------------------------------------------------

    #[test]
    fn deltas_stream_into_the_request_owner_only() {
        let mut store = ChatStore::default();
        let background = start(&mut store, "background");
        store.new_chat().expect("second chat");
        let foreground = start(&mut store, "foreground");

        assert_eq!(store.push_delta(background, "back"), Some(false));
        assert_eq!(store.push_delta(foreground, "fore"), Some(true));
        assert_eq!(store.active_partial(), "fore");

        store.complete_success(foreground, "final".into());
        assert_eq!(
            store.active_partial(),
            "",
            "success clears the partial it replaces"
        );
    }

    #[test]
    fn a_stale_delta_is_dropped() {
        let mut store = ChatStore::default();
        let token = start(&mut store, "question");
        store.cancel_request(token, "Cancelled.".into());
        assert_eq!(store.push_delta(token, "late chunk"), None);
        assert_eq!(store.active_partial(), "");
    }

    #[test]
    fn streamed_partials_are_bounded_on_a_utf8_boundary() {
        let mut store = ChatStore::default();
        let token = start(&mut store, "question");
        let chunk = "é".repeat(MAX_LIVE_ASSISTANT_MESSAGE_BYTES);
        store.push_delta(token, &chunk);
        let partial = store.active_partial();
        assert!(partial.len() <= MAX_LIVE_ASSISTANT_MESSAGE_BYTES);
        assert!(
            partial.chars().all(|ch| ch == 'é'),
            "no partial code point survived the cut"
        );
        // A further delta with no room left is a no-op, not a panic.
        assert_eq!(store.push_delta(token, "more"), Some(true));
    }

    #[test]
    fn cancelling_clears_the_streamed_partial() {
        let mut store = ChatStore::default();
        let token = start(&mut store, "question");
        store.push_delta(token, "half an answer");
        store.cancel_request(token, "Cancelled.".into());
        assert_eq!(store.active_partial(), "");
    }

    // ---- bounds ------------------------------------------------------------

    #[test]
    fn live_message_budget_rejects_oversized_text_without_mutating_the_chat() {
        let mut store = ChatStore::default();
        let huge = "x".repeat(MAX_LIVE_MESSAGE_BYTES + 1);
        assert_eq!(
            store.begin_turn(huge, None, "Thinking…".into(), true),
            Err(ChatStoreError::MessageTooLarge)
        );
        assert!(store.active_history().is_empty());
        assert!(!store.is_active_busy());
        assert_eq!(store.active_title(), DEFAULT_CHAT_TITLE);
    }

    #[test]
    fn empty_messages_are_refused() {
        let mut store = ChatStore::default();
        assert_eq!(
            store.begin_turn("   \n\t ".into(), None, "Thinking…".into(), true),
            Err(ChatStoreError::EmptyMessage)
        );
    }

    #[test]
    fn oversized_assistant_replies_are_bounded_and_marked() {
        let mut store = ChatStore::default();
        let token = start(&mut store, "question");
        let huge = "y".repeat(MAX_LIVE_ASSISTANT_MESSAGE_BYTES + 4096);
        store.complete_success(token, huge);
        let last = store.active_history().last().expect("assistant turn");
        assert!(last.text.len() <= MAX_LIVE_ASSISTANT_MESSAGE_BYTES);
        assert!(last.text.ends_with("live message limit.]"));
        assert!(store.active_history_truncated());
    }

    #[test]
    fn live_history_is_bounded_by_turns_per_chat() {
        let mut store = ChatStore::default();
        for i in 0..(MAX_LIVE_TURNS_PER_CHAT + 10) {
            finish(&mut store, &format!("q{i}"), &format!("a{i}"));
        }
        assert!(store.active_history().len() <= MAX_LIVE_TURNS_PER_CHAT);
        assert!(store.active_history_truncated());
    }

    #[test]
    fn live_history_is_bounded_across_the_whole_library() {
        let mut store = ChatStore::default();
        let big = "z".repeat(200 * 1024);
        // Twelve chats of ~1 MiB each would exceed the 8 MiB library budget.
        for chat in 0..12 {
            if chat > 0 {
                store.new_chat().expect("chat");
            }
            for turn in 0..5 {
                finish(&mut store, &format!("q{chat}-{turn}"), &big);
            }
        }
        let total: usize = store
            .chats
            .iter()
            .flat_map(|c| c.history.iter())
            .map(|t| t.text.len())
            .sum();
        assert!(
            total <= MAX_LIVE_ALL_HISTORY_BYTES,
            "library-wide live budget held: {total}"
        );
    }

    #[test]
    fn global_compaction_preserves_a_background_inflight_question() {
        let mut store = ChatStore::default();
        let big = "z".repeat(200 * 1024);
        let inflight = start(&mut store, "keep me");
        let owner = store.active_id();
        store.new_chat().expect("second chat");
        for turn in 0..48 {
            finish(&mut store, &format!("q{turn}"), &big);
        }
        // The in-flight chat's trailing user turn has no completed pair, so
        // compaction cannot drop it.
        assert_eq!(
            store.complete_success(inflight, "answer".into()),
            Some(false)
        );
        let rows = store.summaries();
        assert!(rows.iter().any(|row| row.id == owner));
    }

    // ---- titles and previews ----------------------------------------------

    #[test]
    fn titles_are_normalised_bounded_and_spoof_resistant() {
        let mut store = ChatStore::default();
        store.rename_active("  many\t\tspaces \u{202e} spoofed\u{e0080}  ");
        assert_eq!(store.active_title(), "many spaces \u{fffd} spoofed\u{fffd}");

        store.rename_active(&"w".repeat(400));
        assert!(store.active_title().chars().count() <= MAX_CHAT_TITLE_CHARS);
        assert!(store.active_title().len() <= MAX_CHAT_TITLE_BYTES);

        store.rename_active("   ");
        assert_eq!(store.active_title(), DEFAULT_CHAT_TITLE);
    }

    #[test]
    fn the_first_message_derives_the_title_only_once() {
        let mut store = ChatStore::default();
        finish(&mut store, "why does the build hang", "because…");
        assert_eq!(store.active_title(), "why does the build hang");
        finish(&mut store, "a completely different follow up", "…");
        assert_eq!(store.active_title(), "why does the build hang");
    }

    #[test]
    fn previews_sanitize_untrusted_assistant_text() {
        let mut store = ChatStore::default();
        finish(
            &mut store,
            "question",
            "answer \u{202e}reversed\u{202c} tail",
        );
        let row = &store.summaries()[0];
        assert!(
            !row.preview.contains('\u{202e}'),
            "a bidi override reached the library row: {:?}",
            row.preview
        );
    }

    #[test]
    fn previews_fall_back_to_the_draft_then_to_a_neutral_label() {
        let mut store = ChatStore::default();
        assert_eq!(store.summaries()[0].preview, "Empty conversation");
        store.set_active_draft("a typed draft".to_string());
        assert_eq!(store.summaries()[0].preview, "a typed draft");
    }

    // ---- library filtering -------------------------------------------------

    #[test]
    fn summaries_filter_on_title_and_preview_and_stay_newest_first() {
        let mut store = ChatStore::default();
        finish(&mut store, "about cargo builds", "answer alpha");
        store.new_chat().expect("second");
        finish(&mut store, "about docker layers", "answer beta");

        assert_eq!(store.summaries().len(), 2);
        assert_eq!(store.summaries()[0].title, "about docker layers");

        let hits = store.summaries_filtered("CARGO");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "about cargo builds");

        // The preview is searched too, and a blank query matches everything.
        assert_eq!(store.summaries_filtered("answer beta").len(), 1);
        assert_eq!(store.summaries_filtered("   ").len(), 2);
        assert!(store.summaries_filtered("no such text").is_empty());
    }

    // ---- archive, delete, capacity ----------------------------------------

    #[test]
    fn archive_preserves_the_chat_and_selects_a_writable_replacement() {
        let mut store = ChatStore::default();
        finish(&mut store, "keep me", "answer");
        let archived = store.active_id();
        store.new_chat().expect("second chat");
        let replacement = store.active_id();
        assert!(store.select_chat(archived));

        let outcome = store.toggle_archive_active().expect("archive");
        assert!(outcome.archived);
        assert_eq!(outcome.active_chat_id, replacement);
        assert_eq!(store.summaries().len(), 2);
        assert!(store.summaries().iter().any(|row| row.archived));
    }

    #[test]
    fn archiving_the_last_writable_chat_at_capacity_is_refused_before_mutating() {
        let mut store = ChatStore::default();
        for _ in 1..MAX_PERSISTED_CHATS {
            store.new_chat().expect("fill the library");
        }
        assert!(store.at_capacity());
        // Archiving the active chat re-selects a writable one, so N-1 archives
        // in a row leave exactly one writable chat.
        for _ in 0..(MAX_PERSISTED_CHATS - 1) {
            store.toggle_archive_active().expect("archive");
        }
        assert_eq!(
            store.summaries().iter().filter(|row| !row.archived).count(),
            1
        );

        assert_eq!(
            store.toggle_archive_active(),
            Err(ChatStoreError::LimitReached)
        );
        assert!(
            !store.active_archived(),
            "a refused archive left no half-applied state"
        );
    }

    #[test]
    fn the_chat_limit_refuses_a_new_chat_without_deleting_rows() {
        let mut store = ChatStore::default();
        for _ in 1..MAX_PERSISTED_CHATS {
            store.new_chat().expect("chat");
        }
        assert_eq!(store.new_chat(), Err(ChatStoreError::LimitReached));
        assert_eq!(store.len(), MAX_PERSISTED_CHATS);
        assert_eq!(store.summaries().len(), MAX_PERSISTED_CHATS);
    }

    #[test]
    fn delete_always_leaves_a_writable_chat_selected() {
        let mut store = ChatStore::default();
        finish(&mut store, "only chat", "answer");
        store.toggle_archive_active().expect("archive");
        // Every chat is archived; deleting the replacement must mint a new one.
        let ids: Vec<u64> = store.summaries().iter().map(|row| row.id).collect();
        for id in ids {
            assert!(store.select_chat(id) || store.active_id() == id);
            store.toggle_archive_active().ok();
        }
        let outcome = store.delete_active().expect("delete");
        assert!(!store.active_archived());
        assert_ne!(outcome.deleted_chat_id, store.active_id());
    }

    #[test]
    fn archive_and_delete_clear_the_replacement_chats_unread_flag() {
        let mut store = ChatStore::default();
        let background = start(&mut store, "background question");
        let owner = store.active_id();
        store.new_chat().expect("second chat");
        store.complete_success(background, "answer".into());
        assert!(store
            .summaries()
            .iter()
            .any(|row| row.id == owner && row.unread));

        // Deleting the visible chat promotes the unread one; being on screen
        // is what clears the badge.
        store.delete_active().expect("delete");
        assert_eq!(store.active_id(), owner);
        assert!(
            !store.summaries().iter().any(|row| row.unread),
            "the promoted chat is on screen, so it is no longer unread"
        );
    }

    #[test]
    fn the_busy_policy_decides_whether_archive_and_delete_are_refused() {
        let mut refusing = ChatStore::with_busy_policy(BusyChatPolicy::Refuse);
        start(&mut refusing, "question");
        assert_eq!(refusing.toggle_archive_active(), Err(ChatStoreError::Busy));
        assert_eq!(refusing.delete_active(), Err(ChatStoreError::Busy));

        let mut allowing = ChatStore::with_busy_policy(BusyChatPolicy::Allow);
        let token = start(&mut allowing, "question");
        assert!(allowing.delete_active().is_ok());
        assert_eq!(
            allowing.complete_success(token, "late".into()),
            None,
            "the late reply is still discarded"
        );
    }

    #[test]
    fn a_busy_chat_refuses_a_second_turn_and_a_context_clear() {
        let mut store = ChatStore::default();
        store
            .begin_turn(
                "first".into(),
                Some(block_context("ls")),
                "Thinking…".into(),
                true,
            )
            .expect("first request");
        assert_eq!(
            store.begin_turn("second".into(), None, "Thinking…".into(), true),
            Err(ChatStoreError::Busy)
        );
        assert_eq!(store.clear_active_context(), Err(ChatStoreError::Busy));
    }

    #[test]
    fn an_archived_chat_refuses_new_turns() {
        let mut store = ChatStore::default();
        store.new_chat().expect("second chat");
        store.toggle_archive_active().expect("archive");
        let archived = store
            .summaries()
            .into_iter()
            .find(|row| row.archived)
            .expect("archived row");
        assert!(store.select_chat(archived.id));
        assert_eq!(
            store.begin_turn("question".into(), None, "Thinking…".into(), true),
            Err(ChatStoreError::Archived)
        );
    }

    // ---- block context -----------------------------------------------------

    #[test]
    fn replacement_context_becomes_durable_only_after_success() {
        let mut store = ChatStore::default();
        store
            .begin_turn(
                "first".into(),
                Some(block_context("first-cmd")),
                "Thinking…".into(),
                true,
            )
            .expect("first");
        let token = store.active_request_token().expect("token");
        store.complete_success(token, "answer".into());
        assert_eq!(
            store.active_context().map(|c| c.cmd.as_str()),
            Some("first-cmd")
        );

        // A failed replacement rolls the durable context back.
        store
            .begin_turn(
                "second".into(),
                Some(block_context("second-cmd")),
                "Thinking…".into(),
                true,
            )
            .expect("second");
        let token = store.active_request_token().expect("token");
        store.complete_error(token, "Error".into());
        assert_eq!(
            store.active_context().map(|c| c.cmd.as_str()),
            Some("first-cmd"),
            "the failed request left no orphan context"
        );
    }

    #[test]
    fn a_failed_first_context_request_leaves_no_orphan_context() {
        let mut store = ChatStore::default();
        store
            .begin_turn(
                "question".into(),
                Some(block_context("ls")),
                "Thinking…".into(),
                true,
            )
            .expect("request");
        let token = store.active_request_token().expect("token");
        store.complete_error(token, "Error".into());
        assert!(store.active_context().is_none());
        assert_eq!(store.clear_active_context(), Ok(false));
    }

    // ---- drafts and retry recovery ----------------------------------------

    #[test]
    fn a_failed_request_is_recoverable_as_a_draft() {
        let mut store = ChatStore::default();
        store.set_active_draft("follow up".into());
        let token = start(&mut store, "the question");
        store.complete_error(token, "Error: offline".into());
        assert_eq!(store.active_draft(), "the question\n\nfollow up");
        assert!(store.active_history().is_empty());
    }

    #[test]
    fn a_request_that_should_not_restore_leaves_the_draft_alone() {
        let mut store = ChatStore::default();
        store.set_active_draft("untouched".into());
        let start = store
            .begin_turn("block ask".into(), None, "Thinking…".into(), false)
            .expect("request");
        store.complete_error(start.token, "Error".into());
        assert_eq!(store.active_draft(), "untouched");
    }

    #[test]
    fn merging_a_recovered_draft_is_idempotent() {
        // The bug this guards: persisting a recovered retry re-merged its own
        // question every time, so the draft grew a copy per autosave.
        let (once, _) = merge_drafts_bounded("question", "question\n\nfollow up");
        assert_eq!(once, "question\n\nfollow up");
        let (twice, _) = merge_drafts_bounded("question", &once);
        assert_eq!(twice, once, "a second merge added nothing");

        let (same, _) = merge_drafts_bounded("question", "question");
        assert_eq!(same, "question");
        let (empty_first, _) = merge_drafts_bounded("", "draft");
        assert_eq!(empty_first, "draft");
        let (empty_second, _) = merge_drafts_bounded("question", "");
        assert_eq!(empty_second, "question");
    }

    #[test]
    fn retry_draft_merging_never_exceeds_the_live_budget_and_reports_it() {
        let half = "a".repeat(MAX_LIVE_MESSAGE_BYTES * 2 / 3);
        let other = "b".repeat(MAX_LIVE_MESSAGE_BYTES * 2 / 3);
        let (merged, truncated) = merge_drafts_bounded(&half, &other);
        assert!(merged.len() <= MAX_LIVE_MESSAGE_BYTES);
        assert!(truncated, "the caller is told text was dropped");
    }

    #[test]
    fn a_failed_request_reports_when_draft_text_was_omitted() {
        let mut store = ChatStore::default();
        store.set_active_draft("b".repeat(MAX_LIVE_MESSAGE_BYTES * 2 / 3));
        let token = start(&mut store, &"a".repeat(MAX_LIVE_MESSAGE_BYTES * 2 / 3));
        store.complete_error(token, "Error: offline.".into());
        match store.active_status() {
            ChatStatus::Error(message) => assert!(
                message.contains("omitted at the 64 KiB limit"),
                "silent truncation: {message}"
            ),
            other => panic!("expected an error status, got {other:?}"),
        }
    }

    #[test]
    fn live_drafts_are_trimmed_on_a_utf8_boundary() {
        let mut store = ChatStore::default();
        store.set_active_draft("é".repeat(MAX_LIVE_MESSAGE_BYTES));
        let draft = store.active_draft();
        assert!(draft.len() <= MAX_LIVE_MESSAGE_BYTES);
        assert!(draft.chars().all(|ch| ch == 'é'));
    }

    #[test]
    fn retry_recovery_refuses_a_busy_chat_but_the_detaching_variant_does_not() {
        let mut store = ChatStore::default();
        let chat_id = store.active_id();
        start(&mut store, "in flight");
        assert!(!store.recover_retry_payload(chat_id, "retry text", None));

        let mut clone = store.clone();
        assert!(clone.recover_retry_payload_detaching(chat_id, "retry text", None));
        assert!(!clone.is_active_busy());
        assert_eq!(clone.active_draft(), "retry text");
        assert!(
            clone.active_history().is_empty(),
            "the detached trailing user turn was popped"
        );
        assert!(store.is_active_busy(), "the live store is untouched");
    }

    #[test]
    fn retry_recovery_on_an_unknown_chat_reports_failure() {
        let mut store = ChatStore::default();
        assert!(!store.recover_retry_payload(9999, "text", None));
        assert!(!store.recover_retry_payload_detaching(9999, "text", None));
    }

    // ---- persistence -------------------------------------------------------

    #[test]
    fn a_snapshot_round_trips_multiple_chats_with_their_metadata() {
        let mut store = ChatStore::default();
        finish(&mut store, "first question", "first answer");
        store.set_active_draft("kept draft".into());
        store.new_chat().expect("second chat");
        finish(&mut store, "second question", "second answer");
        let active = store.active_id();

        let (snapshot, _) = store.snapshot_for_persistence(false).expect("snapshot");
        let json = snapshot.to_json().expect("encode");
        let decoded = ConversationSnapshot::from_json(&json).expect("decode");
        let restored = ChatStore::restore(decoded);

        assert_eq!(restored.active_id(), active);
        assert_eq!(restored.summaries().len(), 2);
        assert_eq!(restored.active_title(), "second question");
        let rows = restored.summaries();
        let first = rows
            .iter()
            .find(|row| row.title == "first question")
            .expect("first row");
        assert!(!first.active);
    }

    #[test]
    fn persistence_redacts_every_chat_including_title_draft_and_context() {
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let mut store = ChatStore::default();
        store
            .begin_turn(
                format!("look at {secret}"),
                Some(BlockContext {
                    cmd: format!("aws --key {secret}"),
                    output: format!("token {secret}"),
                    cwd: Some(format!("/home/{secret}")),
                    exit_code: 0,
                    truncated: false,
                }),
                "Thinking…".into(),
                true,
            )
            .expect("request");
        let token = store.active_request_token().expect("token");
        store.complete_success(token, format!("saw {secret}"));
        store.rename_active(&format!("title {secret}"));
        store.set_active_draft(format!("draft {secret}"));

        let (snapshot, _) = store.snapshot_for_persistence(true).expect("snapshot");
        let json = snapshot.to_json().expect("encode");
        assert!(
            !json.contains(secret),
            "a secret survived redaction into the persisted library"
        );
    }

    #[test]
    fn persistence_compacts_live_state_so_a_snapshot_stays_possible() {
        let mut store = ChatStore::default();
        let big = "z".repeat(200 * 1024);
        for chat in 0..12 {
            if chat > 0 {
                store.new_chat().expect("chat");
            }
            for turn in 0..5 {
                finish(&mut store, &format!("q{chat}-{turn}"), &big);
            }
        }
        // Without library-wide compaction this is the state in which nothing
        // can be saved at all.
        let (snapshot, _) = store
            .snapshot_for_persistence(false)
            .expect("a compacted snapshot is still producible");
        assert!(snapshot.to_json().is_ok());
    }

    #[test]
    fn a_compaction_marker_syncs_back_into_the_live_chat() {
        let mut store = ChatStore::default();
        // Persistence caps total turn text at 4 MiB while the live library
        // budget is 8 MiB, so ~5 MiB of live history is legal in memory and
        // still forces persistence to drop pairs.
        let big = "z".repeat(200 * 1024);
        for turn in 0..25 {
            finish(&mut store, &format!("q{turn}"), &big);
        }
        assert!(!store.active_history_truncated());
        let (_, changed) = store.snapshot_for_persistence(false).expect("snapshot");
        assert!(changed, "the caller is told to re-render the library");
        assert!(store.active_history_truncated());
        let (_, changed_again) = store.snapshot_for_persistence(false).expect("snapshot");
        assert!(!changed_again, "the marker is only reported once");
    }

    #[test]
    fn an_empty_archived_chat_and_its_draft_round_trip() {
        let mut store = ChatStore::default();
        store.set_active_draft("just a draft".into());
        store.new_chat().expect("second chat");
        finish(&mut store, "question", "answer");
        assert!(store.select_chat(1));
        store.toggle_archive_active().expect("archive");

        let (snapshot, _) = store.snapshot_for_persistence(false).expect("snapshot");
        let restored = ChatStore::restore(
            ConversationSnapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
        );
        let rows = restored.summaries();
        let archived = rows.iter().find(|row| row.archived).expect("archived row");
        assert_eq!(archived.preview, "just a draft");
    }

    #[test]
    fn shutdown_recovery_persists_an_independent_block_retry() {
        let mut store = ChatStore::default();
        store.set_active_draft("composer text the user is still typing".into());
        let chat_id = store.active_id();
        store
            .begin_turn(
                "explain this block".into(),
                Some(block_context("make build")),
                "Thinking…".into(),
                false,
            )
            .expect("block request");

        // At teardown the app clones, materialises the in-flight retry, and
        // persists the clone — the live composer keeps its own draft.
        let mut for_persistence = store.clone();
        assert!(for_persistence.recover_retry_payload_detaching(
            chat_id,
            "explain this block",
            Some(block_context("make build")),
        ));
        let (snapshot, _) = for_persistence
            .snapshot_for_persistence(false)
            .expect("snapshot");
        let restored = ChatStore::restore(
            ConversationSnapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
        );
        assert!(restored.active_draft().contains("explain this block"));
        assert!(restored
            .active_draft()
            .contains("composer text the user is still typing"));
        assert_eq!(
            store.active_draft(),
            "composer text the user is still typing"
        );
    }

    #[test]
    fn restore_allocates_ids_above_every_restored_chat() {
        let mut store = ChatStore::default();
        finish(&mut store, "one", "a");
        store.new_chat().expect("two");
        finish(&mut store, "two", "b");
        let (snapshot, _) = store.snapshot_for_persistence(false).expect("snapshot");
        let existing: Vec<u64> = snapshot.chats().iter().map(|c| c.id()).collect();

        let mut restored = ChatStore::restore(snapshot);
        let fresh = restored.new_chat().expect("a fresh chat");
        assert!(
            !existing.contains(&fresh),
            "a new chat reused a restored id: {fresh} in {existing:?}"
        );
    }

    #[test]
    fn in_flight_tokens_lists_every_running_request() {
        let mut store = ChatStore::default();
        let first = start(&mut store, "one");
        store.new_chat().expect("second");
        let second = start(&mut store, "two");
        let mut tokens = store.in_flight_tokens();
        tokens.sort_by_key(|token| token.chat_id);
        assert_eq!(tokens, vec![first, second]);

        store.complete_success(first, "answer".into());
        assert_eq!(store.in_flight_tokens(), vec![second]);
    }

    #[test]
    fn status_helpers_never_overwrite_a_thinking_status() {
        let mut store = ChatStore::default();
        start(&mut store, "question");
        store.clear_active_status();
        assert!(
            matches!(store.active_status(), ChatStatus::Thinking(_)),
            "a busy chat keeps its Thinking status"
        );
        let token = store.active_request_token().expect("token");
        store.complete_success(token, "answer".into());
        store.set_active_info("saved");
        assert_eq!(store.active_status(), &ChatStatus::Info("saved".into()));
        store.clear_active_status();
        assert_eq!(store.active_status(), &ChatStatus::Idle);
    }
}
