//! Versioned, bounded persistence model for the AI chat panel.
//!
//! A snapshot contains an ordered collection of chats. Live chats may have a
//! trailing user turn while a request is in flight; constructors deliberately
//! retain only complete successful `user -> assistant` pairs. Collection-level
//! compaction preserves every chat row and removes old context/history before
//! the embedded window-state JSON can exceed its hard limit.

use super::{BlockContext, Role, Turn};
use serde::de::{self, DeserializeSeed, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::fmt;

const LEGACY_CONVERSATION_SNAPSHOT_VERSION: u32 = 1;
const CONVERSATION_SNAPSHOT_VERSION: u32 = 2;
pub const MAX_PERSISTED_CHATS: usize = 50;
const MAX_PERSISTED_TURNS: usize = 100;
const MAX_TURN_BYTES: usize = 256 * 1024;
const MAX_CHAT_TURN_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ALL_CHAT_TURN_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_BLOCK_CONTEXT_BYTES: usize = 512 * 1024;
const MAX_ALL_BLOCK_CONTEXT_BYTES: usize = 1024 * 1024;
const MAX_CHAT_TITLE_BYTES: usize = 256;
const MAX_CHAT_TITLE_CHARS: usize = 80;
const MAX_CHAT_DRAFT_BYTES: usize = 64 * 1024;
const MAX_ALL_CHAT_DRAFT_BYTES: usize = MAX_PERSISTED_CHATS * MAX_CHAT_DRAFT_BYTES;
const DEFAULT_CHAT_TITLE: &str = "New chat";

/// Hard upper bound for the compact JSON value embedded in window state.
/// Escaping it for the line-oriented state format can at most double this.
pub const MAX_CONVERSATION_SNAPSHOT_JSON_BYTES: usize = 8 * 1024 * 1024;

/// A single durable chat row and its complete provider history.
///
/// Deliberately not [`Deserialize`]: [`ConversationSnapshot::from_json`] is the
/// only wire path, and it decodes through budgeted seeds instead of Serde's
/// ordinary collection deserialization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChatSnapshot {
    id: u64,
    title: String,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    turns: Vec<Turn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    block_context: Option<BlockContext>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    draft: String,
    #[serde(default)]
    history_truncated: bool,
}

/// The complete AI chat collection associated with one window snapshot.
///
/// Deliberately not [`Deserialize`]: see [`ChatSnapshot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConversationSnapshot {
    version: u32,
    active_chat_id: u64,
    chats: Vec<ChatSnapshot>,
}

#[derive(Debug, Deserialize)]
struct SnapshotVersion {
    version: u32,
}

#[derive(Debug)]
struct LegacyConversationSnapshot {
    version: u32,
    turns: Vec<Turn>,
    block_context: Option<BlockContext>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationSnapshotError {
    EncodedTooLarge,
    InvalidJson(String),
    UnsupportedVersion(u32),
    EmptyCollection,
    TooManyChats,
    InvalidChatId,
    DuplicateChatId(u64),
    ActiveChatMissing(u64),
    InvalidChatTitle,
    InvalidTurnSequence,
    TurnTooLarge,
    ConversationTooLarge,
    BlockContextTooLarge,
}

impl fmt::Display for ConversationSnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EncodedTooLarge => write!(f, "encoded conversation snapshot is too large"),
            Self::InvalidJson(error) => write!(f, "invalid conversation snapshot JSON: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported conversation snapshot version {version}")
            }
            Self::EmptyCollection => write!(f, "conversation collection is empty"),
            Self::TooManyChats => write!(f, "conversation collection exceeds the chat limit"),
            Self::InvalidChatId => write!(f, "chat IDs must be greater than zero"),
            Self::DuplicateChatId(id) => write!(f, "duplicate chat ID {id}"),
            Self::ActiveChatMissing(id) => write!(f, "active chat ID {id} does not exist"),
            Self::InvalidChatTitle => write!(f, "chat title is empty or exceeds the size limit"),
            Self::InvalidTurnSequence => {
                write!(f, "conversation must contain complete user/assistant pairs")
            }
            Self::TurnTooLarge => write!(f, "a conversation turn exceeds the size limit"),
            Self::ConversationTooLarge => write!(f, "conversation exceeds the retention limit"),
            Self::BlockContextTooLarge => write!(f, "block context exceeds the size limit"),
        }
    }
}

impl std::error::Error for ConversationSnapshotError {}

impl ChatSnapshot {
    /// Build one chat from live provider history.
    ///
    /// The ID is a stable, caller-owned identity. The title is normalised and
    /// bounded for safe rendering. A trailing in-flight user turn is ignored;
    /// malformed or oversized pairs and retention trimming are recorded with
    /// `history_truncated` rather than making the chat row disappear.
    pub fn from_completed_history(
        id: u64,
        title: &str,
        archived: bool,
        history: &[Turn],
        block_context: Option<&BlockContext>,
        draft: &str,
    ) -> Self {
        assert!(id > 0, "chat ID must be greater than zero");

        let (turns, mut history_truncated) = completed_history(history);
        let block_context = if turns.is_empty() {
            // Context belongs to a completed provider exchange. Persisting it
            // beside only an in-flight user turn would restore an orphaned
            // system prompt with no visible conversation explaining it.
            None
        } else {
            match block_context {
                Some(context) if valid_context(context) => Some(context.clone()),
                Some(_) => {
                    history_truncated = true;
                    None
                }
                None => None,
            }
        };
        let (draft, draft_truncated) = bounded_draft(draft);
        history_truncated |= draft_truncated;

        Self {
            id,
            title: normalise_title(title),
            archived,
            turns,
            block_context,
            draft,
            history_truncated,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn archived(&self) -> bool {
        self.archived
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn block_context(&self) -> Option<&BlockContext> {
        self.block_context.as_ref()
    }

    pub fn draft(&self) -> &str {
        &self.draft
    }

    pub fn history_truncated(&self) -> bool {
        self.history_truncated
    }

    /// Preserve a truncation marker restored from an earlier snapshot. The
    /// constructor can detect trimming performed in the current process, but
    /// already-trimmed turns no longer reveal that older pairs once existed.
    pub fn with_history_truncated(mut self, history_truncated: bool) -> Self {
        self.history_truncated |= history_truncated;
        self
    }

    pub fn into_parts(
        self,
    ) -> (
        u64,
        String,
        bool,
        Vec<Turn>,
        Option<BlockContext>,
        String,
        bool,
    ) {
        (
            self.id,
            self.title,
            self.archived,
            self.turns,
            self.block_context,
            self.draft,
            self.history_truncated,
        )
    }

    fn validate(&self) -> Result<(), ConversationSnapshotError> {
        if self.id == 0 {
            return Err(ConversationSnapshotError::InvalidChatId);
        }
        if !valid_title(&self.title) {
            return Err(ConversationSnapshotError::InvalidChatTitle);
        }
        validate_turns(&self.turns)?;
        if self.turns.is_empty() && self.block_context.is_some() {
            return Err(ConversationSnapshotError::InvalidTurnSequence);
        }
        if self
            .block_context
            .as_ref()
            .is_some_and(|context| !valid_context(context))
        {
            return Err(ConversationSnapshotError::BlockContextTooLarge);
        }
        if self.draft.len() > MAX_CHAT_DRAFT_BYTES {
            return Err(ConversationSnapshotError::ConversationTooLarge);
        }
        Ok(())
    }

    fn turn_text_bytes(&self) -> usize {
        self.turns
            .iter()
            .fold(0usize, |total, turn| total.saturating_add(turn.text.len()))
    }

    fn context_bytes(&self) -> usize {
        self.block_context.as_ref().map_or(0, context_bytes)
    }

    fn draft_bytes(&self) -> usize {
        self.draft.len()
    }

    fn drop_context(&mut self) -> bool {
        if self.block_context.take().is_some() {
            self.history_truncated = true;
            true
        } else {
            false
        }
    }

    fn drop_oldest_pair(&mut self) -> bool {
        if self.turns.len() < 2 {
            return false;
        }
        self.turns.drain(..2);
        // A Block context is meaningful only alongside a completed exchange.
        // Global compaction can remove the final retained pair, so clear the
        // now-orphaned context before the collection is validated again.
        if self.turns.is_empty() {
            self.block_context = None;
        }
        self.history_truncated = true;
        true
    }

    fn drop_draft(&mut self) -> bool {
        if self.draft.is_empty() {
            return false;
        }
        self.draft.clear();
        self.history_truncated = true;
        true
    }
}

impl ConversationSnapshot {
    /// Build a bounded collection while retaining every supplied chat row.
    ///
    /// Chats are expected in oldest-to-newest display order. When the global
    /// budget is exceeded, inactive older context and message pairs are
    /// trimmed before the active chat; metadata is never silently removed.
    pub fn from_chats(active_chat_id: u64, chats: Vec<ChatSnapshot>) -> Option<Self> {
        let mut snapshot = Self {
            version: CONVERSATION_SNAPSHOT_VERSION,
            active_chat_id,
            chats,
        };
        snapshot.validate_structure().ok()?;
        snapshot.compact_raw_budgets();
        snapshot.compact_encoded_budget()?;
        snapshot.validate().ok()?;
        Some(snapshot)
    }

    /// Compatibility constructor for callers which still publish one chat.
    /// Empty or wholly invalid live histories keep the legacy `None` result.
    pub fn from_completed_history(
        history: &[Turn],
        block_context: Option<&BlockContext>,
    ) -> Option<Self> {
        let title = history
            .iter()
            .find(|turn| turn.role == Role::User && !turn.text.trim().is_empty())
            .map_or(DEFAULT_CHAT_TITLE, |turn| turn.text.as_str());
        let chat =
            ChatSnapshot::from_completed_history(1, title, false, history, block_context, "");
        if chat.turns.is_empty() {
            return None;
        }
        Self::from_chats(1, vec![chat])
    }

    pub fn active_chat_id(&self) -> u64 {
        self.active_chat_id
    }

    pub fn chats(&self) -> &[ChatSnapshot] {
        &self.chats
    }

    pub fn active_chat(&self) -> Option<&ChatSnapshot> {
        self.chats
            .iter()
            .find(|chat| chat.id == self.active_chat_id)
    }

    /// Compatibility view over the active chat's provider history.
    pub fn turns(&self) -> &[Turn] {
        self.active_chat().map_or(&[], ChatSnapshot::turns)
    }

    /// Compatibility view over the active chat's selected-block context.
    pub fn block_context(&self) -> Option<&BlockContext> {
        self.active_chat().and_then(ChatSnapshot::block_context)
    }

    pub fn into_parts(self) -> (u64, Vec<ChatSnapshot>) {
        (self.active_chat_id, self.chats)
    }

    pub fn to_json(&self) -> Result<String, ConversationSnapshotError> {
        self.validate()?;
        let encoded = serde_json::to_string(self)
            .map_err(|error| ConversationSnapshotError::InvalidJson(error.to_string()))?;
        if encoded.len() > MAX_CONVERSATION_SNAPSHOT_JSON_BYTES {
            return Err(ConversationSnapshotError::EncodedTooLarge);
        }
        Ok(encoded)
    }

    /// Decode a persisted collection under the schema's own budgets.
    ///
    /// The encoded envelope is bounded first, then decoding itself stops
    /// before chat 51 and turn 101, rejects unknown and duplicate fields, and
    /// charges per-field plus cumulative title, turn, draft, and context
    /// budgets while constructing. Both schema versions and every public error
    /// category are preserved; `validate` still owns the semantic invariants.
    pub fn from_json(encoded: &str) -> Result<Self, ConversationSnapshotError> {
        if encoded.len() > MAX_CONVERSATION_SNAPSHOT_JSON_BYTES {
            return Err(ConversationSnapshotError::EncodedTooLarge);
        }
        // Peek the schema version without retaining any other field.
        let version: SnapshotVersion = serde_json::from_str(encoded)
            .map_err(|error| ConversationSnapshotError::InvalidJson(error.to_string()))?;
        match version.version {
            LEGACY_CONVERSATION_SNAPSHOT_VERSION => {
                let legacy = decode_legacy_snapshot(encoded)?;
                legacy.validate()?;
                let title = legacy
                    .turns
                    .first()
                    .map_or(DEFAULT_CHAT_TITLE, |turn| turn.text.as_str());
                let chat = ChatSnapshot::from_completed_history(
                    1,
                    title,
                    false,
                    &legacy.turns,
                    legacy.block_context.as_ref(),
                    "",
                );
                Self::from_chats(1, vec![chat])
                    .ok_or(ConversationSnapshotError::ConversationTooLarge)
            }
            CONVERSATION_SNAPSHOT_VERSION => {
                let snapshot = decode_snapshot(encoded)?;
                snapshot.validate()?;
                Ok(snapshot)
            }
            version => Err(ConversationSnapshotError::UnsupportedVersion(version)),
        }
    }

    fn validate_structure(&self) -> Result<(), ConversationSnapshotError> {
        if self.version != CONVERSATION_SNAPSHOT_VERSION {
            return Err(ConversationSnapshotError::UnsupportedVersion(self.version));
        }
        if self.chats.is_empty() {
            return Err(ConversationSnapshotError::EmptyCollection);
        }
        if self.chats.len() > MAX_PERSISTED_CHATS {
            return Err(ConversationSnapshotError::TooManyChats);
        }
        if self.active_chat_id == 0 {
            return Err(ConversationSnapshotError::InvalidChatId);
        }

        let mut ids = HashSet::with_capacity(self.chats.len());
        for chat in &self.chats {
            chat.validate()?;
            if !ids.insert(chat.id) {
                return Err(ConversationSnapshotError::DuplicateChatId(chat.id));
            }
        }
        if !ids.contains(&self.active_chat_id) {
            return Err(ConversationSnapshotError::ActiveChatMissing(
                self.active_chat_id,
            ));
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ConversationSnapshotError> {
        self.validate_structure()?;
        if self.total_turn_text_bytes() > MAX_ALL_CHAT_TURN_TEXT_BYTES {
            return Err(ConversationSnapshotError::ConversationTooLarge);
        }
        if self.total_context_bytes() > MAX_ALL_BLOCK_CONTEXT_BYTES {
            return Err(ConversationSnapshotError::BlockContextTooLarge);
        }
        if self.total_draft_bytes() > MAX_ALL_CHAT_DRAFT_BYTES {
            return Err(ConversationSnapshotError::ConversationTooLarge);
        }
        Ok(())
    }

    fn compact_raw_budgets(&mut self) {
        while self.total_context_bytes() > MAX_ALL_BLOCK_CONTEXT_BYTES {
            if !self.drop_old_context(false) && !self.drop_old_context(true) {
                break;
            }
        }
        while self.total_turn_text_bytes() > MAX_ALL_CHAT_TURN_TEXT_BYTES {
            if !self.drop_oldest_pair(false) && !self.drop_oldest_pair(true) {
                break;
            }
        }
    }

    fn compact_encoded_budget(&mut self) -> Option<()> {
        self.compact_to_measured_limit(MAX_CONVERSATION_SNAPSHOT_JSON_BYTES, |snapshot| {
            serde_json::to_vec(snapshot)
                .ok()
                .map(|encoded| encoded.len())
        })
    }

    /// Compact payload to an enclosing format's exact measured limit.
    ///
    /// The deterministic drop order preserves active data for longest. A
    /// binary search keeps worst-case escaped payloads from requiring thousands
    /// of full multi-megabyte serializations.
    pub fn compact_to_measured_limit(
        &mut self,
        max_bytes: usize,
        measure: impl Fn(&Self) -> Option<usize>,
    ) -> Option<()> {
        if measure(self)? <= max_bytes {
            return Some(());
        }

        let original = self.clone();
        let mut emptied = original.clone();
        let mut drop_count = 0usize;
        while emptied.drop_next_payload() {
            drop_count += 1;
        }
        if measure(&emptied)? > max_bytes {
            // Even the metadata-only collection cannot fit. Never delete chat
            // rows merely to make an enclosing state file valid.
            return None;
        }

        let mut low = 1usize;
        let mut high = drop_count;
        while low < high {
            let mid = low + (high - low) / 2;
            let mut candidate = original.clone();
            candidate.drop_payloads(mid);
            if measure(&candidate)? <= max_bytes {
                high = mid;
            } else {
                low = mid + 1;
            }
        }

        let mut compacted = original;
        compacted.drop_payloads(low);
        compacted.validate().ok()?;
        *self = compacted;
        Some(())
    }

    fn drop_payloads(&mut self, count: usize) {
        for _ in 0..count {
            if !self.drop_next_payload() {
                break;
            }
        }
    }

    fn drop_next_payload(&mut self) -> bool {
        // Preserve the active chat for as long as possible. Old context is
        // least useful after switching chats, followed by old message pairs.
        self.drop_old_context(false)
            || self.drop_oldest_pair(false)
            || self.drop_old_context(true)
            || self.drop_oldest_pair(true)
            || self.drop_old_draft(false)
            || self.drop_old_draft(true)
    }

    fn drop_old_context(&mut self, active: bool) -> bool {
        for chat in &mut self.chats {
            if (chat.id == self.active_chat_id) == active && chat.drop_context() {
                return true;
            }
        }
        false
    }

    fn drop_oldest_pair(&mut self, active: bool) -> bool {
        for chat in &mut self.chats {
            if (chat.id == self.active_chat_id) == active && chat.drop_oldest_pair() {
                return true;
            }
        }
        false
    }

    fn drop_old_draft(&mut self, active: bool) -> bool {
        for chat in &mut self.chats {
            if (chat.id == self.active_chat_id) == active && chat.drop_draft() {
                return true;
            }
        }
        false
    }

    fn total_turn_text_bytes(&self) -> usize {
        self.chats.iter().fold(0usize, |total, chat| {
            total.saturating_add(chat.turn_text_bytes())
        })
    }

    fn total_context_bytes(&self) -> usize {
        self.chats.iter().fold(0usize, |total, chat| {
            total.saturating_add(chat.context_bytes())
        })
    }

    fn total_draft_bytes(&self) -> usize {
        self.chats.iter().fold(0usize, |total, chat| {
            total.saturating_add(chat.draft_bytes())
        })
    }
}

impl LegacyConversationSnapshot {
    fn validate(&self) -> Result<(), ConversationSnapshotError> {
        if self.version != LEGACY_CONVERSATION_SNAPSHOT_VERSION {
            return Err(ConversationSnapshotError::UnsupportedVersion(self.version));
        }
        if self.turns.is_empty() {
            return Err(ConversationSnapshotError::InvalidTurnSequence);
        }
        validate_turns(&self.turns)?;
        if self
            .block_context
            .as_ref()
            .is_some_and(|context| !valid_context(context))
        {
            return Err(ConversationSnapshotError::BlockContextTooLarge);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bounded wire decoding
// ---------------------------------------------------------------------------

/// Collection-wide allocation budgets, charged while decoding.
///
/// `from_json` refuses an oversized encoded envelope first, but an 8 MiB
/// document can still describe thousands of chats, thousands of turns, or a
/// handful of near-envelope-sized strings. Ordinary Serde deserialization
/// would build all of them and only then meet the retention limits, so the
/// seeds below charge as they decode and stop at the first value that does not
/// fit. `validate` still owns every *semantic* invariant — identity, ordering,
/// role alternation, and the active-chat binding.
struct DecodeState {
    remaining_turn_text: usize,
    remaining_context: usize,
    remaining_draft: usize,
    /// The schema-specific reason a budget rejected the document, so
    /// decoding-time enforcement keeps the public error categories.
    error: Option<ConversationSnapshotError>,
}

impl DecodeState {
    fn new() -> Self {
        Self {
            remaining_turn_text: MAX_ALL_CHAT_TURN_TEXT_BYTES,
            remaining_context: MAX_ALL_BLOCK_CONTEXT_BYTES,
            remaining_draft: MAX_ALL_CHAT_DRAFT_BYTES,
            error: None,
        }
    }

    fn fail<E: de::Error>(&mut self, reason: ConversationSnapshotError) -> E {
        let message = reason.to_string();
        self.error.get_or_insert(reason);
        de::Error::custom(message)
    }

    fn charge<E: de::Error>(&mut self, budget: TextBudget, bytes: usize) -> Result<(), E> {
        let (remaining, reason) = match budget {
            TextBudget::None => return Ok(()),
            TextBudget::TurnText => (
                &mut self.remaining_turn_text,
                ConversationSnapshotError::ConversationTooLarge,
            ),
            TextBudget::Draft => (
                &mut self.remaining_draft,
                ConversationSnapshotError::ConversationTooLarge,
            ),
            TextBudget::Context => (
                &mut self.remaining_context,
                ConversationSnapshotError::BlockContextTooLarge,
            ),
        };
        match remaining.checked_sub(bytes) {
            Some(left) => {
                *remaining = left;
                Ok(())
            }
            None => Err(self.fail(reason)),
        }
    }
}

/// Which collection-wide budget one string is charged against.
#[derive(Clone, Copy)]
enum TextBudget {
    None,
    TurnText,
    Draft,
    Context,
}

/// Decode one string, rejecting it before it is owned if it does not fit.
struct BoundedText<'a> {
    state: &'a mut DecodeState,
    limit: usize,
    too_large: ConversationSnapshotError,
    budget: TextBudget,
}

impl<'de> DeserializeSeed<'de> for BoundedText<'_> {
    type Value = String;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for BoundedText<'_> {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a string of at most {} bytes", self.limit)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        if value.len() > self.limit {
            return Err(self.state.fail(self.too_large));
        }
        self.state.charge(self.budget, value.len())?;
        Ok(value.to_owned())
    }
}

/// `Option<String>` with the same bound, for nullable fields.
struct BoundedOptionalText<'a> {
    inner: BoundedText<'a>,
}

impl<'de> DeserializeSeed<'de> for BoundedOptionalText<'_> {
    type Value = Option<String>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de> Visitor<'de> for BoundedOptionalText<'_> {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "null or a string of at most {} bytes",
            self.inner.limit
        )
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        self.inner.deserialize(deserializer).map(Some)
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum TurnField {
    Role,
    Text,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum ContextField {
    Cmd,
    Output,
    Cwd,
    ExitCode,
    Truncated,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum ChatField {
    Id,
    Title,
    Archived,
    Turns,
    BlockContext,
    Draft,
    HistoryTruncated,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum SnapshotField {
    Version,
    ActiveChatId,
    Chats,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum LegacyField {
    Version,
    Turns,
    BlockContext,
}

const TURN_FIELDS: &[&str] = &["role", "text"];
const CONTEXT_FIELDS: &[&str] = &["cmd", "output", "cwd", "exit_code", "truncated"];
const CHAT_FIELDS: &[&str] = &[
    "id",
    "title",
    "archived",
    "turns",
    "block_context",
    "draft",
    "history_truncated",
];
const SNAPSHOT_FIELDS: &[&str] = &["version", "active_chat_id", "chats"];
const LEGACY_FIELDS: &[&str] = &["version", "turns", "block_context"];

struct TurnSeed<'a> {
    state: &'a mut DecodeState,
}

impl<'de> DeserializeSeed<'de> for TurnSeed<'_> {
    type Value = Turn;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_struct("Turn", TURN_FIELDS, self)
    }
}

impl<'de> Visitor<'de> for TurnSeed<'_> {
    type Value = Turn;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a conversation turn")
    }

    fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let state = self.state;
        let mut role = None;
        let mut text = None;
        while let Some(field) = map.next_key::<TurnField>()? {
            match field {
                TurnField::Role => {
                    if role.is_some() {
                        return Err(de::Error::duplicate_field("role"));
                    }
                    role = Some(map.next_value::<Role>()?);
                }
                TurnField::Text => {
                    if text.is_some() {
                        return Err(de::Error::duplicate_field("text"));
                    }
                    text = Some(map.next_value_seed(BoundedText {
                        state: &mut *state,
                        limit: MAX_TURN_BYTES,
                        too_large: ConversationSnapshotError::TurnTooLarge,
                        budget: TextBudget::TurnText,
                    })?);
                }
            }
        }
        Ok(Turn {
            role: role.ok_or_else(|| de::Error::missing_field("role"))?,
            text: text.ok_or_else(|| de::Error::missing_field("text"))?,
        })
    }
}

struct TurnsSeed<'a> {
    state: &'a mut DecodeState,
}

impl<'de> DeserializeSeed<'de> for TurnsSeed<'_> {
    type Value = Vec<Turn>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for TurnsSeed<'_> {
    type Value = Vec<Turn>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "at most {MAX_PERSISTED_TURNS} conversation turns"
        )
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let state = self.state;
        let mut turns = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_PERSISTED_TURNS));
        while turns.len() < MAX_PERSISTED_TURNS {
            let Some(turn) = seq.next_element_seed(TurnSeed { state: &mut *state })? else {
                return Ok(turns);
            };
            turns.push(turn);
        }
        // Prove the array is over-long without building turn 101.
        if seq.next_element::<de::IgnoredAny>()?.is_some() {
            return Err(state.fail(ConversationSnapshotError::InvalidTurnSequence));
        }
        Ok(turns)
    }
}

struct BlockContextSeed<'a> {
    state: &'a mut DecodeState,
}

impl<'de> DeserializeSeed<'de> for BlockContextSeed<'_> {
    type Value = BlockContext;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_struct("BlockContext", CONTEXT_FIELDS, self)
    }
}

impl<'de> Visitor<'de> for BlockContextSeed<'_> {
    type Value = BlockContext;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a selected block context")
    }

    fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let state = self.state;
        let mut cmd = None;
        let mut output = None;
        let mut cwd = None;
        let mut exit_code = None;
        let mut truncated = None;
        // The per-context limit applies to cmd + output + cwd together.
        let mut context_bytes = 0usize;
        while let Some(field) = map.next_key::<ContextField>()? {
            match field {
                ContextField::Cmd | ContextField::Output | ContextField::Cwd => {
                    let text = map.next_value_seed(BoundedOptionalText {
                        inner: BoundedText {
                            state: &mut *state,
                            limit: MAX_BLOCK_CONTEXT_BYTES,
                            too_large: ConversationSnapshotError::BlockContextTooLarge,
                            budget: TextBudget::Context,
                        },
                    })?;
                    context_bytes =
                        context_bytes.saturating_add(text.as_ref().map_or(0, String::len));
                    if context_bytes > MAX_BLOCK_CONTEXT_BYTES {
                        return Err(state.fail(ConversationSnapshotError::BlockContextTooLarge));
                    }
                    let slot = match field {
                        ContextField::Cmd => &mut cmd,
                        ContextField::Output => &mut output,
                        _ => &mut cwd,
                    };
                    if slot.is_some() {
                        return Err(de::Error::duplicate_field(match field {
                            ContextField::Cmd => "cmd",
                            ContextField::Output => "output",
                            _ => "cwd",
                        }));
                    }
                    *slot = Some(text);
                }
                ContextField::ExitCode => {
                    if exit_code.is_some() {
                        return Err(de::Error::duplicate_field("exit_code"));
                    }
                    exit_code = Some(map.next_value::<i32>()?);
                }
                ContextField::Truncated => {
                    if truncated.is_some() {
                        return Err(de::Error::duplicate_field("truncated"));
                    }
                    truncated = Some(map.next_value::<bool>()?);
                }
            }
        }
        // `cmd` and `output` are plain strings on the wire; only `cwd` is
        // nullable, so a null in either required field is a schema violation.
        let required = |field, value: Option<Option<String>>| {
            value
                .ok_or_else(|| de::Error::missing_field(field))?
                .ok_or_else(|| de::Error::invalid_type(de::Unexpected::Unit, &"a string"))
        };
        Ok(BlockContext {
            cmd: required("cmd", cmd)?,
            output: required("output", output)?,
            cwd: cwd.flatten(),
            exit_code: exit_code.ok_or_else(|| de::Error::missing_field("exit_code"))?,
            truncated: truncated.unwrap_or_default(),
        })
    }
}

struct OptionalBlockContextSeed<'a> {
    state: &'a mut DecodeState,
}

impl<'de> DeserializeSeed<'de> for OptionalBlockContextSeed<'_> {
    type Value = Option<BlockContext>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de> Visitor<'de> for OptionalBlockContextSeed<'_> {
    type Value = Option<BlockContext>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("null or a selected block context")
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        BlockContextSeed { state: self.state }
            .deserialize(deserializer)
            .map(Some)
    }
}

struct ChatSeed<'a> {
    state: &'a mut DecodeState,
}

impl<'de> DeserializeSeed<'de> for ChatSeed<'_> {
    type Value = ChatSnapshot;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_struct("ChatSnapshot", CHAT_FIELDS, self)
    }
}

impl<'de> Visitor<'de> for ChatSeed<'_> {
    type Value = ChatSnapshot;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a chat row")
    }

    fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let state = self.state;
        let mut id = None;
        let mut title = None;
        let mut archived = None;
        let mut turns = None;
        let mut block_context = None;
        let mut draft = None;
        let mut history_truncated = None;
        while let Some(field) = map.next_key::<ChatField>()? {
            match field {
                ChatField::Id => {
                    if id.is_some() {
                        return Err(de::Error::duplicate_field("id"));
                    }
                    id = Some(map.next_value::<u64>()?);
                }
                ChatField::Title => {
                    if title.is_some() {
                        return Err(de::Error::duplicate_field("title"));
                    }
                    title = Some(map.next_value_seed(BoundedText {
                        state: &mut *state,
                        limit: MAX_CHAT_TITLE_BYTES,
                        too_large: ConversationSnapshotError::InvalidChatTitle,
                        budget: TextBudget::None,
                    })?);
                }
                ChatField::Archived => {
                    if archived.is_some() {
                        return Err(de::Error::duplicate_field("archived"));
                    }
                    archived = Some(map.next_value::<bool>()?);
                }
                ChatField::Turns => {
                    if turns.is_some() {
                        return Err(de::Error::duplicate_field("turns"));
                    }
                    turns = Some(map.next_value_seed(TurnsSeed { state: &mut *state })?);
                }
                ChatField::BlockContext => {
                    if block_context.is_some() {
                        return Err(de::Error::duplicate_field("block_context"));
                    }
                    block_context =
                        Some(map.next_value_seed(OptionalBlockContextSeed { state: &mut *state })?);
                }
                ChatField::Draft => {
                    if draft.is_some() {
                        return Err(de::Error::duplicate_field("draft"));
                    }
                    draft = Some(map.next_value_seed(BoundedText {
                        state: &mut *state,
                        limit: MAX_CHAT_DRAFT_BYTES,
                        too_large: ConversationSnapshotError::ConversationTooLarge,
                        budget: TextBudget::Draft,
                    })?);
                }
                ChatField::HistoryTruncated => {
                    if history_truncated.is_some() {
                        return Err(de::Error::duplicate_field("history_truncated"));
                    }
                    history_truncated = Some(map.next_value::<bool>()?);
                }
            }
        }
        Ok(ChatSnapshot {
            id: id.ok_or_else(|| de::Error::missing_field("id"))?,
            title: title.ok_or_else(|| de::Error::missing_field("title"))?,
            archived: archived.unwrap_or_default(),
            turns: turns.unwrap_or_default(),
            block_context: block_context.flatten(),
            draft: draft.unwrap_or_default(),
            history_truncated: history_truncated.unwrap_or_default(),
        })
    }
}

struct ChatsSeed<'a> {
    state: &'a mut DecodeState,
}

impl<'de> DeserializeSeed<'de> for ChatsSeed<'_> {
    type Value = Vec<ChatSnapshot>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for ChatsSeed<'_> {
    type Value = Vec<ChatSnapshot>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "at most {MAX_PERSISTED_CHATS} chats")
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let state = self.state;
        let mut chats = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_PERSISTED_CHATS));
        while chats.len() < MAX_PERSISTED_CHATS {
            let Some(chat) = seq.next_element_seed(ChatSeed { state: &mut *state })? else {
                return Ok(chats);
            };
            chats.push(chat);
        }
        // Prove the array is over-wide without building chat 51.
        if seq.next_element::<de::IgnoredAny>()?.is_some() {
            return Err(state.fail(ConversationSnapshotError::TooManyChats));
        }
        Ok(chats)
    }
}

struct SnapshotSeed<'a> {
    state: &'a mut DecodeState,
}

impl<'de> DeserializeSeed<'de> for SnapshotSeed<'_> {
    type Value = ConversationSnapshot;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_struct("ConversationSnapshot", SNAPSHOT_FIELDS, self)
    }
}

impl<'de> Visitor<'de> for SnapshotSeed<'_> {
    type Value = ConversationSnapshot;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a conversation snapshot object")
    }

    fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let state = self.state;
        let mut version = None;
        let mut active_chat_id = None;
        let mut chats = None;
        while let Some(field) = map.next_key::<SnapshotField>()? {
            match field {
                SnapshotField::Version => {
                    if version.is_some() {
                        return Err(de::Error::duplicate_field("version"));
                    }
                    version = Some(map.next_value::<u32>()?);
                }
                SnapshotField::ActiveChatId => {
                    if active_chat_id.is_some() {
                        return Err(de::Error::duplicate_field("active_chat_id"));
                    }
                    active_chat_id = Some(map.next_value::<u64>()?);
                }
                SnapshotField::Chats => {
                    if chats.is_some() {
                        return Err(de::Error::duplicate_field("chats"));
                    }
                    chats = Some(map.next_value_seed(ChatsSeed { state: &mut *state })?);
                }
            }
        }
        Ok(ConversationSnapshot {
            version: version.ok_or_else(|| de::Error::missing_field("version"))?,
            active_chat_id: active_chat_id
                .ok_or_else(|| de::Error::missing_field("active_chat_id"))?,
            chats: chats.ok_or_else(|| de::Error::missing_field("chats"))?,
        })
    }
}

struct LegacySeed<'a> {
    state: &'a mut DecodeState,
}

impl<'de> DeserializeSeed<'de> for LegacySeed<'_> {
    type Value = LegacyConversationSnapshot;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_struct("LegacyConversationSnapshot", LEGACY_FIELDS, self)
    }
}

impl<'de> Visitor<'de> for LegacySeed<'_> {
    type Value = LegacyConversationSnapshot;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a version 1 conversation snapshot object")
    }

    fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let state = self.state;
        let mut version = None;
        let mut turns = None;
        let mut block_context = None;
        while let Some(field) = map.next_key::<LegacyField>()? {
            match field {
                LegacyField::Version => {
                    if version.is_some() {
                        return Err(de::Error::duplicate_field("version"));
                    }
                    version = Some(map.next_value::<u32>()?);
                }
                LegacyField::Turns => {
                    if turns.is_some() {
                        return Err(de::Error::duplicate_field("turns"));
                    }
                    turns = Some(map.next_value_seed(TurnsSeed { state: &mut *state })?);
                }
                LegacyField::BlockContext => {
                    if block_context.is_some() {
                        return Err(de::Error::duplicate_field("block_context"));
                    }
                    block_context =
                        Some(map.next_value_seed(OptionalBlockContextSeed { state: &mut *state })?);
                }
            }
        }
        Ok(LegacyConversationSnapshot {
            version: version.ok_or_else(|| de::Error::missing_field("version"))?,
            turns: turns.ok_or_else(|| de::Error::missing_field("turns"))?,
            block_context: block_context.flatten(),
        })
    }
}

fn decode_snapshot(encoded: &str) -> Result<ConversationSnapshot, ConversationSnapshotError> {
    let mut state = DecodeState::new();
    let mut deserializer = serde_json::Deserializer::from_str(encoded);
    let decoded = SnapshotSeed { state: &mut state }.deserialize(&mut deserializer);
    finish_decode(decoded, &mut deserializer, state)
}

fn decode_legacy_snapshot(
    encoded: &str,
) -> Result<LegacyConversationSnapshot, ConversationSnapshotError> {
    let mut state = DecodeState::new();
    let mut deserializer = serde_json::Deserializer::from_str(encoded);
    let decoded = LegacySeed { state: &mut state }.deserialize(&mut deserializer);
    finish_decode(decoded, &mut deserializer, state)
}

fn finish_decode<'de, T, R>(
    decoded: Result<T, serde_json::Error>,
    deserializer: &mut serde_json::Deserializer<R>,
    state: DecodeState,
) -> Result<T, ConversationSnapshotError>
where
    R: serde_json::de::Read<'de>,
{
    match decoded {
        Ok(value) => {
            deserializer
                .end()
                .map_err(|error| ConversationSnapshotError::InvalidJson(error.to_string()))?;
            Ok(value)
        }
        // A budget records the schema-specific reason; anything else is a
        // genuine JSON or shape failure.
        Err(error) => Err(state
            .error
            .unwrap_or_else(|| ConversationSnapshotError::InvalidJson(error.to_string()))),
    }
}

fn completed_history(history: &[Turn]) -> (Vec<Turn>, bool) {
    // Retain only the bounded newest window while scanning. Building an
    // intermediate Vec of every historical pair would let an otherwise valid
    // caller amplify a large live transcript before retention is applied.
    let mut retained = VecDeque::<(Turn, Turn, usize)>::new();
    let mut retained_bytes = 0usize;
    let mut history_truncated = false;
    for pair in history.chunks(2) {
        if pair.len() != 2 {
            // A final lone user turn is an ordinary in-flight request, not
            // persisted history loss.
            break;
        }
        let (user, assistant) = (&pair[0], &pair[1]);
        if user.role != Role::User || assistant.role != Role::Assistant {
            history_truncated = true;
            break;
        }
        if !valid_turn_text(&user.text) || !valid_turn_text(&assistant.text) {
            history_truncated = true;
            continue;
        }
        let pair_bytes = user.text.len().saturating_add(assistant.text.len());
        retained_bytes = retained_bytes.saturating_add(pair_bytes);
        retained.push_back((user.clone(), assistant.clone(), pair_bytes));
        while retained.len().saturating_mul(2) > MAX_PERSISTED_TURNS
            || retained_bytes > MAX_CHAT_TURN_TEXT_BYTES
        {
            let Some((_, _, removed_bytes)) = retained.pop_front() else {
                break;
            };
            retained_bytes = retained_bytes.saturating_sub(removed_bytes);
            history_truncated = true;
        }
    }

    let mut turns = Vec::with_capacity(retained.len().saturating_mul(2));
    for (user, assistant, _) in retained {
        turns.push(user);
        turns.push(assistant);
    }
    (turns, history_truncated)
}

fn validate_turns(turns: &[Turn]) -> Result<(), ConversationSnapshotError> {
    if turns.len() > MAX_PERSISTED_TURNS || turns.len() % 2 != 0 {
        return Err(ConversationSnapshotError::InvalidTurnSequence);
    }

    let mut total_bytes = 0usize;
    for (index, turn) in turns.iter().enumerate() {
        let expected = if index % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        if turn.role != expected || turn.text.trim().is_empty() {
            return Err(ConversationSnapshotError::InvalidTurnSequence);
        }
        if turn.text.len() > MAX_TURN_BYTES {
            return Err(ConversationSnapshotError::TurnTooLarge);
        }
        total_bytes = total_bytes.saturating_add(turn.text.len());
        if total_bytes > MAX_CHAT_TURN_TEXT_BYTES {
            return Err(ConversationSnapshotError::ConversationTooLarge);
        }
    }
    Ok(())
}

fn valid_turn_text(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_TURN_BYTES
}

fn context_bytes(context: &BlockContext) -> usize {
    context
        .cmd
        .len()
        .saturating_add(context.output.len())
        .saturating_add(context.cwd.as_ref().map_or(0, String::len))
}

fn valid_context(context: &BlockContext) -> bool {
    context_bytes(context) <= MAX_BLOCK_CONTEXT_BYTES
}

fn normalise_title(title: &str) -> String {
    let mut bounded = String::new();
    let mut char_count = 0usize;
    let mut pending_space = false;
    for ch in title.chars() {
        if ch.is_whitespace()
            || ch.is_control()
            || crate::review_input::is_visual_spoofing_character(ch)
        {
            pending_space |= !bounded.is_empty();
            continue;
        }
        let separator_bytes = usize::from(pending_space);
        if char_count.saturating_add(separator_bytes) >= MAX_CHAT_TITLE_CHARS
            || bounded
                .len()
                .saturating_add(separator_bytes)
                .saturating_add(ch.len_utf8())
                > MAX_CHAT_TITLE_BYTES
        {
            break;
        }
        if pending_space {
            bounded.push(' ');
            char_count += 1;
            pending_space = false;
        }
        bounded.push(ch);
        char_count += 1;
    }
    if bounded.is_empty() {
        DEFAULT_CHAT_TITLE.to_string()
    } else {
        bounded
    }
}

fn valid_title(title: &str) -> bool {
    !title.trim().is_empty()
        && title.len() <= MAX_CHAT_TITLE_BYTES
        && title.chars().count() <= MAX_CHAT_TITLE_CHARS
        && !title.chars().any(char::is_control)
        && !crate::review_input::contains_visual_spoofing(title)
}

fn bounded_draft(draft: &str) -> (String, bool) {
    if draft.len() <= MAX_CHAT_DRAFT_BYTES {
        return (draft.to_string(), false);
    }
    let mut boundary = MAX_CHAT_DRAFT_BYTES;
    while !draft.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (draft[..boundary].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: Role, text: impl Into<String>) -> Turn {
        Turn {
            role,
            text: text.into(),
        }
    }

    fn pair(index: usize) -> [Turn; 2] {
        [
            turn(Role::User, format!("question {index}")),
            turn(Role::Assistant, format!("answer {index}")),
        ]
    }

    fn chat(id: u64, title: &str, archived: bool, history: &[Turn]) -> ChatSnapshot {
        ChatSnapshot::from_completed_history(id, title, archived, history, None, "")
    }

    #[test]
    fn persists_only_complete_successful_pairs() {
        let mut history = pair(1).to_vec();
        history.push(turn(Role::User, "still in flight"));
        let context = BlockContext {
            cmd: "cargo test".into(),
            output: "ok".into(),
            cwd: Some("/tmp/project".into()),
            exit_code: 0,
            truncated: false,
        };

        let snapshot =
            ConversationSnapshot::from_completed_history(&history, Some(&context)).unwrap();
        assert_eq!(snapshot.turns(), &history[..2]);
        assert_eq!(snapshot.block_context(), Some(&context));
        assert!(!snapshot.active_chat().unwrap().history_truncated());
    }

    #[test]
    fn v1_json_migrates_to_one_v2_chat() {
        let encoded = r#"{"version":1,"turns":[{"role":"user","text":"  为什么失败？\n请解释 "},{"role":"assistant","text":"权限不足"}],"block_context":{"cmd":"cargo test","output":"failed","cwd":"/tmp/项目","exit_code":1}}"#;

        let migrated = ConversationSnapshot::from_json(encoded).unwrap();
        assert_eq!(migrated.active_chat_id(), 1);
        assert_eq!(migrated.chats().len(), 1);
        let migrated_chat = &migrated.chats()[0];
        assert_eq!(migrated_chat.id(), 1);
        assert_eq!(migrated_chat.title(), "为什么失败？ 请解释");
        assert_eq!(migrated_chat.turns().len(), 2);
        assert_eq!(migrated_chat.block_context().unwrap().exit_code, 1);

        let v2 = migrated.to_json().unwrap();
        assert!(v2.contains("\"version\":2"));
        assert_eq!(ConversationSnapshot::from_json(&v2).unwrap(), migrated);
    }

    #[test]
    fn v2_round_trip_preserves_empty_and_archived_chats() {
        let old = chat(7, "Old investigation", false, &pair(1));
        let empty_archived = chat(9, "Archived blank", true, &[]);
        let snapshot = ConversationSnapshot::from_chats(9, vec![old, empty_archived]).unwrap();

        let decoded = ConversationSnapshot::from_json(&snapshot.to_json().unwrap()).unwrap();
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.active_chat_id(), 9);
        assert!(decoded.chats()[1].archived());
        assert!(decoded.chats()[1].turns().is_empty());
    }

    #[test]
    fn collection_rejects_duplicate_missing_active_and_excess_chats() {
        let first = chat(1, "one", false, &[]);
        assert!(ConversationSnapshot::from_chats(2, vec![first.clone()]).is_none());
        assert!(ConversationSnapshot::from_chats(1, vec![first.clone(), first]).is_none());

        let chats: Vec<_> = (1..=(MAX_PERSISTED_CHATS as u64 + 1))
            .map(|id| chat(id, "bounded", false, &[]))
            .collect();
        assert!(ConversationSnapshot::from_chats(1, chats).is_none());

        let duplicate = r#"{"version":2,"active_chat_id":1,"chats":[{"id":1,"title":"one","turns":[]},{"id":1,"title":"again","turns":[]}]}"#;
        assert!(matches!(
            ConversationSnapshot::from_json(duplicate),
            Err(ConversationSnapshotError::DuplicateChatId(1))
        ));
        let missing =
            r#"{"version":2,"active_chat_id":2,"chats":[{"id":1,"title":"one","turns":[]}]}"#;
        assert!(matches!(
            ConversationSnapshot::from_json(missing),
            Err(ConversationSnapshotError::ActiveChatMissing(2))
        ));
    }

    #[test]
    fn decoder_strictly_rejects_invalid_ids_roles_and_future_versions() {
        let zero_id = r#"{"version":2,"active_chat_id":0,"chats":[{"id":0,"title":"bad","archived":false,"turns":[],"history_truncated":false}]}"#;
        assert!(matches!(
            ConversationSnapshot::from_json(zero_id),
            Err(ConversationSnapshotError::InvalidChatId)
        ));

        let bad_role = r#"{"version":2,"active_chat_id":1,"chats":[{"id":1,"title":"bad","archived":false,"turns":[{"role":"assistant","text":"wrong"},{"role":"user","text":"order"}],"history_truncated":false}]}"#;
        assert!(matches!(
            ConversationSnapshot::from_json(bad_role),
            Err(ConversationSnapshotError::InvalidTurnSequence)
        ));

        let control_title = r#"{"version":2,"active_chat_id":1,"chats":[{"id":1,"title":"bad\u0000title","turns":[]}]}"#;
        assert!(matches!(
            ConversationSnapshot::from_json(control_title),
            Err(ConversationSnapshotError::InvalidChatTitle)
        ));

        let future = r#"{"version":3,"active_chat_id":1,"chats":[]}"#;
        assert!(matches!(
            ConversationSnapshot::from_json(future),
            Err(ConversationSnapshotError::UnsupportedVersion(3))
        ));
    }

    #[test]
    fn decoder_rejects_unknown_fields_at_every_schema_level() {
        let payloads = [
            r#"{"version":2,"active_chat_id":1,"chats":[{"id":1,"title":"one","turns":[]}],"legacy_turns":[]}"#,
            r#"{"version":2,"active_chat_id":1,"chats":[{"id":1,"title":"one","turns":[],"mystery":true}]}"#,
            r#"{"version":2,"active_chat_id":1,"chats":[{"id":1,"title":"one","turns":[{"role":"user","text":"q","extra":1},{"role":"assistant","text":"a"}]}]}"#,
            r#"{"version":2,"active_chat_id":1,"chats":[{"id":1,"title":"one","turns":[{"role":"user","text":"q"},{"role":"assistant","text":"a"}],"block_context":{"cmd":"echo","output":"ok","cwd":null,"exit_code":0,"extra":1}}]}"#,
            r#"{"version":1,"turns":[{"role":"user","text":"q"},{"role":"assistant","text":"a"}],"chats":[]}"#,
        ];

        for payload in payloads {
            assert!(matches!(
                ConversationSnapshot::from_json(payload),
                Err(ConversationSnapshotError::InvalidJson(_))
            ));
        }
    }

    /// Wrap `chats` in a v2 document with chat 1 active.
    fn snapshot_json(chats: &str) -> String {
        format!(r#"{{"version":2,"active_chat_id":1,"chats":[{chats}]}}"#)
    }

    fn chat_json(id: u64, turns: &str) -> String {
        format!(r#"{{"id":{id},"title":"chat {id}","turns":[{turns}]}}"#)
    }

    fn pair_json(text: &str) -> String {
        format!(r#"{{"role":"user","text":"{text}"}},{{"role":"assistant","text":"a"}}"#)
    }

    #[test]
    fn decoding_stops_before_building_the_51st_chat_or_101st_turn() {
        let widest = (1..=MAX_PERSISTED_CHATS as u64)
            .map(|id| chat_json(id, &pair_json("q")))
            .collect::<Vec<_>>()
            .join(",");
        let decoded = ConversationSnapshot::from_json(&snapshot_json(&widest)).unwrap();
        assert_eq!(decoded.chats().len(), MAX_PERSISTED_CHATS);

        // Thousands of tiny chats still fit the encoded envelope; the seed
        // refuses the 51st before constructing it.
        let over_wide = (1..=MAX_PERSISTED_CHATS as u64 * 20)
            .map(|id| chat_json(id, ""))
            .collect::<Vec<_>>()
            .join(",");
        let json = snapshot_json(&over_wide);
        assert!(json.len() < MAX_CONVERSATION_SNAPSHOT_JSON_BYTES);
        assert_eq!(
            ConversationSnapshot::from_json(&json),
            Err(ConversationSnapshotError::TooManyChats)
        );

        let longest = (0..MAX_PERSISTED_TURNS / 2)
            .map(|index| pair_json(&format!("q{index}")))
            .collect::<Vec<_>>()
            .join(",");
        assert!(ConversationSnapshot::from_json(&snapshot_json(&chat_json(1, &longest))).is_ok());

        let over_long = (0..MAX_PERSISTED_TURNS)
            .map(|index| pair_json(&format!("q{index}")))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            ConversationSnapshot::from_json(&snapshot_json(&chat_json(1, &over_long))),
            Err(ConversationSnapshotError::InvalidTurnSequence)
        );
    }

    #[test]
    fn decoding_charges_per_field_budgets_with_their_own_error_categories() {
        let oversized_turn = "q".repeat(MAX_TURN_BYTES + 1);
        assert_eq!(
            ConversationSnapshot::from_json(&snapshot_json(&chat_json(
                1,
                &pair_json(&oversized_turn)
            ))),
            Err(ConversationSnapshotError::TurnTooLarge)
        );

        let oversized_title = "t".repeat(MAX_CHAT_TITLE_BYTES + 1);
        let chat = format!(
            r#"{{"id":1,"title":"{oversized_title}","turns":[{}]}}"#,
            pair_json("q")
        );
        assert_eq!(
            ConversationSnapshot::from_json(&snapshot_json(&chat)),
            Err(ConversationSnapshotError::InvalidChatTitle)
        );

        let oversized_draft = "d".repeat(MAX_CHAT_DRAFT_BYTES + 1);
        let chat = format!(
            r#"{{"id":1,"title":"one","turns":[{}],"draft":"{oversized_draft}"}}"#,
            pair_json("q")
        );
        assert_eq!(
            ConversationSnapshot::from_json(&snapshot_json(&chat)),
            Err(ConversationSnapshotError::ConversationTooLarge)
        );

        let oversized_output = "o".repeat(MAX_BLOCK_CONTEXT_BYTES + 1);
        let chat = format!(
            r#"{{"id":1,"title":"one","turns":[{}],"block_context":{{"cmd":"echo","output":"{oversized_output}","cwd":null,"exit_code":0}}}}"#,
            pair_json("q")
        );
        assert_eq!(
            ConversationSnapshot::from_json(&snapshot_json(&chat)),
            Err(ConversationSnapshotError::BlockContextTooLarge)
        );

        // cmd + output + cwd share one per-context limit.
        let half = "o".repeat(MAX_BLOCK_CONTEXT_BYTES / 2 + 1);
        let chat = format!(
            r#"{{"id":1,"title":"one","turns":[{}],"block_context":{{"cmd":"{half}","output":"{half}","cwd":null,"exit_code":0}}}}"#,
            pair_json("q")
        );
        assert_eq!(
            ConversationSnapshot::from_json(&snapshot_json(&chat)),
            Err(ConversationSnapshotError::BlockContextTooLarge)
        );
    }

    #[test]
    fn decoding_charges_cumulative_budgets_across_chats() {
        let turn = "q".repeat(MAX_TURN_BYTES);
        let turns_per_chat = 4usize;
        let chats_needed = MAX_ALL_CHAT_TURN_TEXT_BYTES / (MAX_TURN_BYTES * turns_per_chat) + 1;
        let chats = (1..=chats_needed as u64)
            .map(|id| {
                let turns = (0..turns_per_chat / 2)
                    .map(|_| {
                        format!(
                            r#"{{"role":"user","text":"{turn}"}},{{"role":"assistant","text":"{turn}"}}"#
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                chat_json(id, &turns)
            })
            .collect::<Vec<_>>()
            .join(",");
        let json = snapshot_json(&chats);
        assert!(json.len() < MAX_CONVERSATION_SNAPSHOT_JSON_BYTES);
        assert_eq!(
            ConversationSnapshot::from_json(&json),
            Err(ConversationSnapshotError::ConversationTooLarge)
        );

        let output = "o".repeat(MAX_BLOCK_CONTEXT_BYTES);
        let chats = (1..=(MAX_ALL_BLOCK_CONTEXT_BYTES / MAX_BLOCK_CONTEXT_BYTES + 1) as u64)
            .map(|id| {
                format!(
                    r#"{{"id":{id},"title":"chat {id}","turns":[{}],"block_context":{{"cmd":"","output":"{output}","cwd":null,"exit_code":0}}}}"#,
                    pair_json("q")
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            ConversationSnapshot::from_json(&snapshot_json(&chats)),
            Err(ConversationSnapshotError::BlockContextTooLarge)
        );
    }

    #[test]
    fn decoding_rejects_duplicate_fields_and_trailing_content() {
        let valid = snapshot_json(&chat_json(1, &pair_json("q")));
        assert!(ConversationSnapshot::from_json(&valid).is_ok());

        for duplicated in [
            valid.replace(
                r#""active_chat_id":1"#,
                r#""active_chat_id":1,"active_chat_id":2"#,
            ),
            valid.replace(r#""title":"chat 1""#, r#""title":"chat 1","title":"other""#),
            valid.replace(r#""role":"user""#, r#""role":"user","role":"assistant""#),
        ] {
            assert!(matches!(
                ConversationSnapshot::from_json(&duplicated),
                Err(ConversationSnapshotError::InvalidJson(message))
                    if message.contains("duplicate field")
            ));
        }

        assert!(matches!(
            ConversationSnapshot::from_json(&format!("{valid}{valid}")),
            Err(ConversationSnapshotError::InvalidJson(_))
        ));
    }

    #[test]
    fn per_chat_retention_keeps_newest_complete_pairs_and_bounds_titles() {
        let mut history = Vec::new();
        for index in 0..(MAX_PERSISTED_TURNS / 2 + 3) {
            history.extend(pair(index));
        }
        let long_title = "会".repeat(MAX_CHAT_TITLE_CHARS + 20);
        let retained =
            ChatSnapshot::from_completed_history(1, &long_title, false, &history, None, "draft");
        assert_eq!(retained.turns().len(), MAX_PERSISTED_TURNS);
        assert_eq!(retained.turns()[0].text, "question 3");
        assert_eq!(retained.turns().last().unwrap().text, "answer 52");
        assert!(retained.title().len() <= MAX_CHAT_TITLE_BYTES);
        assert!(retained.title().chars().count() <= MAX_CHAT_TITLE_CHARS);
        assert_eq!(retained.draft(), "draft");
        assert!(retained.history_truncated());
    }

    #[test]
    fn chat_titles_cannot_hide_or_reorder_list_chrome() {
        let history = pair(1);
        let chat = ChatSnapshot::from_completed_history(
            7,
            "  left\u{202e}right\u{00a0}tail  ",
            false,
            &history,
            None,
            "",
        );
        assert_eq!(chat.title(), "left right tail");

        let encoded = r#"{"version":2,"active_chat_id":7,"chats":[{"id":7,"title":"leftSPOOFright","turns":[{"role":"user","text":"q"},{"role":"assistant","text":"a"}]}]}"#
            .replace("SPOOF", "\u{202e}");
        assert_eq!(
            ConversationSnapshot::from_json(&encoded),
            Err(ConversationSnapshotError::InvalidChatTitle)
        );
    }

    #[test]
    fn draft_round_trips_and_oversized_unicode_draft_is_safely_bounded() {
        let draft = "准备下一步\n--flag=值";
        let with_draft =
            ChatSnapshot::from_completed_history(1, "draft chat", false, &pair(1), None, draft);
        let snapshot = ConversationSnapshot::from_chats(1, vec![with_draft]).unwrap();
        let decoded = ConversationSnapshot::from_json(&snapshot.to_json().unwrap()).unwrap();
        assert_eq!(decoded.chats()[0].draft(), draft);

        let oversized = "草".repeat(MAX_CHAT_DRAFT_BYTES);
        let bounded =
            ChatSnapshot::from_completed_history(2, "bounded draft", false, &[], None, &oversized);
        assert!(bounded.draft().len() <= MAX_CHAT_DRAFT_BYTES);
        assert!(bounded.draft().is_char_boundary(bounded.draft().len()));
        assert!(bounded.history_truncated());
    }

    #[test]
    fn encoded_budget_trims_hostile_old_drafts_last_without_deleting_rows() {
        // NUL expands to six bytes (`\u0000`) in JSON, so otherwise-valid
        // drafts can collectively exceed the encoded boundary.
        let hostile_draft = "\0".repeat(MAX_CHAT_DRAFT_BYTES);
        let chats: Vec<_> = (1..=22)
            .map(|id| {
                ChatSnapshot::from_completed_history(
                    id,
                    &format!("chat {id}"),
                    false,
                    &[],
                    None,
                    &hostile_draft,
                )
            })
            .collect();

        let snapshot = ConversationSnapshot::from_chats(22, chats).unwrap();
        assert_eq!(snapshot.chats().len(), 22);
        assert_eq!(snapshot.chats()[0].id(), 1);
        assert!(snapshot.chats()[0].draft().is_empty());
        assert!(snapshot.chats()[0].history_truncated());
        assert_eq!(snapshot.active_chat().unwrap().draft(), hostile_draft);
        assert!(snapshot.to_json().unwrap().len() <= MAX_CONVERSATION_SNAPSHOT_JSON_BYTES);
    }

    #[test]
    fn global_budget_trims_payload_without_deleting_chat_metadata() {
        let chunk = "x".repeat(MAX_TURN_BYTES);
        let history: Vec<_> = (0usize..8)
            .map(|index| {
                turn(
                    if index % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    chunk.clone(),
                )
            })
            .collect();
        let chats = vec![
            chat(1, "first", false, &history),
            chat(2, "second", true, &history),
            chat(3, "active", false, &history),
        ];

        let snapshot = ConversationSnapshot::from_chats(3, chats).unwrap();
        assert_eq!(snapshot.chats().len(), 3);
        assert_eq!(
            snapshot
                .chats()
                .iter()
                .map(ChatSnapshot::id)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(snapshot.chats()[0].history_truncated());
        assert!(snapshot.total_turn_text_bytes() <= MAX_ALL_CHAT_TURN_TEXT_BYTES);
        assert!(snapshot.to_json().unwrap().len() <= MAX_CONVERSATION_SNAPSHOT_JSON_BYTES);
    }

    #[test]
    fn global_compaction_clears_context_when_a_chat_loses_its_final_pair() {
        let chunk = "x".repeat(MAX_TURN_BYTES);
        let history: Vec<_> = (0usize..16)
            .map(|index| {
                turn(
                    if index % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    chunk.clone(),
                )
            })
            .collect();
        let context = BlockContext {
            cmd: "cargo test".into(),
            output: "ok".into(),
            cwd: Some("/tmp/project".into()),
            exit_code: 0,
            truncated: false,
        };
        let old =
            ChatSnapshot::from_completed_history(1, "old", false, &history, Some(&context), "");
        let active = chat(2, "active", false, &history);

        let snapshot = ConversationSnapshot::from_chats(2, vec![old, active]).unwrap();
        assert_eq!(snapshot.chats().len(), 2);
        assert!(snapshot.chats()[0].turns().is_empty());
        assert!(snapshot.chats()[0].block_context().is_none());
        assert!(snapshot.chats()[0].history_truncated());
        assert_eq!(snapshot.active_chat().unwrap().turns().len(), 16);
    }

    #[test]
    fn json_escape_expansion_trims_pairs_until_snapshot_is_encodable() {
        let escaped = "\\".repeat(MAX_TURN_BYTES);
        let history: Vec<_> = (0..(MAX_CHAT_TURN_TEXT_BYTES / MAX_TURN_BYTES))
            .map(|index| {
                turn(
                    if index % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    escaped.clone(),
                )
            })
            .collect();
        let original_len = history.len();
        let snapshot = ConversationSnapshot::from_chats(
            1,
            vec![ChatSnapshot::from_completed_history(
                1, "escaped", false, &history, None, "",
            )],
        )
        .unwrap();

        assert!(snapshot.chats()[0].turns().len() < original_len);
        assert!(snapshot.chats()[0].history_truncated());
        assert!(snapshot.to_json().is_ok());
    }

    #[test]
    fn oversized_turn_and_context_never_enter_a_constructed_chat() {
        let oversized = "x".repeat(MAX_TURN_BYTES + 1);
        let history = vec![turn(Role::User, oversized), turn(Role::Assistant, "answer")];
        let context = BlockContext {
            cmd: "cmd".into(),
            output: "x".repeat(MAX_BLOCK_CONTEXT_BYTES),
            cwd: None,
            exit_code: 0,
            truncated: false,
        };
        let snapshot =
            ChatSnapshot::from_completed_history(1, "bounded", false, &history, Some(&context), "");
        assert!(snapshot.turns().is_empty());
        assert!(snapshot.block_context().is_none());
        assert!(snapshot.history_truncated());
    }

    #[test]
    fn context_is_not_persisted_without_a_complete_exchange() {
        let context = BlockContext {
            cmd: "cargo test".into(),
            output: "running".into(),
            cwd: Some("/tmp/project".into()),
            exit_code: 0,
            truncated: false,
        };
        let in_flight = vec![turn(Role::User, "please inspect")];
        let snapshot = ChatSnapshot::from_completed_history(
            1,
            "in flight",
            false,
            &in_flight,
            Some(&context),
            "draft remains",
        );

        assert!(snapshot.turns().is_empty());
        assert!(snapshot.block_context().is_none());
        assert_eq!(snapshot.draft(), "draft remains");

        let orphaned = r#"{"version":2,"active_chat_id":1,"chats":[{"id":1,"title":"orphaned","turns":[],"block_context":{"cmd":"cargo test","output":"running","cwd":null,"exit_code":0}}]}"#;
        assert!(matches!(
            ConversationSnapshot::from_json(orphaned),
            Err(ConversationSnapshotError::InvalidTurnSequence)
        ));
    }
}
