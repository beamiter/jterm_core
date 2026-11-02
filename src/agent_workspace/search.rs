//! Durable, read-only search evidence. Opening a hit never resumes a provider
//! or enlarges the context submitted to a model.
use super::{ConversationId, WorkspaceRow};
use serde::{Deserialize, Serialize};

pub const WORKSPACE_SEARCH_LIMIT: usize = 100;
pub const WORKSPACE_TEXT_BYTES: usize = 256 * 1024;
pub const WORKSPACE_TEXT_BATCH: usize = 32;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorkspaceSearchRequest {
    pub query: String,
    pub conversation: Option<ConversationId>,
    /// Stable document cursor. New documents are not injected into later pages.
    pub before: Option<u64>,
    pub limit: usize,
    pub include_archived: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum SearchSource {
    #[default]
    Metadata,
    ApiMessage,
    NativeTurn,
    ProviderHistory,
    ImportedHistory,
    Validation,
}
impl SearchSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Metadata => "Conversation metadata",
            Self::ApiMessage => "Saved API message",
            Self::NativeTurn => "Native task history",
            Self::ProviderHistory => "Provider history (indexed excerpt)",
            Self::ImportedHistory => "Imported history (read-only)",
            Self::Validation => "Validation evidence",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSearchAnchor {
    pub event_id: String,
    pub message_sequence: Option<u64>,
    pub revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceSearchHit {
    pub row: WorkspaceRow,
    pub anchor: WorkspaceSearchAnchor,
    pub source: SearchSource,
    pub snippet: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SearchCoverage {
    pub conversations: usize,
    pub indexed_documents: usize,
    pub pending_conversations: usize,
    pub pending_messages: usize,
    /// CLI/provider histories are explicitly indexed excerpts, not a claim
    /// that discovery sampled every message in the original history files.
    pub external_documents: usize,
    pub provider_history_complete: bool,
}
impl SearchCoverage {
    pub fn label(&self) -> String {
        format!(
            "{} conversations · {} indexed records · {} conversations / {} saved messages pending · provider history may be partial",
            self.conversations, self.indexed_documents, self.pending_conversations, self.pending_messages
        )
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorkspaceSearchPage {
    pub hits: Vec<WorkspaceSearchHit>,
    pub next_before: Option<u64>,
    pub coverage: SearchCoverage,
}

/// Text explicitly admitted for search, never executable input. Repeating the
/// same (conversation,event_id,revision) is idempotent. A changed revision
/// replaces the record instead of creating duplicate matches.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceTextDocument {
    pub conversation: ConversationId,
    pub event_id: String,
    pub revision: String,
    pub source: SearchSource,
    pub message_sequence: Option<u64>,
    pub text: String,
    pub truncated: bool,
}

pub(crate) fn snippet(text: &str, query: &str) -> String {
    let safe = crate::redact::redact_secrets(text);
    let terms: Vec<_> = query
        .split_whitespace()
        .map(|s| s.trim_matches('"'))
        .filter(|s| !s.is_empty() && !s.contains(':') && !s.starts_with('-'))
        .collect();
    let lower = safe.to_lowercase();
    let found = terms
        .iter()
        .filter_map(|term| lower.find(&term.to_lowercase()))
        .min()
        .unwrap_or(0);
    // Case folding can change byte length; map through char counts instead of
    // slicing the original string at an offset from its lowercase copy.
    let matched_chars = lower[..found].chars().count();
    let start = matched_chars.saturating_sub(48);
    let all: Vec<_> = safe.chars().collect();
    let end = (start + 300).min(all.len());
    let mut result = String::new();
    if start > 0 {
        result.push('…');
    }
    for ch in all.iter().skip(start).take(end.saturating_sub(start)) {
        if !ch.is_control() || matches!(*ch, '\n' | '\t') {
            result.push(*ch);
        }
    }
    if end < all.len() {
        result.push('…');
    }
    result
}
