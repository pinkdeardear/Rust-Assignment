use tokio::time::Instant;

use chrono::Local;

pub fn current_timestamp() -> String {
    String::new()
}


pub fn log_duration(start: Instant, label: &str) {
    let duration = start.elapsed();
    println!(">>> {} took {:.2?} seconds", label, duration);
}
//async