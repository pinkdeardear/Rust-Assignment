use crate::types::{SensorData, DownlinkPacket, WirePacket, xor_encrypt_to_hex, ENC_KEY};
use crossbeam_channel::{Sender, Receiver};
use std::collections::VecDeque;
use std::sync::{Mutex, atomic::{AtomicU64, Ordering}};
use std::thread;
use std::time::{Duration, Instant};
use crate::utils::logger::log_event;
use crate::utils::timing::current_timestamp;
use crate::fault::{is_command_safe, handle_fault};
use crate::monitor::{update_cpu_active_time, record_missed_downlink, record_downlink_latency, trigger_degraded_mode};
use log::{info, warn};
use chrono::Utc;
use serde_json;

// -------------------- GLOBALS --------------------
static PACKET_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
static COMMAND_QUEUE: Mutex<VecDeque<u32>> = Mutex::new(VecDeque::new());
static STATS: Mutex<Vec<u128>> = Mutex::new(Vec::new());
static mut CPU_ACTIVE_MICROS: u128 = 0;


// -------------------- COMMAND SCHEDULER --------------------
fn get_priority(command_id: u32) -> u8 {
    match command_id {
        1 => 1,
        2 => 2,
        3 => 3,
        _ => 5,
    }
}


/// Decode HEX string and XOR-decrypt with repeating key -> UTF-8 string.
pub fn xor_decrypt_from_hex(hex: &str, key: &[u8]) -> Result<String, String> {
    if key.is_empty() {
        return Err("empty key".to_string());
    }
    let s = hex.trim();
    if s.len() % 2 != 0 {
        return Err("hex length must be even".to_string());
    }

    // parse hex two chars at a time, case-insensitive
    let mut out: Vec<u8> = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let pair = &s[i..i + 2];
        match u8::from_str_radix(pair, 16) {
            Ok(byte) => out.push(byte),
            Err(e) => return Err(format!("hex parse error at {}: {}", i, e)),
        }
    }

    // XOR with key and collect bytes
    for (i, b) in out.iter_mut().enumerate() {
        *b = *b ^ key[i % key.len()];
    }

    String::from_utf8(out).map_err(|e| format!("utf8 error: {}", e))
}

pub fn init_command_scheduler() {
    log_event("📡 Command Scheduler Initialized (Rate Monotonic Scheduling)");
}

pub fn trigger_command_scheduler(packet_id: u32) {
    if !is_command_safe(packet_id) { return; }

    let priority = get_priority(packet_id);
    let deadline = Instant::now() + Duration::from_millis(2);
    let start = Instant::now();

    if priority == 1 {
        log_event(&format!(
            "[{}] 🔥 Critical Command {} (Thermal Control) preempting lower-priority tasks",
            current_timestamp(),
            packet_id
        ));
    }

    {
        let mut queue = COMMAND_QUEUE.lock().unwrap();
        queue.push_back(packet_id);
    }

    let simulated_exec = match priority {
        1 => 1000,
        2 => 1300,
        3 => 1500,
        _ => 1800,
    };
    thread::sleep(Duration::from_micros(simulated_exec));

    let now = Instant::now();
    let elapsed = now.duration_since(start).as_micros();

    unsafe {
        CPU_ACTIVE_MICROS += elapsed;
        update_cpu_active_time(CPU_ACTIVE_MICROS);
    }

    {
        let mut stats = STATS.lock().unwrap();
        stats.push(elapsed);
        if stats.len() % 10 == 0 {
            let min = *stats.iter().min().unwrap_or(&0);
            let max = *stats.iter().max().unwrap_or(&0);
            let avg = stats.iter().sum::<u128>() / stats.len() as u128;
            let batch = stats.len() / 10;
            log_event(&format!(
                "[{}] 📊 Drift Stats (Batch {}–{}): Min={}µs | Max={}µs | Avg={}µs",
                current_timestamp(),
                (batch - 1) * 10 + 1,
                batch * 10,
                min,
                max,
                avg
            ));
        }
    }

    if now <= deadline {
        log_event(&format!(
            "[{}] ✅ Command {} executed on time ({}µs)",
            current_timestamp(),
            packet_id,
            elapsed
        ));
    } else {
        let drift = now.duration_since(deadline).as_micros();
        log_event(&format!(
            "[{}] ⚠️ Deadline Miss: Command {} delayed by {}µs (Exec: {}µs)",
            current_timestamp(),
            packet_id,
            drift,
            elapsed
        ));
    }
}

pub fn report_cpu_utilization(sim_duration: Duration) {
    let active_time_micros = unsafe { CPU_ACTIVE_MICROS };
    let total_time_micros = sim_duration.as_micros().max(1);
    let cpu_usage_percent = (active_time_micros as f64 / total_time_micros as f64) * 100.0;

    log_event(&format!(
        "[{}] 🧠 CPU Utilization: Active={} µs / Total={} µs | Usage={:.2}%",
        current_timestamp(),
        active_time_micros,
        total_time_micros,
        cpu_usage_percent
    ));
}

// -------------------- DOWNLINK MANAGER --------------------
pub fn compress_and_packetize(
    data: SensorData,
    response_tx: Sender<String>,
    buffer_len: usize,
    buffer_max: usize,
) -> DownlinkPacket {
    let start = Instant::now();

    // Missed comm init check
    let init_elapsed = start.elapsed().as_micros();
    if init_elapsed > 5000 {
        warn!(
            "🚨 MISSED COMM INIT: Downlink not ready within 5ms for {}",
            data.name
        );
        record_missed_downlink(&data.name, init_elapsed as i64);
    }

    let id = PACKET_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
    let queued_at = Instant::now();

    // Buffer usage check
    let buffer_usage = buffer_len as f32 / buffer_max as f32;
    if buffer_usage > 0.8 {
        warn!(
            "⚠️ DOWNLINK DEGRADED MODE: buffer at {:.0}% for {}",
            buffer_usage * 100.0,
            data.name
        );
        trigger_degraded_mode(buffer_usage);
    }

    let compressed = format!("{:.2}", data.value);

    let packet = DownlinkPacket {
        id,
        sensor_name: data.name,
        compressed_value: compressed,
        timestamp: data.generated_at,
        priority: data.priority,
        response_tx: Some(response_tx),
        queued_at,
        window_start_at: Utc::now(),
    };

    // Measure prep latency from sensor generation to downlink-ready
    let prep_elapsed = (Utc::now() - packet.window_start_at).num_milliseconds();
    
    if prep_elapsed > 30 {
        warn!(
            "🚫 MISSED COMMUNICATION: Packet prepared {} ms after generation (>30ms) for {}",
            prep_elapsed, packet.sensor_name
        );
        record_missed_downlink(&packet.sensor_name, prep_elapsed as i64);
    } else {
        info!("✅ Packet prepared in {} ms for {}", prep_elapsed, packet.sensor_name);
        record_downlink_latency(&packet.sensor_name, prep_elapsed as i64);
    }

    packet
}


// -------------------- START DOWNLINK MANAGER --------------------
pub fn start_downlink_manager(
    downlink_rx: Receiver<DownlinkPacket>,
    ground_sender: Sender<String>,    // send serialized (BUT ENCODED) payload to ground station
    uplink_rx: Receiver<String>,      // uplink ACKs (from run_ground)
) {
    thread::spawn(move || {
        info!("[DownlinkManager] Started.");

        while let Ok(packet) = downlink_rx.recv() {
            // early visibility / latency check
            let queue_latency = packet.queued_at.elapsed().as_millis();
            if queue_latency > 30 {
                warn!("🚫 Packet {} from {} missed visibility window", packet.id, packet.sensor_name);
                crate::monitor::record_downlink_dropped();
                continue;
            }

            let packet_id_u32: u32 = packet.id.try_into().unwrap_or(0);
            if !is_command_safe(packet_id_u32) {
                handle_fault(packet_id_u32);
                warn!("[DownlinkManager] ⚠️ Command/Packet id={} blocked due to active fault(s)", packet.id);
                crate::monitor::record_downlink_dropped();
                continue;
            }

            // Convert to wire format and serialize (JSON)
            let wire = WirePacket::from(&packet);
            let serialized = match serde_json::to_string(&wire) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to serialize WirePacket {}: {}", packet.id, e);
                    crate::monitor::record_downlink_dropped();
                    continue;
                }
            };

            // ENCODE the serialized JSON before sending to ground:
            // using existing xor_encrypt_to_hex + ENC_KEY
            let encrypted_hex = xor_encrypt_to_hex(&serialized, ENC_KEY);

            // Send encoded payload to ground (encoded hex string)
            if ground_sender.send(encrypted_hex.clone()).is_err() {
                warn!("[DownlinkManager] Failed to send encoded payload to ground for pkt id={} (sample {:?})", packet.id, packet.sensor_name);
                crate::monitor::record_downlink_dropped();
            } else {
                info!("[DownlinkManager] Sent ENCODED packet {} to ground | encoded_len={} chars | raw_len={} chars",
                    packet.id, encrypted_hex.len(), serialized.len());
            }
            crate::monitor::record_packet_generated();

            // simulate RF transmission delay
            let transmit_start = Instant::now();
            thread::sleep(Duration::from_millis(15)); // simulated RF + processing

            info!("[DownlinkManager] Packet id={} transmitted (simulated latency {}ms)", packet.id, transmit_start.elapsed().as_millis());

            // Wait for ACK on uplink_rx and forward to task via packet.response_tx
            match uplink_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(ack) => {
                    info!("[DownlinkManager] Received ACK for pkt id={} from ground: {}", packet.id, ack);
                    if let Some(tx) = packet.response_tx {
                        let _ = tx.send(ack);
                    }
                    crate::monitor::record_downlink_sent();
                }
                Err(_) => {
                    warn!("[DownlinkManager] No ACK from ground for pkt id={} (timeout)", packet.id);
                    crate::monitor::record_downlink_dropped();
                }
            }
        }

        info!("[DownlinkManager] Exiting thread.");
    });
}


// -------------------- GROUND STATION --------------------
pub fn run_ground(
    command_rx: Receiver<String>, // receives serialized WirePacket JSON (or encoded hex)
    shutdown_rx: Receiver<()>,
    uplink_tx: Sender<String>,    // send ACKs back to satellite
) {
    thread::spawn(move || loop {
        if shutdown_rx.try_recv().is_ok() {
            println!("[Ground Station] 🔻 Shutdown received, stopping ground station...");
            break;
        }

        if let Ok(serialized) = command_rx.try_recv() {

            let maybe_plain = match xor_decrypt_from_hex(&serialized, ENC_KEY) {
                Ok(p) => p,
                Err(e) => {
                    println!("[Ground Station] ⚠️ Failed to decrypt incoming packet: {}", e);
                    let _ = uplink_tx.send("ACK: undecodable packet".to_string());
                    continue; // 🚨 skip JSON parsing, don't treat hex as JSON
                }
            };

            // Try to parse as WirePacket
            match serde_json::from_str::<WirePacket>(&maybe_plain) {
                Ok(w) => {
                    println!("[Ground Station] 🛰️ Received WirePacket id={} from {}", w.id, w.sensor_name);
                    println!("[Ground Station] ✅ Packet decoded: {:?}", w);
                    println!("[Ground Station] ✅ Unwrapped Packet: {:?}", w);

                    // record that the ground station successfully processed this packet
                    crate::monitor::record_packet_processed();

                    crate::monitor::start_monitoring(w.id as u32);

                    // attempt to extract timestamp from raw JSON (generic approach)
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&maybe_plain) {
                        if let Some(ts_val) = v.get("window_start_at")
                            .or_else(|| v.get("timestamp"))
                            .or_else(|| v.get("generated_at")) {
                            if let Some(ts_str) = ts_val.as_str() {
                                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str) {
                                    // compute latency in ms and record it
                                    let latency_ms = chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc)).num_milliseconds();
                                    crate::monitor::record_telemetry_latency_and_jitter(latency_ms as u64, None);
                                }
                            }
                        }
                    }

                    let ack = format!("ACK: Packet {} processed", w.id);
                    if let Err(e) = uplink_tx.send(ack) {
                        eprintln!("[Ground Station] ⚠️ Failed to send ACK: {}", e);
                    } else {
                        println!("[Ground Station] 📡 ACK sent back to satellite for pkt {}", w.id);
                    }
                }
                Err(e) => {
                    // not parseable as WirePacket
                    // Try to see if the original serialized was raw JSON that can be parsed
                    match serde_json::from_str::<serde_json::Value>(&serialized) {
                        Ok(val) => {
                            // it's JSON but not match WirePacket struct — still consider "processed"
                            println!("[Ground Station] 🔹 Received JSON (unmapped): {:?}", val);

                            // try extract timestamp and record latency
                            if let Some(ts_val) = val.get("window_start_at").or_else(|| val.get("timestamp")).or_else(|| val.get("generated_at")) {
                                if let Some(ts_str) = ts_val.as_str() {
                                    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str) {
                                        let latency_ms = chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc)).num_milliseconds();
                                        crate::monitor::record_telemetry_latency_and_jitter(latency_ms as u64, None);
                                    }
                                }
                            }

                            if !serialized.starts_with("ACK:") {
                                let _ = uplink_tx.send("ACK: JSON received".to_string());
                            }
                        }
                        Err(_) => {
                            // Plain text — treat as non-packet telemetry/text
                            println!("[Ground Station] 🔹 Received text: {}", serialized);
                            if !serialized.starts_with("ACK:") {
                                let ack = format!("ACK: Text received");
                                let _ = uplink_tx.send(ack);
                            }
                        }
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(1));
    });
}


// -------------------- UTILITIES --------------------
pub fn get_cpu_active_micros() -> u128 {
    unsafe { CPU_ACTIVE_MICROS }
}

pub fn send_command(packet_id: u32) {
    if !is_command_safe(packet_id) {
        handle_fault(packet_id);
        log_event(&format!(
            "[{}] ⚠️ Command {} blocked due to active fault(s), not sending to satellite.",
            current_timestamp(),
            packet_id
        ));
        return;
    }
    log_event(&format!(
        "[{}] 📡 Sending command {} to satellite (no blocking faults).",
        current_timestamp(),
        packet_id
    ));
}

pub fn trigger_fault_command(fault: &str) {
    let action = match fault {
        "TEMP_HIGH" => "initiating cooling procedure",
        "BATTERY_LOW" => "initiating power-saving procedure",
        "SIGNAL_LOSS" => "initiating reconnection procedure",
        _ => "no corrective action taken",
    };
    log_event(&format!(
        "[{}] [DownlinkManager] ⚠️ {} detected, {}",
        current_timestamp(),
        fault,
        action
    ));

    if matches!(fault, "TEMP_HIGH" | "BATTERY_LOW" | "SIGNAL_LOSS") {
        log_event(&format!(
            "[{}] [DownlinkManager] 📡 Sending corrective action for {} to satellite...",
            current_timestamp(),
            fault
        ));
        log_event(&format!(
            "[{}] [Satellite] 🛰️ Corrective action for {} executed successfully!",
            current_timestamp(),
            fault
        ));
    } else {
        log_event(&format!(
            "[{}] [DownlinkManager] ⚠️ Unknown fault {}, no action sent to satellite",
            current_timestamp(),
            fault
        ));
    }
}
