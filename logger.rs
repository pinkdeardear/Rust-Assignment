use chrono::Local;
use std::sync::Mutex;
use std::time::Instant;
use std::sync::atomic::{AtomicBool, Ordering};

// Global mutable START_TIME protected by a Mutex
lazy_static::lazy_static! {
    static ref START_TIME: Mutex<Option<Instant>> = Mutex::new(None);
}

// Toggle logging on/off
static LOGGING_ENABLED: AtomicBool = AtomicBool::new(true);

pub fn disable_logging() {
    LOGGING_ENABLED.store(false, Ordering::SeqCst);
}

pub fn enable_logging() {
    LOGGING_ENABLED.store(true, Ordering::SeqCst);
}

pub fn init_logger() {
    let mut start = START_TIME.lock().unwrap();
    *start = Some(Instant::now());

    println!(
        "[{}] Logger initialized (elapsed: 0.000s)",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    );
}

pub fn log_event(event: &str) {
    if !LOGGING_ENABLED.load(Ordering::SeqCst) {
        return; // Skip logging if disabled
    }

    let elapsed_str = {
        let start = START_TIME.lock().unwrap();
        match *start {
            Some(start_time) => {
                let elapsed = start_time.elapsed();
                format!("{:.3}s", elapsed.as_secs_f64())
            }
            None => "N/A".to_string(),
        }
    };

    println!(
        "[{} | elapsed: {}] {}",
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        elapsed_str,
        event
    );
}
