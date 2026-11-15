use super::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

/// A tiny fake daemon. `old` = refuses the `config`/`presets` topics.
struct Fake {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    topics: Arc<Mutex<Vec<Vec<String>>>>,
    conns: Arc<AtomicUsize>,
    _dir: tempfile::TempDir,
}

impl Fake {
    fn start(old: bool) -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let f = Fake {
            path,
            stop: Arc::default(),
            topics: Arc::default(),
            conns: Arc::default(),
            _dir: dir,
        };
        f.listen(old);
        f
    }

    fn listen(&self, old: bool) {
        let _ = std::fs::remove_file(&self.path);
        let listener = UnixListener::bind(&self.path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (stop, topics, conns) = (self.stop.clone(), self.topics.clone(), self.conns.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((s, _)) => {
                        s.set_nonblocking(false).unwrap();
                        conns.fetch_add(1, Ordering::SeqCst);
                        let (stop, topics) = (stop.clone(), topics.clone());
                        std::thread::spawn(move || serve(s, old, stop, topics));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
    }

    fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.path);
    }

    fn path_fn(&self) -> PathFn {
        let p = self.path.clone();
        Arc::new(move || p.clone())
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn serve(s: UnixStream, old: bool, stop: Arc<AtomicBool>, topics: Arc<Mutex<Vec<Vec<String>>>>) {
    let _ = s.set_read_timeout(Some(Duration::from_millis(20)));
    let writer = Arc::new(Mutex::new(s.try_clone().unwrap()));
    let mut reader = BufReader::new(s);
    let mut buf = String::new();
    let mut subscribed = false;
    let mut spectrum_subscribed = false;
    while !stop.load(Ordering::SeqCst) {
        if subscribed {
            let mut w = writer.lock().unwrap();
            if writeln!(w, r#"{{"event":"meters","data":{{"peak_db":[-10.0]}}}}"#).is_err() {
                return;
            }
            if spectrum_subscribed
                && writeln!(
                    w,
                    r#"{{"event":"spectrum","data":{{"f_min_hz":20.0,"bins_per_octave":24,"pre":[-20.0],"post":[-20.0],"rate":48000}}}}"#
                )
                .is_err()
            {
                return;
            }
        }
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => continue,
        }
        let v: Value = serde_json::from_str(buf.trim()).unwrap();
        let id = v["id"].as_u64().unwrap();
        let method = v["method"].as_str().unwrap().to_string();
        let reply = match method.as_str() {
            "echo" => {
                json!({"id": id, "ok": {"has_params": v.get("params").is_some(), "params": v.get("params")}})
            }
            "fail" => json!({"id": id, "error": {"code": 1, "message": "nope"}}),
            "slow" => {
                let w = writer.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(150));
                    let line = json!({"id": id, "ok": "slow"});
                    let _ = writeln!(w.lock().unwrap(), "{line}");
                });
                continue;
            }
            "upsampler_response" => {
                let taps = v["params"]["config"]["taps"].as_u64().unwrap_or(0);
                if taps >= 999 {
                    // Block this connection's handler loop synchronously, just like
                    // the real daemon handling one connection's requests in order.
                    let deadline = Instant::now() + Duration::from_millis(taps.min(500));
                    while Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    json!({"id": id, "ok": {"slow": true, "taps": taps}})
                } else {
                    json!({"id": id, "ok": {"fast": true, "taps": taps}})
                }
            }
            "subscribe" => {
                let t: Vec<String> = serde_json::from_value(v["params"]["topics"].clone()).unwrap();
                topics.lock().unwrap().push(t.clone());
                if old && t.iter().any(|x| x == "config") {
                    json!({"id": id, "error": {"code": -32602, "message": "unknown variant `config`"}})
                } else {
                    subscribed = true;
                    if t.iter().any(|x| x == "spectrum") {
                        spectrum_subscribed = true;
                    }
                    let line = json!({"id": id, "ok": null});
                    let mut w = writer.lock().unwrap();
                    writeln!(w, "{line}").unwrap();
                    if id == 1 || id == 2 {
                        writeln!(w, r#"{{"event":"status","data":{{"state":"ready"}}}}"#).unwrap();
                    }
                    continue;
                }
            }
            _ => json!({"id": id, "ok": method}),
        };
        let _ = writeln!(writer.lock().unwrap(), "{reply}");
    }
}

#[test]
fn params_presence_is_preserved() {
    let fake = Fake::start(false);
    let c = DaemonClient::new(fake.path_fn());
    let r = c.call("echo", None).unwrap();
    assert_eq!(r["has_params"], json!(false));
    let r = c.call("echo", Some(Value::Null)).unwrap();
    assert_eq!(r["has_params"], json!(true));
    assert_eq!(r["params"], Value::Null);
    let r = c
        .call_request(json!({"method": "echo", "params": {"a": 1}, "id": 99}))
        .unwrap();
    assert_eq!(r["params"], json!({"a": 1}));
    let r = c.call_request(json!({"method": "echo"})).unwrap();
    assert_eq!(r["has_params"], json!(false));
}

#[test]
fn errors_and_refusals() {
    let fake = Fake::start(false);
    let c = DaemonClient::new(fake.path_fn());
    assert_eq!(
        c.call("fail", None).unwrap_err(),
        CallError {
            code: 1,
            message: "nope".into()
        }
    );
    assert_eq!(
        c.call_request(json!({"method": "subscribe"}))
            .unwrap_err()
            .code,
        CallError::INVALID_REQUEST
    );
    assert_eq!(
        c.call_request(json!([1])).unwrap_err().code,
        CallError::INVALID_REQUEST
    );
}

#[test]
fn pipelined_calls_get_their_own_replies() {
    let fake = Fake::start(false);
    let c = Arc::new(DaemonClient::new(fake.path_fn()));
    let slow = {
        let c = c.clone();
        std::thread::spawn(move || c.call("slow", None))
    };
    std::thread::sleep(Duration::from_millis(30));
    // Answered while `slow` is still pending on the same connection.
    let t = Instant::now();
    assert_eq!(c.call("status", None).unwrap(), json!("status"));
    assert!(t.elapsed() < Duration::from_millis(140));
    assert_eq!(slow.join().unwrap().unwrap(), json!("slow"));
    assert_eq!(fake.conns.load(Ordering::SeqCst), 1);
}

#[test]
fn reconnects_after_the_daemon_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late.sock");
    let p = path.clone();
    let c = DaemonClient::new(Arc::new(move || p.clone()));
    let e = c.call("status", None).unwrap_err();
    assert_eq!(e.code, CallError::DISCONNECTED);
    assert!(
        e.message.contains("No engine is listening"),
        "{}",
        e.message
    );

    let fake = Fake::start(false);
    let p2 = fake.path.clone();
    let c = DaemonClient::new(Arc::new(move || p2.clone()));
    assert_eq!(c.call("status", None).unwrap(), json!("status"));
    fake.shutdown();
    std::thread::sleep(Duration::from_millis(60));
    // The old connection is closed by the server; the next call fails or
    // reconnects to the restarted daemon.
    let fake2 = Fake {
        path: fake.path.clone(),
        stop: Arc::default(),
        topics: Arc::default(),
        conns: Arc::default(),
        _dir: tempfile::tempdir().unwrap(),
    };
    fake2.listen(false);
    let t = Instant::now();
    loop {
        match c.call("status", None) {
            Ok(v) => {
                assert_eq!(v, json!("status"));
                break;
            }
            Err(_) if t.elapsed() < Duration::from_secs(2) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => panic!("did not reconnect: {e}"),
        }
    }
    drop(path);
}

fn collect(sub_path: PathFn) -> (Subscription, Arc<Mutex<Vec<SubMsg>>>) {
    let got: Arc<Mutex<Vec<SubMsg>>> = Arc::default();
    let g = got.clone();
    let sub = Subscription::spawn(sub_path, Arc::new(move |m| g.lock().unwrap().push(m)));
    (sub, got)
}

fn wait_for(got: &Mutex<Vec<SubMsg>>, pred: impl Fn(&[SubMsg]) -> bool) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        if pred(&got.lock().unwrap()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out; got {:?}", got.lock().unwrap());
}

#[test]
fn subscription_forwards_events_and_falls_back_to_base_topics() {
    let fake = Fake::start(true);
    let (mut sub, got) = collect(fake.path_fn());
    wait_for(&got, |m| {
        m.iter()
            .any(|x| matches!(x, SubMsg::Event(v) if v["event"] == "meters"))
    });
    let msgs = got.lock().unwrap().clone();
    assert_eq!(msgs[0], SubMsg::Connecting);
    assert!(msgs.contains(&SubMsg::Up));
    assert!(
        msgs.iter()
            .any(|x| matches!(x, SubMsg::Event(v) if v["event"] == "status"))
    );
    let topics = fake.topics.lock().unwrap().clone();
    assert_eq!(topics.len(), 2);
    assert!(topics[0].contains(&"config".to_string()));
    assert_eq!(
        topics[1],
        BASE_TOPICS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );
    sub.stop();
}

#[test]
fn subscription_reports_down_and_reconnects() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let p = path.clone();
    let (mut sub, got) = collect(Arc::new(move || p.clone()));
    wait_for(&got, |m| m.iter().any(|x| matches!(x, SubMsg::Down(_))));
    // Repeated failures with the same reason are reported once.
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(
        got.lock()
            .unwrap()
            .iter()
            .filter(|x| matches!(x, SubMsg::Down(_)))
            .count(),
        1
    );

    let fake = Fake {
        path: path.clone(),
        stop: Arc::default(),
        topics: Arc::default(),
        conns: Arc::default(),
        _dir: tempfile::tempdir().unwrap(),
    };
    fake.listen(false);
    wait_for(&got, |m| m.contains(&SubMsg::Up));
    assert_eq!(
        fake.topics.lock().unwrap()[0],
        ALL_TOPICS.iter().map(|s| s.to_string()).collect::<Vec<_>>()
    );
    fake.shutdown();
    wait_for(
        &got,
        |m| matches!(m.last(), Some(SubMsg::Down(r)) if r == "The engine stopped."),
    );
    sub.stop();
}

#[test]
fn subscription_toggles_spectrum_topic() {
    let fake = Fake::start(false);
    let (mut sub, got) = collect(fake.path_fn());
    wait_for(&got, |m| m.contains(&SubMsg::Up));
    assert!(!fake.topics.lock().unwrap()[0].contains(&"spectrum".to_string()));

    sub.set_spectrum(true);
    wait_for(&got, |m| {
        m.iter()
            .any(|x| matches!(x, SubMsg::Event(v) if v["event"] == "spectrum"))
    });
    assert!(
        fake.topics
            .lock()
            .unwrap()
            .iter()
            .any(|t| t == &vec!["spectrum".to_string()])
    );

    let before_conns = fake.conns.load(Ordering::SeqCst);
    sub.set_spectrum(false);
    // Reconnects without emitting Down and without `spectrum` in the new topic list.
    wait_for(&got, |_| fake.conns.load(Ordering::SeqCst) > before_conns);
    std::thread::sleep(Duration::from_millis(60));
    let msgs = got.lock().unwrap().clone();
    assert!(!msgs.iter().any(|m| matches!(m, SubMsg::Down(_))));
    let last_topics = fake.topics.lock().unwrap().last().cloned().unwrap();
    assert!(!last_topics.contains(&"spectrum".to_string()));
    sub.stop();
}

#[test]
fn slow_analysis_does_not_delay_control_calls() {
    let fake = Fake::start(false);
    let c = Arc::new(DaemonClient::new(fake.path_fn()));
    // Control calls open only the control connection (analysis stays lazy).
    assert_eq!(c.call("status", None).unwrap(), json!("status"));
    assert_eq!(fake.conns.load(Ordering::SeqCst), 1);

    let analysis = {
        let c = c.clone();
        std::thread::spawn(move || {
            c.call(
                "upsampler_response",
                Some(json!({"config": {"taps": 999}, "source_rate": 44100})),
            )
        })
    };
    std::thread::sleep(Duration::from_millis(40));

    // Control calls while `upsampler_response` is blocked on the daemon still finish immediately.
    let t = Instant::now();
    assert_eq!(c.call("set_volume", Some(json!({"volume_db": -10}))).unwrap(), json!("set_volume"));
    assert_eq!(c.call("set_dsp_enabled", Some(json!({"enabled": true}))).unwrap(), json!("set_dsp_enabled"));
    assert!(
        t.elapsed() < Duration::from_millis(120),
        "control calls took {:?} behind slow analysis",
        t.elapsed()
    );
    assert_eq!(fake.conns.load(Ordering::SeqCst), 2);

    let res = analysis.join().unwrap().unwrap();
    assert_eq!(res["slow"], json!(true));
}

#[test]
fn newer_analysis_supersedes_pending_one() {
    assert_eq!(timeout_for("upsampler_response"), Duration::from_secs(120));
    let fake = Fake::start(false);
    let c = Arc::new(DaemonClient::new(fake.path_fn()));

    let first_started = Instant::now();
    let first = {
        let c = c.clone();
        std::thread::spawn(move || {
            c.call(
                "upsampler_response",
                Some(json!({"config": {"taps": 999}, "source_rate": 44100})),
            )
        })
    };
    std::thread::sleep(Duration::from_millis(40));

    let second_started = Instant::now();
    let second = c
        .call(
            "upsampler_response",
            Some(json!({"config": {"taps": 100}, "source_rate": 48000})),
        )
        .unwrap();
    assert_eq!(second["fast"], json!(true));
    assert!(
        second_started.elapsed() < Duration::from_millis(150),
        "second analysis waited {:?}",
        second_started.elapsed()
    );

    let first_err = first.join().unwrap().unwrap_err();
    assert_eq!(first_err.code, CallError::SUPERSEDED);
    assert!(
        first_started.elapsed() < Duration::from_millis(250),
        "superseded analysis did not unblock promptly: {:?}",
        first_started.elapsed()
    );
}

