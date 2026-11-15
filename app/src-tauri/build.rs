fn main() {
    // Only the Tauri shell needs the generated context (tauri.conf.json,
    // capabilities, icons); the pure client builds without it.
    #[cfg(feature = "gui")]
    tauri_build::build();
}
