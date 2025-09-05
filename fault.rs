use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use crossbeam_channel::Sender;
use crate::types::DownlinkPacket;

use once_cell::sync::Lazy;

use crate::utils::logger::log_event;
use crate::utils::timing::current_timestamp;
use crate::monitor::MonitoringData;
use crate::monitor::{log_fault_recovery, MONITOR_DATA};

// use crate::monitor::{MonitoringData, log_fault_recovery}; // 🔁 REMOVE this import
// use crate::shared::MONITOR_DATA; // 🔁 REMOVE this to avoid circular dependency

pub static ACTIVE_FAULTS: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));
pub static FAULT_TIMESTAMPS: Lazy<Mutex<HashMap<String, Instant>>> = Lazy::new(|| Mutex::new(HashMap::new()));
pub static FAULT_SEVERITY: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));

const LATENCY_THRESHOLD: Duration = Duration::from_millis(100);
const AUTO_CLEAR_THRESHOLD: Duration = Duration::from_secs(15);


pub fn handle_faults() {
    let simulated_faults = vec!["TEMP_HIGH", "BATTERY_LOW", "SIGNAL_LOSS"];

    let mut faults = ACTIVE_FAULTS.lock().unwrap();
    let mut timestamps = FAULT_TIMESTAMPS.lock().unwrap();
    let mut severities = FAULT_SEVERITY.lock().unwrap();

    for fault in simulated_faults {
        if rand::random::<f32>() < 0.2 {
            if faults.insert(fault.to_string()) {
                let now = Instant::now();
                timestamps.insert(fault.to_string(), now);

                let severity = match fault {
                    "SIGNAL_LOSS" => "CRITICAL",
                    "BATTERY_LOW" => "WARNING",
                    "TEMP_HIGH" => "WARNING",
                    _ => "UNKNOWN",
                };

                severities.insert(fault.to_string(), severity.to_string());

                log_event(&format!(
                    "[{}] ⚠️  FAULT DETECTED [{}]: {}",
                    current_timestamp(),
                    severity,
                    fault
                ));
                // Trigger corrective action for the packet
                crate::downlink::trigger_fault_command(fault);

            }
        }
    }

    let now = Instant::now();
    let expired: Vec<_> = timestamps
        .iter()
        .filter(|(_, ts)| now.duration_since(**ts) > AUTO_CLEAR_THRESHOLD)

        .map(|(f, _)| f.clone())
        .collect();

    for fault in expired {
        faults.remove(&fault);
        timestamps.remove(&fault);
        severities.remove(&fault);
        log_event(&format!(
            "[{}] ✅ FAULT CLEARED: {}",
            current_timestamp(),
            fault
        ));
    }
}

fn fault_blocks_packet(fault: &str, packet_id: u32) -> bool {
    match fault {
        "TEMP_HIGH" => packet_id % 5 == 0,
        "BATTERY_LOW" => packet_id % 4 == 0,
        "SIGNAL_LOSS" => packet_id % 2 == 0,
        _ => false,
    }
}

pub fn is_command_safe(packet_id: u32) -> bool {
    let faults = ACTIVE_FAULTS.lock().unwrap();
    let timestamps = FAULT_TIMESTAMPS.lock().unwrap();

    for fault in faults.iter() {
        if fault_blocks_packet(fault, packet_id) {
            if let Some(&ts) = timestamps.get(fault) {
                let latency = ts.elapsed();
                log_blocking_context(fault, packet_id, latency);
                return false;
            }
        }
    }

    true
}

fn log_blocking_context(fault: &str, packet_id: u32, latency: Duration) {
    log_event(&format!(
        "[{}] 🚫 BLOCKED: Packet {} blocked by [{}]",
        current_timestamp(),
        packet_id,
        fault
    ));

    log_event(&format!(
    "[{}] ⏱️  Interlock Latency: {} ms (from fault detection to command block)",
    current_timestamp(),
    latency.as_millis()
    ));


    if latency > LATENCY_THRESHOLD {
        log_event(&format!(
            "[{}] 🔥 ALERT: Latency exceeds threshold ({} ms)",
            current_timestamp(),
            LATENCY_THRESHOLD.as_millis()
        ));
    }
    log_event(&format!(
    "[{}] 📄 Logged: Interlock delay for command {} due to fault [{}] was {} ms",
    current_timestamp(),
    packet_id,
    fault,
    latency.as_millis()
));

}

pub fn handle_fault(packet_id: u32) {
    log_event(&format!(
        "[{}] 🔎 Checking Packet {} for faults...",
        current_timestamp(),
        packet_id
    ));

    let faults = ACTIVE_FAULTS.lock().unwrap();
    let timestamps = FAULT_TIMESTAMPS.lock().unwrap();
    let severities = FAULT_SEVERITY.lock().unwrap();

    if faults.is_empty() {
        log_event(&format!(
            "[{}] ✅ No faults. Packet {} is SAFE.",
            current_timestamp(),
            packet_id
        ));
        return;
    }

    log_event(&format!(
        "[{}] 🚨 Active Faults Detected: {:?}",
        current_timestamp(),
        *faults
    ));

    for fault in faults.iter() {
        let age = timestamps.get(fault).map(|ts| ts.elapsed()).unwrap_or_default();
        let severity = severities.get(fault).cloned().unwrap_or_else(|| "UNKNOWN".to_string());

        if fault_blocks_packet(fault, packet_id) {
            log_event(&format!(
                "[{}] ❌ BLOCKED: Fault [{}] blocks Packet {}",
                current_timestamp(),
                fault,
                packet_id
            ));
        } else {
            log_event(&format!(
                "[{}] ✅ ALLOWED: Fault [{}] does not block Packet {}",
                current_timestamp(),
                fault,
                packet_id
            ));
        }

        log_event(&format!(
            "[{}] 🕓 Fault Age: {} ms | Severity: {}",
            current_timestamp(),
            age.as_millis(),
            severity
        ));

        // ❌ Temporarily removed to avoid cyclic dependency
        if age.as_millis() > 2000 {
            // start timing now
            let recovery_start = Instant::now();

            // simulate quick recovery (<100ms)
            std::thread::sleep(Duration::from_millis(50));

            // log using the Instant
            log_fault_recovery(recovery_start, Arc::clone(&MONITOR_DATA));

            // if you also want to print the duration, measure separately
            let recovery_time = recovery_start.elapsed();
            log_event(&format!(
                "[{}] 🛠️ Fault [{}] recovered in {} ms",
                current_timestamp(),
                fault,
                recovery_time.as_millis()
            ));
        }
    }


    pub fn print_health_summary() {
        let faults = ACTIVE_FAULTS.lock().unwrap();
        let severities = FAULT_SEVERITY.lock().unwrap();

        if faults.is_empty() {
            log_event(&format!(
                "[{}] ✅ HEALTHY: No active faults.",
                current_timestamp()
            ));
        } else {
            log_event(&format!(
                "[{}] 🛑 ALERT: {} active fault(s) detected.",
                current_timestamp(),
                faults.len()
            ));

            for fault in faults.iter() {
                let severity = severities.get(fault).cloned().unwrap_or_else(|| "UNKNOWN".to_string());
                log_event(&format!(
                    "[{}] ➤ Fault: {} | Severity: {}",
                    current_timestamp(),
                    fault,
                    severity
                ));
            }
        }
    }
}

pub fn start_fault_injector(
    shutdown_flag: Arc<AtomicBool>,
    downlink_tx: Option<Sender<DownlinkPacket>>,
) {
    thread::spawn(move || {
        while !shutdown_flag.load(Ordering::SeqCst) {
            // Sleep for 60s between fault injections
            thread::sleep(Duration::from_secs(60));

            if shutdown_flag.load(Ordering::SeqCst) {
                break;
            }

            // Inject faults into the system
            handle_faults();

            // Optionally, notify downlink tasks of faults
            if let Some(tx) = &downlink_tx {
                // Push a dummy packet representing a fault-triggered event
                let dummy_packet = DownlinkPacket {
                    id: 0, // 0 means system/fault event
                    sensor_name: "FAULT".to_string(),
                    compressed_value: "FAULT_EVENT".to_string(),
                    timestamp: chrono::Utc::now(),
                    priority: 0,
                    response_tx: None,
                    queued_at: std::time::Instant::now(),
                    window_start_at: chrono::Utc::now(),
                };
                let _ = tx.send(dummy_packet);
            }
        }

        log_event(&format!(
            "[{}] 🛑 Fault injector thread terminated.",
            current_timestamp()
        ));
    });
}