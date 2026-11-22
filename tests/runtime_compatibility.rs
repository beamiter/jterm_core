#[test]
fn old_and_new_paths_share_type_identity_and_callbacks() {
    use std::sync::{Arc, Mutex};
    let order = Arc::new(Mutex::new(vec![42]));
    let out = order.clone();
    let old = jterm_core::runtime_wake::WakeSink::new(move || {
        assert_eq!(*out.lock().unwrap(), [42]);
    });
    let new: jterm_runtime::runtime_wake::WakeSink = old;
    std::thread::spawn(move || new.wake()).join().unwrap();
}

#[test]
fn old_exit_paths_honor_existing_const_points_and_exit_names() {
    use jterm_core::exit_status::{interrupt_signal, is_job_stop, signal_name_for_exit};
    const STOP: bool = is_job_stop(148);
    const INT: Option<&str> = interrupt_signal(130);
    assert_eq!(STOP, jterm_runtime::exit_status::is_job_stop(148));
    assert_eq!(INT, Some("SIGINT"));
    assert_eq!(signal_name_for_exit(148), Some("SIGTSTP"));
}

#[test]
fn old_process_path_names_the_runtime_snapshot_type() {
    use jterm_core::process::{parse_process_stat, process_stat, process_stat_result, ProcessStat};
    let parsed: ProcessStat = parse_process_stat("9 (z) Z 1 9 9 0 -1 0 0").unwrap();
    let runtime: jterm_runtime::process_stat::ProcessStat = parsed;
    assert!(!runtime.is_live());
    assert_eq!(
        process_stat_result(0).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput,
    );
    assert_eq!(process_stat(0), None);
}

#[test]
fn old_argv_probe_keeps_the_independent_runtime_api() {
    let old: fn(i32) -> Option<Vec<String>> = jterm_core::process::read_proc_cmdline;
    assert_eq!(old(0), None);
    assert_eq!(old(-1), jterm_runtime::process_argv::read_proc_cmdline(-1));
    #[cfg(target_os = "linux")]
    {
        let pid = i32::try_from(std::process::id()).unwrap();
        let own = old(pid).unwrap();
        assert!(!own[0].is_empty());
        assert_eq!(
            Some(own),
            jterm_runtime::process_argv::read_proc_cmdline(pid)
        );
    }
}
