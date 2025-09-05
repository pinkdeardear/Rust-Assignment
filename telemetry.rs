use std::thread;
use std::time::{Duration, Instant};
use crossbeam_channel::{Sender, Receiver};
use crate::types::{WirePacket, xor_decrypt_from_hex, ENC_KEY};
use crate::downlink::{trigger_command_scheduler, report_cpu_utilization};
use crate::fault::{is_command_safe, handle_fault};
use crate::monitor::{start_monitoring, record_telemetry_latency_and_jitter, record_missing_telemetry, log_jitter};
use crate::utils::logger::log_event;
use crate::utils::timing::current_timestamp;
use chrono::{DateTime, Utc};
use serde_json;


#[derive(Debug, Clone)]
pub enum TelemetryEvent {
    PacketReceived { packet_id: u64, latency_ms: u64, drift_us: i128 },
    PacketMissing(u64),
    ContactLost,
}

pub fn start_telemetry_system(serialized_rx: Receiver<String>) -> Sender<TelemetryEvent> {
    let (tx, _rx) = crossbeam_channel::unbounded::<TelemetryEvent>();
    let tx_clone = tx.clone();

    thread::spawn(move || {
        let mut last_packet_time = None::<Instant>;
        let mut last_interval_us: Option<i128> = None;
        let start_time = Instant::now();
        let mut consecutive_failures = 0;   // ✅ NEW: track failures

        while let Ok(serialized) = serialized_rx.recv() {
            let decode_start = Instant::now();

            // Step 1: decrypt
            let decrypted = match xor_decrypt_from_hex(&serialized, ENC_KEY) {
                Ok(json) => json,
                Err(e) => {
                    log_event(&format!(
                        "[{}] ❌ Telemetry decryption failed: {} | raw={}",
                        current_timestamp(),
                        e,
                        serialized
                    ));
                    consecutive_failures += 1;
                    record_missing_telemetry();
                    let _ = tx_clone.send(TelemetryEvent::PacketMissing(consecutive_failures));
                    continue; // skip loop
                }
            };

            // Step 2: parse JSON
            let parsed: Result<WirePacket, _> = serde_json::from_str(&decrypted);

            let decode_duration = decode_start.elapsed();
            let now = Instant::now();

            match parsed {
                Ok(wp) => {
                    // ✅ reset failure counter when success
                    consecutive_failures = 0;

                    // ✅ NEW: log decode time
                    if decode_duration <= Duration::from_millis(3) {
                        log_event(&format!(
                            "[{}] ⏱️ Telemetry decode OK in {}µs for Packet {}",
                            current_timestamp(),
                            decode_duration.as_micros(),
                            wp.id
                        ));
                    } else {
                        log_event(&format!(
                            "[{}] ⚠️ Telemetry decode SLOW: {}µs for Packet {}",
                            current_timestamp(),
                            decode_duration.as_micros(),
                            wp.id
                        ));
                    }

                    // 🔹 Your existing latency/jitter logic (untouched)
                    let ts_parse: Result<DateTime<Utc>, _> =
                        DateTime::parse_from_rfc3339(&wp.timestamp_rfc3339)
                            .map(|dt| dt.with_timezone(&Utc));
                    let latency_ms = ts_parse.map(|t| {
                        let dur = Utc::now().signed_duration_since(t);
                        dur.num_milliseconds().max(0) as u64
                    }).unwrap_or(0u64);

                    if let Some(last) = last_packet_time {
                        let interval_us = now.duration_since(last).as_micros() as i128;
                        let drift_us = if let Some(prev) = last_interval_us {
                            (interval_us - prev).abs()
                        } else { 0i128 };
                        last_interval_us = Some(interval_us);
                        log_jitter(drift_us as u128);
                        record_telemetry_latency_and_jitter(latency_ms, Some(drift_us as u128));
                        let _ = tx_clone.send(TelemetryEvent::PacketReceived {
                            packet_id: wp.id,
                            latency_ms,
                            drift_us,
                        });
                    } else {
                        record_telemetry_latency_and_jitter(latency_ms, None);
                        let _ = tx_clone.send(TelemetryEvent::PacketReceived {
                            packet_id: wp.id,
                            latency_ms,
                            drift_us: 0,
                        });
                    }

                    last_packet_time = Some(now);

                    log_event(&format!(
                        "[{}] ✅ Telemetry Packet {} parsed | Latency={}ms",
                        current_timestamp(),
                        wp.id,
                        latency_ms
                    ));

                    // 🔹 Your existing command safety + scheduling logic (untouched)
                    if is_command_safe(wp.id as u32) {
                        let command_start = Instant::now();
                        trigger_command_scheduler(wp.id as u32);
                        let command_duration = command_start.elapsed();
                        if command_duration <= Duration::from_millis(2) {
                            log_event(&format!(
                                "[{}] ⏱️  Command dispatched within deadline for Packet {} ({}µs)",
                                current_timestamp(),
                                wp.id,
                                command_duration.as_micros()
                            ));
                        } else {
                            log_event(&format!(
                                "[{}] ⚠️  Deadline MISS: Command dispatch took {}µs for Packet {}",
                                current_timestamp(),
                                command_duration.as_micros(),
                                wp.id
                            ));
                        }
                    } else {
                        log_event(&format!(
                            "[{}] ❌ Command for Packet {} BLOCKED by safety system",
                            current_timestamp(),
                            wp.id
                        ));
                    }

                    // 🔹 Your existing fault + monitoring (untouched)
                    handle_fault(wp.id as u32);
                    start_monitoring(wp.id as u32);
                    report_cpu_utilization(now.duration_since(start_time));
                }
                Err(e) => {
                    // ✅ JSON parse failed
                    consecutive_failures += 1;
                    log_event(&format!(
                        "[{}] ⚠️ Telemetry: failed to parse JSON ({}). Failure #{}, raw={}",
                        current_timestamp(),
                        e,
                        consecutive_failures,
                        serialized
                    ));
                    record_missing_telemetry();
                    let _ = tx_clone.send(TelemetryEvent::PacketMissing(consecutive_failures));

                    if consecutive_failures >= 3 {
                        log_event(&format!(
                            "[{}] 🚨 LOSS OF CONTACT: 3 consecutive packets failed",
                            current_timestamp()
                        ));
                        let _ = tx_clone.send(TelemetryEvent::ContactLost);
                    } else {
                        log_event(&format!(
                            "[{}] 🔄 Re-requesting telemetry packet (attempt #{})",
                            current_timestamp(),
                            consecutive_failures
                        ));
                    }
                }
            }
        }
    });

    tx
}
