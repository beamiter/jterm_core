//! Explicit user-authored task instructions, separate from terminal evidence.
//! A brief does not execute anything. Creation, provider start, validation and
//! review remain separate actions owned by the existing task runtime.

use super::replay_text::{sanitize_history_replay, MAX_REPLAY_COMMAND_BYTES};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const TASK_GOAL_MAX_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskValidationRecipe {
    pub command: String,
    /// Immutable absolute shell selected when the user creates the task.
    pub shell: String,
}

impl TaskValidationRecipe {
    pub fn new(command: impl Into<String>, shell: impl Into<String>) -> Result<Self, String> {
        let recipe = Self {
            command: command.into(),
            shell: shell.into(),
        };
        recipe.validate()?;
        Ok(recipe)
    }

    pub fn validate(&self) -> Result<(), String> {
        let sanitized = sanitize_history_replay(&self.command, MAX_REPLAY_COMMAND_BYTES)
            .map_err(|error| format!("Invalid validation command: {error}"))?;
        if sanitized != self.command || self.command.contains(['\n', '\r']) {
            return Err(
                "Validation requires one exact, single-line command without control characters."
                    .into(),
            );
        }
        if self.shell.len() > 4096
            || !Path::new(&self.shell).is_absolute()
            || self.shell.chars().any(char::is_control)
            || crate::review_input::contains_visual_spoofing(&self.shell)
        {
            return Err("Choose an absolute, bounded shell path for validation.".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskBrief {
    pub goal: String,
    /// None means explicit human review, never an implicit passing validation.
    pub validation: Option<TaskValidationRecipe>,
}

impl TaskBrief {
    pub fn new(
        goal: impl Into<String>,
        validation: Option<TaskValidationRecipe>,
    ) -> Result<Self, String> {
        let mut brief = Self {
            goal: goal.into(),
            validation,
        };
        brief.validate()?;
        brief.goal = brief.goal.trim().to_string();
        Ok(brief)
    }

    /// Revalidate decoded or caller-constructed briefs at every execution
    /// boundary. Public fields are convenient to render, not a trust token.
    pub fn validate(&self) -> Result<(), String> {
        if self.goal.trim().is_empty() || self.goal.len() > TASK_GOAL_MAX_BYTES {
            return Err("Describe the task goal in 1 to 16,384 UTF-8 bytes.".into());
        }
        if self
            .goal
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
            || crate::review_input::contains_noncontrol_visual_spoofing(&self.goal)
        {
            return Err(
                "The task goal contains unsupported control or invisible formatting characters."
                    .into(),
            );
        }
        if let Some(validation) = &self.validation {
            validation.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authored_goal_preserves_multiline_unicode_and_explicit_recipe() {
        let recipe = TaskValidationRecipe::new("cargo test --locked", "/bin/bash").unwrap();
        let brief =
            TaskBrief::new("  改进会话管理\n\tKeep draft identity.  ", Some(recipe)).unwrap();
        assert_eq!(brief.goal, "改进会话管理\n\tKeep draft identity.");
        let decoded: TaskBrief =
            serde_json::from_str(&serde_json::to_string(&brief).unwrap()).unwrap();
        assert_eq!(brief, decoded);
        decoded.validate().unwrap();
    }

    #[test]
    fn goal_and_recipe_reject_unsafe_or_oversized_inputs() {
        for goal in [
            " ",
            "goal\u{1b}[2J",
            "goal\rnext",
            "go\u{202e}al",
            "go\u{200b}al",
        ] {
            assert!(TaskBrief::new(goal, None).is_err(), "{goal:?}");
        }
        assert!(TaskBrief::new("a".repeat(TASK_GOAL_MAX_BYTES), None).is_ok());
        assert!(TaskBrief::new("a".repeat(TASK_GOAL_MAX_BYTES + 1), None).is_err());
        for command in [
            "",
            "true\nfalse",
            "true\rfalse",
            "echo\u{1b}[31m unsafe",
            "tr\u{202e}ue",
        ] {
            assert!(
                TaskValidationRecipe::new(command, "/bin/bash").is_err(),
                "{command:?}"
            );
        }
        for shell in ["bash", "", "/bin/bash\n", "/bin/ba\u{200b}sh"] {
            assert!(TaskValidationRecipe::new("true", shell).is_err());
        }
    }

    #[test]
    fn decoded_public_fields_still_need_boundary_validation() {
        let brief: TaskBrief = serde_json::from_str(r#"{"goal":"","validation":null}"#).unwrap();
        assert!(brief.validate().is_err());
        assert!(serde_json::from_str::<TaskBrief>(
            r#"{"goal":"review","validation":null,"auto_start":true}"#
        )
        .is_err());
    }
}
