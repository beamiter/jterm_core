//! Explicit, previewable materials. Reading history never adds model context.
use super::ConversationId;
use serde::{Deserialize, Serialize};

pub const WORKSPACE_CONTEXT_BYTES: usize = 64 * 1024;
pub const WORKSPACE_MATERIAL_LIMIT: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MaterialKind {
    Note,
    FileExcerpt,
    Diff,
    Log,
    ConversationExcerpt,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMaterialSource {
    pub conversation: ConversationId,
    pub event_id: String,
    pub revision: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMaterial {
    pub id: String,
    pub label: String,
    pub kind: MaterialKind,
    pub text: String,
    pub source: Option<WorkspaceMaterialSource>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceContext {
    pub conversation: ConversationId,
    pub revision: u64,
    pub goal: String,
    pub completed: String,
    pub remaining: String,
    pub constraints: String,
    pub materials: Vec<WorkspaceMaterial>,
}

impl WorkspaceContext {
    pub fn empty(conversation: ConversationId) -> Self {
        Self {
            conversation,
            revision: 0,
            goal: String::new(),
            completed: String::new(),
            remaining: String::new(),
            constraints: String::new(),
            materials: Vec::new(),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.materials.len() > WORKSPACE_MATERIAL_LIMIT {
            return Err("A context packet supports at most 16 selected materials".into());
        }
        let mut bytes = 0usize;
        let mut identifiers = std::collections::HashSet::new();
        for text in [
            &self.goal,
            &self.completed,
            &self.remaining,
            &self.constraints,
        ] {
            validate_text(text)?;
            bytes = bytes.saturating_add(text.len());
        }
        for material in &self.materials {
            if material.id.is_empty()
                || material.id.len() > 128
                || material.label.len() > 256
                || !identifiers.insert(&material.id)
            {
                return Err("Each material needs a unique bounded identity and label".into());
            }
            validate_text(&material.id)?;
            validate_text(&material.label)?;
            validate_text(&material.text)?;
            bytes = bytes
                .saturating_add(material.text.len())
                .saturating_add(material.label.len());
        }
        if bytes > WORKSPACE_CONTEXT_BYTES {
            return Err(
                "Selected materials exceed the 64 KiB context budget; remove or shorten an excerpt"
                    .into(),
            );
        }
        Ok(())
    }
    /// Visible text assembled for a *new* target conversation. No provider
    /// thread ID, control lease, approval or runnable tool call is transferred.
    pub fn handoff(
        &self,
        target_provider: &str,
        title: &str,
        cwd: &str,
    ) -> Result<WorkspaceHandoff, String> {
        self.validate()?;
        if !matches!(
            target_provider.to_ascii_lowercase().as_str(),
            "codex" | "claude" | "kimi" | "api"
        ) {
            return Err("Choose a supported target provider for the new conversation".into());
        }
        if self.goal.trim().is_empty() {
            return Err("Describe the handoff goal before creating a new task".into());
        }
        let mut body = format!("Goal\n{}\n\nSource conversation: {}\nSource materials revision: {}\nTarget: {} (new conversation)\n",
            self.goal.trim(), self.conversation.0, self.revision, target_provider);
        for (label, text) in [
            ("Completed", &self.completed),
            ("Remaining", &self.remaining),
            ("Constraints", &self.constraints),
        ] {
            if !text.trim().is_empty() {
                body.push_str(&format!("\n{label}\n{}\n", text.trim()));
            }
        }
        for material in &self.materials {
            body.push_str(&format!(
                "\nMaterial: {} ({:?}{})\n",
                material.label,
                material.kind,
                if material.truncated { ", excerpt" } else { "" }
            ));
            if let Some(source) = &material.source {
                body.push_str(&format!(
                    "Source: {} / {} @ {}\n",
                    source.conversation.0, source.event_id, source.revision
                ));
            }
            body.push_str(&material.text);
            body.push('\n');
        }
        if body.len() > crate::agent_task::TASK_GOAL_MAX_BYTES {
            return Err("The handoff draft exceeds the 16 KiB task goal limit. Shorten or remove selected excerpts; the saved materials have not changed.".into());
        }
        Ok(WorkspaceHandoff {
            source: self.conversation.clone(),
            source_revision: self.revision,
            target_provider: target_provider.into(),
            title: title.into(),
            cwd: cwd.into(),
            body,
            materials_count: self.materials.len(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceHandoff {
    pub source: ConversationId,
    pub source_revision: u64,
    pub target_provider: String,
    pub title: String,
    pub cwd: String,
    pub body: String,
    pub materials_count: usize,
}

fn validate_text(text: &str) -> Result<(), String> {
    if text.chars().any(|ch| {
        (ch.is_control() && !matches!(ch, '\n' | '\t'))
            || (!ch.is_control() && crate::review_input::is_visual_spoofing_character(ch))
    }) {
        Err("Materials contain unsupported control or invisible formatting characters".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handoff_is_new_provider_text_with_exact_source_and_explicit_budget() {
        let mut context = WorkspaceContext::empty(ConversationId("source".into()));
        context.goal = "继续检查中文搜索\n保留多行材料".into();
        context.revision = 8;
        context.materials.push(WorkspaceMaterial {
            id: "m1".into(),
            label: "failure".into(),
            kind: MaterialKind::Log,
            text: "assertion failed".into(),
            source: Some(WorkspaceMaterialSource {
                conversation: ConversationId("source".into()),
                event_id: "turn:1".into(),
                revision: "r1".into(),
            }),
            truncated: false,
        });
        let packet = context.handoff("Claude", "Search", "/repo").unwrap();
        assert!(packet.body.contains("Claude (new conversation)"));
        assert!(packet.body.contains("turn:1 @ r1"));
        context.materials[0].text = "x".repeat(18 * 1024);
        assert!(context.handoff("Claude", "Search", "/repo").is_err());
        assert_eq!(context.materials[0].text.len(), 18 * 1024);
    }
}
