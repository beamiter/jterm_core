//! Optional standalone entrypoint. Frontends also embed the same dispatcher,
//! so installing any terminal does not require installing this binary.
fn main() {
    if let Some(code) = jterm_core::agent_workspace::run_embedded_if_requested() {
        std::process::exit(code);
    }
    eprintln!("jterm-sessiond: use --jterm-session-host, or launch a session from a jterm Agent Workspace sidebar");
    std::process::exit(2);
}
