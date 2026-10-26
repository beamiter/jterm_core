//! Consumer boundary regression: delayed callbacks must not mutate a new task.
use jterm_core::agent::{AgentSession, ModelOutcome, ModelRequestError};

const REPLY: &str = r#"{"action":"run","command":"pwd"}"#;

#[test]
fn retry_discards_delayed_success_failure_and_duplicates() {
    let mut session = AgentSession::new(20);
    session.submit_user("inspect").unwrap();
    let old = session.model_request_token().unwrap();
    session.model_failed_for(&old, "timeout").unwrap();
    session.retry_model().unwrap();
    let current = session.model_request_token().unwrap();
    let before = format!("{:?}", session.transcript());
    assert_eq!(
        session.accept_model_reply_for(&old, REPLY),
        Err(ModelRequestError::StaleRequest)
    );
    assert_eq!(
        session.model_failed_for(&old, "late failure"),
        Err(ModelRequestError::StaleRequest)
    );
    assert_eq!(before, format!("{:?}", session.transcript()));
    assert!(current.is_active());
    let ModelOutcome::Proposal { id, .. } =
        session.accept_model_reply_for(&current, REPLY).unwrap()
    else {
        panic!("approval required")
    };
    assert_eq!(
        session.accept_model_reply_for(&current, REPLY),
        Err(ModelRequestError::StaleRequest)
    );
    assert_eq!(
        session.model_failed_for(&current, "duplicate"),
        Err(ModelRequestError::StaleRequest)
    );
    assert_eq!(session.approve(id).unwrap().command, "pwd");
}

#[test]
fn cross_session_and_restore_keep_tokens_process_local() {
    let mut original = AgentSession::new(20);
    original.submit_user("inspect").unwrap();
    let old = original.model_request_token().unwrap();
    let mut restored = AgentSession::restore(original.snapshot().unwrap()).unwrap();
    let current = restored.model_request_token().unwrap();
    assert_eq!(
        restored.accept_model_reply_for(&old, REPLY),
        Err(ModelRequestError::StaleRequest)
    );
    assert_eq!(
        restored.model_failed_for(&old, "late"),
        Err(ModelRequestError::StaleRequest)
    );
    assert_eq!(
        original.accept_model_reply_for(&current, REPLY),
        Err(ModelRequestError::StaleRequest)
    );
    original.cancel();
    assert!(!old.is_active());
    assert!(current.is_active());
    restored.accept_model_reply_for(&current, REPLY).unwrap();
}

#[test]
fn racing_failures_can_only_schedule_one_retry() {
    use std::sync::{Arc, Barrier, Mutex};
    let mut session = AgentSession::new(20);
    session.submit_user("inspect").unwrap();
    let request = session.model_request_token().unwrap();
    let shared = Arc::new(Mutex::new(session));
    let barrier = Arc::new(Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let shared = shared.clone();
            let barrier = barrier.clone();
            let request = request.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let mut session = shared.lock().unwrap();
                if session.model_failed_for(&request, "timeout").is_ok() {
                    session.retry_model().unwrap();
                    true
                } else {
                    false
                }
            })
        })
        .collect();
    assert_eq!(
        workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum::<usize>(),
        1
    );
    let mut session = shared.lock().unwrap();
    let current = session.model_request_token().unwrap();
    assert_eq!(
        session.accept_model_reply_for(&request, REPLY),
        Err(ModelRequestError::StaleRequest)
    );
    session.accept_model_reply_for(&current, REPLY).unwrap();
}

#[test]
fn cancelled_task_and_invalid_command_still_fail_closed() {
    let mut session = AgentSession::new(20);
    session.submit_user("inspect").unwrap();
    let old = session.model_request_token().unwrap();
    session.cancel();
    assert!(!old.is_active());
    session = AgentSession::new(20);
    session.submit_user("new task").unwrap();
    assert_eq!(
        session.accept_model_reply_for(&old, REPLY),
        Err(ModelRequestError::StaleRequest)
    );
    let current = session.model_request_token().unwrap();
    assert!(session
        .accept_model_reply_for(&current, r#"{"action":"run","command":"echo\nunsafe"}"#)
        .is_err());
}
