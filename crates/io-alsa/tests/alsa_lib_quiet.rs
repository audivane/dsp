//! alsa-lib prints some failures itself (to stderr, through its error
//! handler). On the audio thread those go to a counter instead.

use audivane_io_alsa::{AlsaConfig, AlsaOutput, alsa_lib_errors, silence_alsa_lib_on_this_thread};

fn open_missing() {
    let cfg = AlsaConfig {
        device: "hw:CARD=NoSuchCardForThisTest,DEV=0".into(),
        rate: 48_000,
        channels: 2,
        channel_map: Vec::new(),
        period_frames: 1024,
        buffer_periods: 2,
        preferred_format: None,
        strict_format: false,
        nonblock: true,
    };
    assert!(AlsaOutput::open(&cfg).is_err());
}

#[test]
fn alsa_lib_messages_on_a_silenced_thread_are_counted_not_printed() {
    std::thread::spawn(|| {
        silence_alsa_lib_on_this_thread();
        let before = alsa_lib_errors();
        // alsa-lib reports the missing card itself ("cannot find card").
        open_missing();
        assert!(
            alsa_lib_errors() > before,
            "alsa-lib's message did not reach the counter"
        );
    })
    .join()
    .unwrap();
}
