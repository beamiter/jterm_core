//! Presentation and input-ownership boundary for the optional Chat mode.
//!
//! Chat is a second presentation of a real, long-lived terminal session. It
//! does not execute commands, own an editor, observe keystrokes, or create a
//! second command history. The shell's existing PTY and terminal renderer
//! continue to own editing, completion, multiline input, passwords, and
//! interactive applications. Frontends keep their existing key/paste paths.
//!
//! Native frontends provide an already-observed [`ChatSessionState`] and use
//! [`resolve_surface`] to decide where that live terminal belongs. In a ready
//! shell prompt it is the docked composer; while a command runs it is a live
//! result/interaction surface. Alternate-screen programs take the full native
//! terminal. Missing shell boundaries also retain an ordinary native terminal
//! instead of guessing which output is a command or input prompt.
//!
//! This module deliberately receives no command text, output, or input bytes.
//! In particular, selecting a Chat surface cannot record a password, replay a
//! command, write a PTY, change its dimensions, or contact an AI service.

/// The family-wide opt-in value for the existing/new `terminal_mode` setting.
/// It is not a new default and must not be translated into `block` on save.
pub const CONFIG_VALUE: &str = "chat";

/// Live terminal facts already known to a frontend's existing lifecycle owner.
///
/// This is a snapshot, not another parser or state machine. The frontend must
/// refresh it after processing a PTY batch and before applying a composer
/// button action. A cached idle prompt must not authorize submission to a
/// foreground program that started in the meantime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChatSessionState {
    /// The current session has usable shell lifecycle boundaries. Merely
    /// configuring a shell or having seen integration in an earlier session
    /// is insufficient. Clear this when boundary authority is lost.
    pub shell_integration: bool,
    /// The existing shell lifecycle owner identifies the current surface as a
    /// ready prompt. This may contain an unfinished or multiline command;
    /// "ready" does not mean "empty", nor does it authorize an agent to run it.
    pub prompt_ready: bool,
    /// A foreground command has started and the lifecycle owner has not yet
    /// observed its completion. A simultaneous stale `prompt_ready` is ignored.
    pub command_running: bool,
    /// The native terminal currently owns an alternate screen. This overrides
    /// every other field, including missing shell integration.
    pub alternate_screen: bool,
}

/// Which native live-terminal surface Chat mode should currently present.
///
/// These are temporary presentation states. None changes the user's selected
/// `terminal_mode`, replaces the PTY, or changes terminal input semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatSurface {
    /// Dock the existing live shell terminal in the bottom multiline composer.
    /// Completed command/result pairs remain in the independent transcript.
    Prompt,
    /// The running command owns the existing live terminal and all stdin.
    /// Render it as the active result/interaction surface without a second
    /// editable composer. Non-alternate-screen `read`, `cat`, and REPLs remain
    /// usable, including hidden input. Keyboard Enter remains native input.
    Running,
    /// Shell boundaries are unavailable or the prompt is between lifecycle
    /// states. Keep the existing native terminal usable without manufacturing
    /// command bubbles or capturing input. Restore Chat on a later valid prompt.
    Passthrough,
    /// Give the native alternate-screen terminal the entire pane. Hide Chat
    /// chrome and preserve keyboard, mouse, selection, and resize behavior.
    Fullscreen,
}

impl ChatSurface {
    /// Whether the live terminal may be presented as the pinned composer.
    ///
    /// This does not authorize synthesizing text. The composer is the real
    /// terminal, not a local editor mirrored back into the shell.
    #[must_use]
    pub const fn composer_docked(self) -> bool {
        matches!(self, Self::Prompt)
    }

    /// Whether a visible Chat "send" button may invoke the existing shell
    /// Enter action once. Re-evaluate from live state when the button fires.
    ///
    /// This gates only the optional button, never keyboard input. In every
    /// state the native terminal retains its normal Enter, Ctrl-C, EOF, paste,
    /// IME, and application-input behavior. Do not copy/re-paste the visible
    /// prompt, append a second Enter, or clear input before a confirmed write.
    #[must_use]
    pub const fn allows_submit_affordance(self) -> bool {
        matches!(self, Self::Prompt)
    }
}

/// Select Chat presentation conservatively from existing terminal facts.
///
/// Alternate-screen ownership wins unconditionally. Without usable shell
/// boundaries there can be no safe separation of command and output, so the
/// native terminal passes through. A running command wins over a stale prompt.
/// Finally, a between-boundaries state passes through rather than pretending
/// that a command completed or a prompt is ready.
#[must_use]
pub const fn resolve_surface(state: ChatSessionState) -> ChatSurface {
    if state.alternate_screen {
        ChatSurface::Fullscreen
    } else if !state.shell_integration {
        ChatSurface::Passthrough
    } else if state.command_running {
        ChatSurface::Running
    } else if state.prompt_ready {
        ChatSurface::Prompt
    } else {
        ChatSurface::Passthrough
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_surface, ChatSessionState, ChatSurface, CONFIG_VALUE};

    const READY: ChatSessionState = ChatSessionState {
        shell_integration: true,
        prompt_ready: true,
        command_running: false,
        alternate_screen: false,
    };

    #[test]
    fn config_value_is_an_independent_opt_in_mode() {
        assert_eq!(CONFIG_VALUE, "chat");
        assert_ne!(CONFIG_VALUE, "block");
    }

    #[test]
    fn startup_and_missing_boundaries_keep_native_input() {
        assert_eq!(
            resolve_surface(ChatSessionState::default()),
            ChatSurface::Passthrough
        );
        assert_eq!(
            resolve_surface(ChatSessionState {
                shell_integration: false,
                ..READY
            }),
            ChatSurface::Passthrough
        );
        assert_eq!(
            resolve_surface(ChatSessionState {
                prompt_ready: false,
                ..READY
            }),
            ChatSurface::Passthrough
        );
    }

    #[test]
    fn foreground_input_wins_over_a_stale_prompt() {
        let state = ChatSessionState {
            command_running: true,
            ..READY
        };
        assert_eq!(resolve_surface(state), ChatSurface::Running);
        assert!(!resolve_surface(state).composer_docked());
        assert!(!resolve_surface(state).allows_submit_affordance());
    }

    #[test]
    fn full_screen_wins_without_shell_integration() {
        assert_eq!(
            resolve_surface(ChatSessionState {
                alternate_screen: true,
                ..ChatSessionState::default()
            }),
            ChatSurface::Fullscreen
        );
    }

    #[test]
    fn complete_command_and_alternate_screen_round_trip() {
        let mut state = READY;
        assert_eq!(resolve_surface(state), ChatSurface::Prompt);
        state.prompt_ready = false;
        state.command_running = true;
        assert_eq!(resolve_surface(state), ChatSurface::Running);
        state.alternate_screen = true;
        assert_eq!(resolve_surface(state), ChatSurface::Fullscreen);
        state.alternate_screen = false;
        assert_eq!(resolve_surface(state), ChatSurface::Running);
        state.command_running = false;
        assert_eq!(resolve_surface(state), ChatSurface::Passthrough);
        state.prompt_ready = true;
        assert_eq!(resolve_surface(state), ChatSurface::Prompt);
    }

    #[test]
    fn integration_loss_recovers_without_replacing_the_session() {
        let mut state = READY;
        state.shell_integration = false;
        assert_eq!(resolve_surface(state), ChatSurface::Passthrough);
        state.shell_integration = true;
        assert_eq!(resolve_surface(state), ChatSurface::Prompt);
    }

    #[test]
    fn every_boolean_combination_has_one_safe_surface() {
        for bits in 0..16 {
            let state = ChatSessionState {
                shell_integration: bits & 1 != 0,
                prompt_ready: bits & 2 != 0,
                command_running: bits & 4 != 0,
                alternate_screen: bits & 8 != 0,
            };
            let surface = resolve_surface(state);
            let safe_composer = state.shell_integration
                && state.prompt_ready
                && !state.command_running
                && !state.alternate_screen;
            assert_eq!(surface.composer_docked(), safe_composer, "{state:?}");
            assert_eq!(
                surface.allows_submit_affordance(),
                safe_composer,
                "{state:?}"
            );
            assert_eq!(
                surface == ChatSurface::Fullscreen,
                state.alternate_screen,
                "{state:?}"
            );
            assert_eq!(
                surface == ChatSurface::Running,
                state.shell_integration && state.command_running && !state.alternate_screen,
                "{state:?}"
            );
        }
    }

    #[test]
    fn only_prompt_exposes_chat_submit_without_restricting_native_input() {
        for surface in [
            ChatSurface::Prompt,
            ChatSurface::Running,
            ChatSurface::Passthrough,
            ChatSurface::Fullscreen,
        ] {
            assert_eq!(
                surface.allows_submit_affordance(),
                surface == ChatSurface::Prompt
            );
        }
    }
}
