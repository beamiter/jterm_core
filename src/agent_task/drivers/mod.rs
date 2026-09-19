//! Provider adapters for the native Agent runtime.

pub mod claude_stream_json;
pub mod codex_app_server;
pub mod fake;
pub mod kimi_stream_json;
mod print_stream;

pub use claude_stream_json::{
    claude_print_argv, parse_stream_json_line, ClaudeStreamJsonDriver, ClaudeStreamSignal,
};
pub use codex_app_server::{
    CodexAppServerApproval, CodexAppServerApprovalFileChange, CodexAppServerApprovalKind,
    CodexAppServerCommandView, CodexAppServerExitCause, CodexAppServerExitReport,
    CodexAppServerFileChange, CodexAppServerFileChangeView, CodexAppServerPhase,
    CodexAppServerProcessExit, CodexAppServerTurnCommandSummary, CodexAppServerTurnFileSummary,
    CodexAppServerTurnHistory, CodexAppServerViewSnapshot, CODEX_APP_SERVER_LIVE_TURN_MAX,
    CODEX_APP_SERVER_TURN_HISTORY_CAPACITY, CODEX_APP_SERVER_TURN_HISTORY_MAX_BYTES,
};
#[allow(unused_imports)] // test-support adapter, exercised by driver tests
pub use fake::{FakeAgentDriver, FakeAgentEvent, FakeAgentProgress};
pub use kimi_stream_json::{
    kimi_print_argv, parse_stream_json_line as parse_kimi_stream_json_line, KimiStreamJsonDriver,
    KimiStreamSignal,
};
