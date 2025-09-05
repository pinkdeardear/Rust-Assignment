use chrono::Local;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Global mutable START_TIME protected by a Mutex
lazy_static::lazy_static! {
    static ref START_TIME: Mutex<Option<Instant>> = Mutex::new(None);
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
