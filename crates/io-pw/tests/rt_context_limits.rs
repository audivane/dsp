//! PipeWire's RT module, loaded by a default client context from PipeWire
//! 1.4.0 on, sets `RLIMIT_RTTIME` soft = hard unless the context names the
//! engine's override config; with it, soft stays at half the hard limit.
//!
//! The contexts are local: nothing connects to a PipeWire server. Its own
//! file, because the limits are per process. With no rtkit, or a PipeWire
//! whose `client.conf` does not load the RT module (1.0.5, 1.2.8), the
//! negative
//! control sees no change and the test says so and passes.

use audivane_io_alsa::{rtkit_rttime_max_usec, set_rttime_limits};
use audivane_io_pw::rt_context::context_properties_in;
use pipewire as pw;
use std::time::{Duration, Instant};

fn limits() -> (u64, u64) {
    let mut cur = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut cur) };
    (cur.rlim_cur, cur.rlim_max)
}

/// Iterate the loop for `span`, or until `done`.
fn run_until(main_loop: &pw::main_loop::MainLoopRc, span: Duration, done: impl Fn() -> bool) {
    let t0 = Instant::now();
    while t0.elapsed() < span && !done() {
        main_loop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
    }
}

#[test]
fn the_rt_module_keeps_the_soft_limit_with_the_override() {
    let Some(max) = rtkit_rttime_max_usec() else {
        eprintln!("rt_context_limits: no rtkit; skipped");
        return;
    };
    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None).unwrap();
    let half = max / 2;

    // Negative control: a plain context's RT module sets soft = hard.
    set_rttime_limits(max).unwrap();
    assert_eq!(limits(), (half, max));
    let plain = pw::context::ContextRc::new(&main_loop, None).unwrap();
    let t0 = Instant::now();
    run_until(&main_loop, Duration::from_secs(3), || limits().0 == max);
    let took = t0.elapsed();
    drop(plain);
    if limits().0 != max {
        eprintln!(
            "rt_context_limits: this PipeWire's client context leaves the limits alone; skipped"
        );
        return;
    }
    eprintln!("rt_context_limits: plain context set soft = hard after {took:?}");

    // With the override the module sets the engine's limits. Start from a
    // soft limit of a quarter, so the test sees the module set the half.
    let quarter = libc::rlimit {
        rlim_cur: max / 4,
        rlim_max: max,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &quarter) }, 0);
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("rt-context-limits");
    let props = context_properties_in(Some(dir.clone())).expect("override properties");
    let ctx = pw::context::ContextRc::new(&main_loop, Some(props)).unwrap();
    // Give the module several times what it took above.
    run_until(&main_loop, (took * 5).max(Duration::from_secs(2)), || {
        limits().0 == half
    });
    assert_eq!(
        limits(),
        (half, max),
        "soft limit kept at half the hard one"
    );
    drop(ctx);
    eprintln!(
        "rt_context_limits: context with {} kept soft = {half} us, hard = {max} us",
        dir.join(audivane_io_pw::rt_context::OVERRIDE_NAME)
            .display()
    );
}
