//! Live check of the volume keys' portal client, without the GUI.
//!
//! `cargo run -p audivane-dsp-ui --example shortcuts_probe` registers the
//! three volume shortcuts through the desktop's GlobalShortcuts portal
//! (KDE asks to confirm), then prints every state change and each step
//! (with repeat) until Ctrl+C. Without a portal it prints why.

use audivane_dsp_ui::shortcuts::{self, KeyEvent};
use std::sync::Mutex;
use std::sync::mpsc::Sender;

fn main() {
    let steps = shortcuts::spawn_repeater(|a| println!("step: {a:?}"));
    let steps: Mutex<Sender<KeyEvent>> = Mutex::new(steps);
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let done = Mutex::new(Some(done_tx));
    let _keys = shortcuts::start_portal(
        |s| println!("state: {}", serde_json::to_string(&s).unwrap_or_default()),
        move |e| {
            println!("key: {e:?}");
            let _ = steps.lock().unwrap().send(e);
        },
        move |why| {
            println!("no portal: {why}");
            if let Some(tx) = done.lock().unwrap().take() {
                let _ = tx.send(());
            }
        },
    );
    // Runs until Ctrl+C, or ends when there is no portal at all.
    let _ = done_rx.recv();
}
