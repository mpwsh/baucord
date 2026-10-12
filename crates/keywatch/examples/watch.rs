//! Watch F7 (a toggle), Ctrl+F8 (a toggle) and V (a hold) and print
//! their edges: `cargo run -p keywatch --example watch`. What a
//! machine check of the watcher runs (DEVICE-CHECKLIST.md).

use std::time::{Duration, Instant};

use keywatch::{platform_key, Binding, Watcher};

fn main() {
    let key = |code: &str| platform_key(code).expect("a key the watcher reads");
    let binding = |action, code, ctrl, hold| Binding {
        action,
        key: key(code),
        ctrl,
        shift: false,
        alt: false,
        hold,
    };
    let bindings = vec![
        binding("F7", "F7", false, false),
        binding("Ctrl+F8", "F8", true, false),
        binding("V (hold)", "KeyV", false, true),
    ];
    println!("access granted: {}", keywatch::access_granted());
    let start = Instant::now();
    let Some(watcher) = Watcher::start(bindings, move |action, pressed| {
        println!(
            "{:>8.3}s  {action} {}",
            start.elapsed().as_secs_f64(),
            if pressed { "down" } else { "up" }
        );
        true
    }) else {
        println!("the key state cannot be read here");
        return;
    };
    println!(
        "watching, {}",
        if watcher.by_events() {
            "woken by key events"
        } else {
            "on a clock"
        }
    );
    let secs = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    std::thread::sleep(Duration::from_secs(secs));
    drop(watcher);
    println!("stopped");
}
