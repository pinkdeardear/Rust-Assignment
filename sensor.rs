use crate::types::SensorData;
use chrono::Utc;
use crossbeam_channel::{Sender};
use log::{info, warn};
use rand::Rng;
use std::thread;
use std::time::{Duration, Instant};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

pub fn run_sensor(
    name: String,
    priority: u8,
    interval: Duration,           // <- new parameter
    tx: Sender<SensorData>,
    fault_flag: Arc<AtomicBool>,
    fault_start: Arc<Mutex<Option<Instant>>>,
    shutdown_flag: Arc<AtomicBool>,
){
    thread::spawn(move || {
        let mut rng = rand::thread_rng();
        let mut cycle = 0;

        // Safety monitoring
        let mut consecutive_jitter_violations = 0;
        let mut consecutive_missed_cycles = 0;


        let start_time = Instant::now();

        info!("[{}] 🟢 Sensor thread started (interval {:?})", name, interval);

        while !shutdown_flag.load(Ordering::SeqCst) {
            let expected_time = start_time + cycle * interval;

            // === Sleep until next scheduled cycle ===
            let now = Instant::now();
            if expected_time > now {
                let sleep_dur = expected_time - now;
                if sleep_dur > Duration::from_micros(500) {
                    // Sleep most of the time
                    thread::sleep(sleep_dur - Duration::from_micros(500));
                }
                // Spin-wait last few hundred microseconds
                while Instant::now() < expected_time {
                    std::hint::spin_loop();
                }
            } else {
                // Running late
                warn!(
                    "[{}] ⚠️ Running late by {:.3}ms",
                    name,
                    (now - expected_time).as_secs_f64() * 1000.0
                );
            }

            // Measure jitter after sleep/work
            let actual_time = Instant::now();
            let jitter = if actual_time > expected_time {
                actual_time - expected_time
            } else {
                expected_time - actual_time
            };

            cycle += 1;

            // === Simulate sensor data ===
            let roll = rng.gen_range(0..100);
            let injecting_fault = fault_flag.load(Ordering::SeqCst);
            let value = if injecting_fault {
                if rng.gen_bool(0.5) {
                    -9999.0
                } else {
                    // Simulate minimal delay only
                    rng.gen_range(0.0..100.0)
                }
            } else {
                if roll < 10 { -9999.0 } else { rng.gen_range(0.0..100.0) }
            };

            // set queued_at immediately after value generation
            let gen_time = Utc::now();


            // === CRITICAL SENSOR: enforce jitter and track invalid data ===
            if name == "TempSensor" {
                let is_violation = jitter > Duration::from_millis(1) || value == -9999.0;

                if is_violation {
                    consecutive_jitter_violations += 1;
                    consecutive_missed_cycles += 1; // keep for your existing logs

                    warn!(
                        "[{}] ⚠️ VIOLATION! Cycle {} | Jitter = {:.3}ms | Value = {:.2} | Consecutive Missed = {}",
                        name,
                        cycle,
                        jitter.as_secs_f64() * 1000.0,
                        value,
                        consecutive_jitter_violations
                    );

                    if consecutive_jitter_violations > 3 {
                        warn!(
                            "[{}] 🚨 SAFETY ALERT: missed {} consecutive critical cycles!",
                            name,
                            consecutive_jitter_violations
                        );
                    }
                } else {
                    info!(
                        "[{}] ✅ Jitter OK: {:.3}ms | Value OK: {:.2}",
                        name,
                        jitter.as_secs_f64() * 1000.0,
                        value
                    );
                    consecutive_jitter_violations = 0; // reset counter
                    consecutive_missed_cycles = 0; // reset old counter
                }

                crate::monitor::record_sensor_jitter(&name, jitter);
            } else {
                info!(
                    "[{}] Cycle {} | Jitter = {:.4}ms",
                    name,
                    cycle,
                    jitter.as_secs_f64() * 1000.0
                );
            }

            let gen_time = Utc::now();
            let data = SensorData {
                name: name.clone(),
                value,
                generated_at: gen_time,
                queued_at: gen_time,
                priority,
            };

            if value == -9999.0 {
                warn!("[{}] INVALID data: value={}", name, value);
            } else {
                let unit = match name.as_str() {
                    "TempSensor" => "°C",
                    "AccelSensor" => "m/s²",
                    "GyroSensor" => "°/s",
                    _ => "",
                };
                info!("[{}] value={:.2}{} | priority={}", name, value, unit, priority);
            }

            // === Send to task buffer ===
            if let Err(e) = tx.send(data.clone()) {
                warn!("[{}] ❌ ERROR sending data: {}", name, e);
                break;
            }
        }

        info!("[{}] ✅ Sensor thread ended.", name);
    });
}
