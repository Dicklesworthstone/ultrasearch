//! GPUI desktop entry point (placeholder).

use core_types::config::load_or_create_config;

fn main() {
    if let Err(e) = load_or_create_config(None) {
        eprintln!("Failed to load configuration: {}", e);
        return;
    }
    println!("UltraSearch UI placeholder – GPUI wiring pending.");
}
