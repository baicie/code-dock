// Prevents additional console window on Windows in release, DOI does not matter in dev.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    codedock_desktop_lib::run()
}
