//! Terminal-native associations for the JWM session launcher. The GUI supplies
//! only sessions actually attached to its live panes; this bridge never opens a
//! PTY, runs a provider, infers completion from titles, or resumes history.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
        },
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub session_id: String,
    pub pane_id: String,
    pub project_id: String,
    pub title: String,
    pub status: String,
}
impl Session {
    pub fn from_agent(
        session: &crate::agent_sessions::AgentSession,
        pane_id: String,
    ) -> Option<Self> {
        let id = crate::agent_workspace::session_id(session).ok()?;
        Some(Self {
            session_id: id.0,
            pane_id,
            project_id: session.cwd.to_string_lossy().into_owned(),
            title: session.title.clone(),
            status: match session.status {
                crate::agent_sessions::SessionStatus::Working => "running",
                crate::agent_sessions::SessionStatus::Waiting => "waiting",
                _ => "unknown",
            }
            .into(),
        })
    }
}
#[derive(Clone, Debug)]
pub struct Reveal {
    pub session_id: String,
    pub pane_id: String,
    receipt: Value,
    expected: Session,
}

/// One bridge per native window. Absent/ambiguous native identity stays
/// unavailable. PID is used once to discover a unique current window; every
/// subsequent request is bound to the WM's non-reusable lifetime token.
#[derive(Debug)]
pub struct Bridge {
    sessions: Arc<Mutex<Vec<Session>>>,
    reveals: Receiver<Reveal>,
    replies: SyncSender<Reveal>,
    stop: Arc<AtomicBool>,
}
impl Bridge {
    pub fn new(
        provider: &str,
        native_window_id: Option<u64>,
        wake: impl Fn() + Send + 'static,
    ) -> Self {
        Self::with_endpoint(provider, native_window_id, wake, endpoint())
    }
    fn with_endpoint(
        provider: &str,
        native_window_id: Option<u64>,
        wake: impl Fn() + Send + 'static,
        path: PathBuf,
    ) -> Self {
        let sessions = Arc::new(Mutex::new(Vec::<Session>::new()));
        let (events, reveals) = sync_channel(32);
        let (replies, acks) = sync_channel::<Reveal>(32);
        let stop = Arc::new(AtomicBool::new(false));
        let (state, quit) = (sessions.clone(), stop.clone());
        let provider = provider.to_owned();
        let _=std::thread::Builder::new().name("jterm-desktop-sessions".into()).spawn(move||{
            let host=format!("{}:{}",provider,uuid::Uuid::new_v4());
            let incarnation=uuid::Uuid::new_v4().to_string();
            let mut bound:Option<(String,String)>=None;
            let mut retired_wm=None;
            let mut registered=BTreeMap::<String,(Session,u64)>::new();
            let mut sequence=0u64;let mut seen=BTreeSet::<String>::new();
            let mut pending=BTreeMap::<String,Value>::new();
            let mut heartbeat=Instant::now()-Duration::from_secs(6);
            while !quit.load(Ordering::Acquire) {
                let snapshot=state.lock().unwrap_or_else(|e|e.into_inner()).clone();
                if snapshot.is_empty() && registered.is_empty(){std::thread::sleep(Duration::from_millis(250));continue;}
                let attempt=(||->io::Result<()>{
                    let context=ipc(&path,"query","get_execution_context",json!({}))?;
                    let wm=context["wm_instance_id"].as_str().filter(|s|!s.is_empty()).ok_or_else(||io::Error::other("missing_wm_identity"))?.to_owned();
                    if bound.as_ref().is_some_and(|(old,_)|old!=&wm){bound=None;registered.clear();pending.clear();seen.clear();retired_wm=None;}
                    if bound.is_none(){
                        if retired_wm.as_ref()==Some(&wm){return Err(io::Error::other("native_window_lifetime_ended"));}
                        let window=unique_window(&context,std::process::id(),native_window_id,&provider)?;
                        bound=Some((wm.clone(),window));
                    }
                    let window=bound.as_ref().unwrap().1.clone();
                    if !context["windows"].as_array().is_some_and(|rows|rows.iter().any(|w|w["target_token"]==window)){
                        retired_wm=Some(wm);bound=None;registered.clear();pending.clear();return Err(io::Error::other("native_window_gone"));
                    }
                    let live=snapshot.iter().map(|s|s.session_id.clone()).collect::<BTreeSet<_>>();
                    let removed=registered.keys().filter(|id|!live.contains(*id)).cloned().collect::<Vec<_>>();
                    for id in removed{let _=ipc(&path,"command","unregister_agent_session",json!({"wm_instance_id":wm,"session_id":id,"host_id":host,"host_incarnation":incarnation}));registered.remove(&id);}
                    let refresh=heartbeat.elapsed()>Duration::from_secs(4);
                    for session in &snapshot{
                        if session.session_id.len()>256||session.pane_id.len()>256||session.project_id.len()>256||session.title.len()>512{continue;}
                        let changed=registered.get(&session.session_id).is_none_or(|(old,_)|old!=session);
                        if changed||refresh{
                            let revision=if changed{sequence=sequence.checked_add(1).ok_or_else(||io::Error::other("revision_exhausted"))?;sequence}else{registered[&session.session_id].1};
                            ipc(&path,"command","register_agent_session",json!({"wm_instance_id":wm,"session":{"session_id":session.session_id,"project_id":session.project_id,"provider":provider,"host_id":host,"host_incarnation":incarnation,"pane_id":session.pane_id,"window_token":window,"title":session.title,"status":session.status,"can_reveal":true,"revision":revision}}))?;
                            registered.insert(session.session_id.clone(),(session.clone(),revision));
                        }
                    }
                    if refresh{heartbeat=Instant::now();}
                    while let Ok(reveal)=acks.try_recv(){
                        let id=reveal.receipt["operation_id"].as_str().unwrap_or("").to_owned();
                        if let Some(original)=pending.remove(&id){
                            let mut receipt=original;receipt["wm_instance_id"]=json!(wm);receipt["status"]=reveal.receipt["status"].clone();
                            let _=ipc(&path,"command","agent_session_revealed",receipt);
                        }
                    }
                    let requests=ipc(&path,"query","get_agent_session_requests",json!({"host_id":host,"host_incarnation":incarnation}))?;
                    let queued=requests["requests"].as_array().into_iter().flatten().filter(|r|r["status"]=="queued"||r["status"]=="claimed").filter_map(|r|r["operation_id"].as_str()).collect::<BTreeSet<_>>();
                    pending.retain(|id,_|queued.contains(id.as_str()));
                    for request in requests["requests"].as_array().into_iter().flatten(){
                        if request["status"]!="queued" {continue;}
                        let (Some(operation),Some(id),Some(pane))=(request["operation_id"].as_str(),request["session_id"].as_str(),request["pane_id"].as_str())else{continue;};
                        let Some((session,revision))=registered.get(id)else{continue;};
                        if request["window_token"]!=window||request["revision"]!=*revision||session.pane_id!=pane||request["host_id"]!=host||request["host_incarnation"]!=incarnation||seen.contains(operation){continue;}
                        if seen.len()>=128{seen.retain(|id|pending.contains_key(id));}
                        if seen.len()>=128{continue;}
                        // Atomically claim before a GUI callback can change panes.
                        // Cancellation before this boundary forbids dispatch;
                        // after it, cancellation cannot promise to revoke an
                        // already issued native UI event. A lost claim reply is
                        // not retried as another GUI dispatch.
                        let mut claim=request.clone();claim["wm_instance_id"]=json!(wm);
                        let claimed=ipc(&path,"command","claim_agent_session_reveal",claim)?;
                        if claimed["status"]!="claimed"||claimed["already_claimed"]==true{continue;}
                        seen.insert(operation.into());
                        if events.try_send(Reveal{session_id:id.into(),pane_id:pane.into(),receipt:request.clone(),expected:session.clone()}).is_ok(){pending.insert(operation.into(),request.clone());wake();}
                    }
                    Ok(())
                })();
                std::thread::sleep(if attempt.is_ok(){Duration::from_millis(250)}else{Duration::from_secs(1)});
            }
            if let Some((wm,_))=bound{for id in registered.keys(){let _=ipc(&path,"command","unregister_agent_session",json!({"wm_instance_id":wm,"session_id":id,"host_id":host,"host_incarnation":incarnation}));}}
        });
        Self {
            sessions,
            reveals,
            replies,
            stop,
        }
    }
    pub fn publish(&self, mut sessions: Vec<Session>) {
        sessions.truncate(1000);
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        sessions.dedup_by(|a, b| a.session_id == b.session_id);
        let mut snapshot = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if *snapshot != sessions {
            *snapshot = sessions;
        }
    }
    pub fn poll(&self) -> Vec<Reveal> {
        let current = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        self.reveals
            .try_iter()
            .take(32)
            .filter(|request| current.iter().any(|session| session == &request.expected))
            .collect()
    }
    pub fn acknowledge(&self, mut request: Reveal, revealed: bool) {
        request.receipt["status"] = json!(if revealed { "revealed" } else { "failed" });
        let _ = self.replies.try_send(request);
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}
fn unique_window(
    context: &Value,
    pid: u32,
    native: Option<u64>,
    provider: &str,
) -> io::Result<String> {
    let mut matches = context["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| {
            let class = w["class"].as_str().unwrap_or("").to_ascii_lowercase();
            w["pid"].as_u64() == Some(pid as u64)
                && native.is_none_or(|id| w["native_window_id"].as_u64() == Some(id))
                && (class == provider
                    || class == format!("io.github.beamiter.{provider}")
                    || class == format!("org.{provider}.{provider}"))
        });
    let value = matches
        .next()
        .and_then(|w| w["target_token"].as_str())
        .ok_or_else(|| io::Error::other("native_window_not_registered"))?;
    if matches.next().is_some() {
        return Err(io::Error::other("native_window_identity_ambiguous"));
    }
    Ok(value.into())
}
fn endpoint() -> PathBuf {
    std::env::var_os("JWM_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(format!("/tmp/jwm-{}", unsafe { libc::geteuid() }))
                })
                .join("jwm-ipc.sock")
        })
}
fn ipc(path: &std::path::Path, kind: &str, name: &str, args: Value) -> io::Result<Value> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other("unsafe_wm_endpoint"));
    }
    let mut stream = connect(path)?;
    let timeout = Some(Duration::from_millis(500));
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;
    let request = if kind == "command" {
        json!({"command":name,"args":args})
    } else {
        json!({"query":name,"args":args})
    };
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    let mut bytes = Vec::new();
    let limit = 1024 * 1024;
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut buffer = [0u8; 8192];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "wm_frame_timeout"))?;
        stream.set_read_timeout(Some(remaining))?;
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(io::Error::other("truncated_wm_frame"));
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > limit {
            return Err(io::Error::other("oversized_wm_frame"));
        }
        if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
            bytes.truncate(end);
            break;
        }
    }
    let reply: Value = serde_json::from_slice(&bytes)?;
    if reply["success"] != true {
        return Err(io::Error::other("wm_refused_session_request"));
    }
    Ok(reply["data"].clone())
}
fn connect(path: &std::path::Path) -> io::Result<UnixStream> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::other("invalid_wm_endpoint"));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as _;
    }
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let result = unsafe {
        libc::connect(
            fd,
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as _,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EAGAIN)
        ) {
            return Err(error);
        }
        let mut poll = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, 500) } <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "wm_connect_timeout",
            ));
        }
        if let Some(error) = stream.take_error()? {
            return Err(error);
        }
        if error.raw_os_error() == Some(libc::EAGAIN) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "wm_backlog_full"));
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_time_binding_requires_unique_live_native_identity() {
        let mut context = json!({"windows":[{"pid":42,"id":"1011","native_window_id":7,"class":"Forge","target_token":"lifetime:one"}]});
        assert_eq!(
            unique_window(&context, 42, None, "forge").unwrap(),
            "lifetime:one"
        );
        assert!(unique_window(&context, 43, None, "forge").is_err());
        context["windows"]
            .as_array_mut()
            .unwrap()
            .push(json!({"pid":42,"id":"1012","native_window_id":8,"class":"Forge","target_token":"lifetime:two"}));
        assert!(unique_window(&context, 42, None, "forge").is_err());
        assert_eq!(
            unique_window(&context, 42, Some(8), "forge").unwrap(),
            "lifetime:two"
        );
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    #[test]
    fn worker_registers_live_pane_and_requires_gui_receipt_for_completion() {
        let directory =
            std::env::temp_dir().join(format!("jterm-desktop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("wm.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let deliveries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (shutdown, completed, calls) = (stop.clone(), done.clone(), deliveries.clone());
        let worker = std::thread::spawn(move || {
            let mut session = Value::Null;
            while !shutdown.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let name = request["query"]
                    .as_str()
                    .or_else(|| request["command"].as_str())
                    .unwrap();
                let data = match name {
                    "get_execution_context" => {
                        json!({"wm_instance_id":"wm-test","windows":[{"pid":std::process::id(),"id":"1011","native_window_id":7,"class":"forge","target_token":"wm-test:window:incarnation1"}]})
                    }
                    "register_agent_session" => {
                        session = request["args"]["session"].clone();
                        assert_eq!(session["pane_id"], "pane-stable");
                        json!({})
                    }
                    "get_agent_session_requests" => {
                        if session.is_null() {
                            json!({"requests":[]})
                        } else {
                            let mut r = session.clone();
                            r["operation_id"] = json!("wm-test:agent:1");
                            r["status"] = json!("queued");
                            calls.fetch_add(1, Ordering::AcqRel);
                            json!({"requests":[r]})
                        }
                    }
                    "claim_agent_session_reveal" => {
                        json!({"status":"claimed","already_claimed":false})
                    }
                    "agent_session_revealed" => {
                        assert_eq!(request["args"]["status"], "revealed");
                        assert_eq!(
                            request["args"]["window_token"],
                            "wm-test:window:incarnation1"
                        );
                        completed.store(true, Ordering::Release);
                        json!({})
                    }
                    "unregister_agent_session" => json!({}),
                    _ => panic!("unexpected IPC {name}"),
                };
                writeln!(stream, "{}", json!({"success":true,"data":data})).unwrap();
            }
        });
        let bridge = Bridge::with_endpoint("forge", Some(7), || {}, socket);
        bridge.publish(vec![Session {
            session_id: "provider-session".into(),
            pane_id: "pane-stable".into(),
            project_id: "project".into(),
            title: "project".into(),
            status: "waiting".into(),
        }]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let request = loop {
            if let Some(request) = bridge.poll().pop() {
                break request;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            !done.load(Ordering::Acquire),
            "queued request is not a GUI reveal"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            bridge.poll().is_empty(),
            "polling cannot re-execute an already delivered request"
        );
        bridge.acknowledge(request, true);
        while !done.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(deliveries.load(Ordering::Acquire) >= 1);
        drop(bridge);
        stop.store(true, Ordering::Release);
        worker.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
