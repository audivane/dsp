// No console window on Windows release builds (harmless elsewhere).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    audivane_dsp_ui::app::run();
}
