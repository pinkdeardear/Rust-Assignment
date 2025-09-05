use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::VecDeque;
use std::thread;
use std::time::{Duration, Instant};

use crate::utils::logger::log_event;
use crate::utils::timing::current_timestamp;
use once_cell::sync::Lazy;

use crate::downlink::get_cpu_active_micros;
use log::{info, warn};
use std::collections::HashMap;

extern crate once_cell;

pub static SHUTDOWN_FLAG: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));

pub static MONITOR_DATA: Lazy<Arc<Mutex<MonitoringData>>> =
    Lazy::new(|| Arc::new(Mutex::new(MonitoringData::new())));

const DEBUG_SIMULATE_BACKLOG: bool = true; // Toggle to false to disable backlog simulation

#[derive(Debug)]
pub struct MonitoringData {
    pub jitter_values: Vec<u128>,
    pub telemetry_latencies_ms: Vec<u64>,
    pub execution_drift_records: Vec<i128>,
    pub missed_deadlines: usize,
    pub backlog_history: Vec<usize>,
    pub fault_recovery_times: Vec<u128>,
    pub max_drift: Option<i128>,
    pub min_drift: Option<i128>,
    pub monitoring_start: Option<Instant>,
    pub monitoring_end: Option<Instant>,
    pub simulated_cpu_load: Vec<f32>,
    pub packets_processed: usize,
    pub cpu_active_micros: u128, // ✅ New field to store CPU active time

    // Satellite metrics summary
    pub sat_metrics: SatelliteSummary,

    // Contact / telemetry health
    pub consecutive_missing_packets: usize,
    pub contact_lost: bool,
}

impl MonitoringData {
    pub fn new() -> Self {
        MonitoringData {
            jitter_values: Vec::new(),
            telemetry_latencies_ms: Vec::new(),
            execution_drift_records: Vec::new(),
            missed_deadlines: 0,
            backlog_history: Vec::new(),
            fault_recovery_times: Vec::new(),
            max_drift: None,
            min_drift: None,
            monitoring_start: None,
            monitoring_end: None,
            simulated_cpu_load: Vec::new(),
            packets_processed: 0,
            cpu_active_micros: 0,
            sat_metrics: SatelliteSummary::new(),
            consecutive_missing_packets: 0,
            contact_lost: false,
        }
    }

    pub fn calculate_average_jitter(&self) -> Option<u128> {
        if self.jitter_values.is_empty() {
            None
        } else {
            Some(self.jitter_values.iter().sum::<u128>() / self.jitter_values.len() as u128)
        }
    }

    pub fn calculate_average_drift(&self) -> Option<i128> {
        if self.execution_drift_records.is_empty() {
            None
        } else {
            Some(self.execution_drift_records.iter().sum::<i128>() / self.execution_drift_records.len() as i128)
        }
    }

    pub fn calculate_average_telemetry_latency(&self) -> Option<f64> {
        if self.telemetry_latencies_ms.is_empty() {
            None
        } else {
            Some(self.telemetry_latencies_ms.iter().sum::<u64>() as f64 / self.telemetry_latencies_ms.len() as f64)
        }
    }
}

pub fn start_monitoring(packet_id: u32) {
    log_event(&format!(
        "[{}] 📡 Monitoring packet ID: {}",
        current_timestamp(),
        packet_id
    ));
}

/// Record a telemetry reception latency (ms) and optionally jitter (µs)
pub fn record_telemetry_latency_and_jitter(latency_ms: u64, jitter_us: Option<u128>) {
    let mut monitor = MONITOR_DATA.lock().unwrap();
    monitor.telemetry_latencies_ms.push(latency_ms);
    if let Some(j) = jitter_us {
        monitor.jitter_values.push(j);
    }
    // reset consecutive missing since we just received something
    monitor.consecutive_missing_packets = 0;
    monitor.contact_lost = false;
}

/// Record a missing telemetry packet (called by telemetry logic when packet missed)
pub fn record_missing_telemetry() {
    let mut monitor = MONITOR_DATA.lock().unwrap();
    monitor.consecutive_missing_packets += 1;
    if monitor.consecutive_missing_packets >= 3 && !monitor.contact_lost {
        monitor.contact_lost = true;
        log_event(&format!(
            "[{}] 🚨 CONTACT LOST: {} consecutive telemetry missing",
            current_timestamp(),
            monitor.consecutive_missing_packets
        ));
    } else {
        log_event(&format!(
            "[{}] ⚠️ Telemetry missing (consecutive {})",
            current_timestamp(),
            monitor.consecutive_missing_packets
        ));
    }
}

pub fn periodic_monitoring(
    monitor_data: Arc<Mutex<MonitoringData>>,
    backlog_queue: Arc<Mutex<VecDeque<u32>>>,
    expected_interval: Duration,
) {
    thread::spawn(move || {
        {
            let mut monitor = monitor_data.lock().unwrap();
            monitor.monitoring_start = Some(Instant::now());
        }

        let mut last_run_time = Instant::now();
        let mut tick = 0usize;
        let mut last_packets_processed: usize = 0;

        loop {
            thread::sleep(expected_interval);
            tick += 1;

            let now = Instant::now();
            let scheduled_time = last_run_time + expected_interval;
            let drift = now.duration_since(scheduled_time).as_micros() as i128;
            last_run_time = now;

            let mut monitor = monitor_data.lock().unwrap();

            // record drift and update mins/max
            monitor.execution_drift_records.push(drift);
            monitor.max_drift = Some(monitor.max_drift.map_or(drift, |max| max.max(drift)));
            monitor.min_drift = Some(monitor.min_drift.map_or(drift, |min| min.min(drift)));

            if drift > 2_000 {
                monitor.missed_deadlines += 1;
                log_event(&format!(
                    "[{}] ❗ Missed periodic deadline: drift {} µs",
                    current_timestamp(),
                    drift
                ));
            } else {
                log_event(&format!(
                    "[{}] ✅ Execution drift: {} µs",
                    current_timestamp(),
                    drift
                ));
            }

            // backlog: real value or simulated for demo
            let backlog_len = {
                let queue = backlog_queue.lock().unwrap();
                if DEBUG_SIMULATE_BACKLOG && queue.is_empty() {
                    tick % 5 + (drift.abs() as usize % 4)
                } else {
                    queue.len()
                }
            };
            monitor.backlog_history.push(backlog_len);

            log_event(&format!(
                "[{}] 📊 Telemetry backlog: {} packets",
                current_timestamp(),
                backlog_len
            ));

            // simple simulated load metric (for reporting)
            let simulated_load = ((drift.abs() as f32) * 0.02 + backlog_len as f32 * 2.5).min(100.0);
            monitor.simulated_cpu_load.push(simulated_load);

            log_event(&format!(
                "[{}] 🧠 Simulated System Load: {:.1}%",
                current_timestamp(),
                simulated_load
            ));

            // Detect contact loss by comparing processed packets count
            if monitor.packets_processed == last_packets_processed {
                // no new packets processed since last tick
                monitor.consecutive_missing_packets += 1;
                if monitor.consecutive_missing_packets >= 3 && !monitor.contact_lost {
                    monitor.contact_lost = true;
                    log_event(&format!(
                        "[{}] 🚨 CONTACT LOST (periodic): {} intervals without packets",
                        current_timestamp(),
                        monitor.consecutive_missing_packets
                    ));
                }
            } else {
                // reset counters on progress
                monitor.consecutive_missing_packets = 0;
                monitor.contact_lost = false;
                last_packets_processed = monitor.packets_processed;
            }

            monitor.monitoring_end = Some(Instant::now());
        }
    });
}

/// Append an uplink jitter sample (µs) — used by telemetry
pub fn log_jitter(jitter: u128) {
    let mut monitor = MONITOR_DATA.lock().unwrap();
    monitor.jitter_values.push(jitter);
    log_event(&format!(
        "[{}] ⏱️  Uplink jitter recorded: {} µs",
        current_timestamp(),
        jitter
    ));
}

/// Log fault recovery to both the general monitor and satellite summary
pub fn log_fault_recovery(start: Instant, monitor_data: Arc<Mutex<MonitoringData>>) {
    let duration_us = Instant::now().duration_since(start).as_micros();
    {
        let mut monitor = monitor_data.lock().unwrap();
        monitor.fault_recovery_times.push(duration_us);
        monitor.sat_metrics.record_fault_recovery(duration_us as u128);
    }

    log_event(&format!(
        "[{}] 🛠️  Fault recovered in {} µs",
        current_timestamp(),
        duration_us
    ));

    // 🚨 Mission abort check
    if duration_us > 200_000 {
        log_event(&format!(
            "[{}] 💥 Mission ABORT: Fault recovery exceeded 200ms ({} µs)",
            current_timestamp(),
            duration_us
        ));

        // Trigger system shutdown
        SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
    }
}

/// Report CPU utilization using stored micros in the monitor struct (not unsafe)
pub fn report_cpu_utilization_block(monitor_data: &MonitoringData, sim_duration: Duration) {
    let active = monitor_data.cpu_active_micros;
    let total = sim_duration.as_micros().max(1);
    let usage = (active as f64 / total as f64) * 100.0;

    println!("🧠 CPU Utilization:");
    println!("   • Active Time: {} µs", active);
    println!("   • Total Time:  {:.2?}", sim_duration);
    println!("   • Usage:       {:.2}%", usage);
}

/// Called by other modules to update CPU active micros (thread-safe)
pub fn update_cpu_active_time(micros: u128) {
    let mut monitor = MONITOR_DATA.lock().unwrap();
    monitor.cpu_active_micros = micros;
}

pub fn print_monitoring_report(monitor_data: Arc<Mutex<MonitoringData>>) {
    let monitor = monitor_data.lock().unwrap();
    let monitoring_duration = monitor.monitoring_start.and_then(|start| monitor.monitoring_end.map(|end| end - start)).unwrap_or_default();

    let active = get_cpu_active_micros();

    let usage = if monitoring_duration.as_micros() > 0 {
        (active as f64 / monitoring_duration.as_micros() as f64) * 100.0
    } else {
        0.0
    };

    println!("\n=========== 📊 GROUND SYSTEM PERFORMANCE REPORT ===========");
    let monitoring_duration = monitor.monitoring_start.and_then(|start| monitor.monitoring_end.map(|end| end - start)).unwrap_or_default();
    println!("🕒 Monitoring Duration: {:.2?}", monitoring_duration);

    println!("⏱️  Avg Uplink Jitter: {} µs",
        monitor.calculate_average_jitter().map_or("N/A".to_string(), |v| v.to_string()));

    println!("📉 Execution Drift:");
    println!("   • Avg: {} µs", monitor.calculate_average_drift().unwrap_or(0));
    println!("   • Max: {} µs", monitor.max_drift.unwrap_or(0));
    println!("   • Min: {} µs", monitor.min_drift.unwrap_or(0));

    println!("❗ Missed Deadlines: {}", monitor.missed_deadlines);
    println!("📦 Total Packets Processed: {} packets", monitor.packets_processed);

    let max_backlog = monitor.backlog_history.iter().max().cloned().unwrap_or(0);
    let min_backlog = monitor.backlog_history.iter().min().cloned().unwrap_or(0);
    let avg_backlog = if monitor.backlog_history.is_empty() {
        0.0
    } else {
        monitor.backlog_history.iter().sum::<usize>() as f32 / monitor.backlog_history.len() as f32
    };
    println!("📦 Avg Backlog: {:.1} packets | Max Backlog: {} packets | Min Backlog: {} packets", avg_backlog, max_backlog, min_backlog);

    let avg_load = if monitor.simulated_cpu_load.is_empty() {
        0.0
    } else {
        monitor.simulated_cpu_load.iter().sum::<f32>() / monitor.simulated_cpu_load.len() as f32
    };
    let max_load = monitor.simulated_cpu_load.iter().cloned().fold(f32::MIN, f32::max);
    let min_load = monitor.simulated_cpu_load.iter().cloned().fold(f32::MAX, f32::min);

    println!(
        "🧠 Avg System Load: {:.1}% (max: {:.1}%, min: {:.1}%)",
        avg_load, if max_load==f32::MIN {0.0} else {max_load}, if min_load==f32::MAX {0.0} else {min_load}
    );

    if !monitor.fault_recovery_times.is_empty() {
        let avg_recovery = monitor.fault_recovery_times.iter().sum::<u128>() as f32 / monitor.fault_recovery_times.len() as f32;
        let max_recovery = monitor.fault_recovery_times.iter().cloned().max().unwrap_or(0);
        let min_recovery = monitor.fault_recovery_times.iter().cloned().min().unwrap_or(0);
        println!("🛠️  Fault Recoveries: {} recoveries | Avg: {:.1} ms | Max: {} | Min: {} ms",
            monitor.fault_recovery_times.len(), avg_recovery, max_recovery, min_recovery);
    } else {
        println!("🛠️ Fault Recoveries: None recorded");
    }

    // ✅ Show actual CPU utilization from struct
    report_cpu_utilization_block(&monitor, monitoring_duration);

    println!("====================================================\n");
}

// --- SATELLITE METRICS (append to monitor.rs) ---
#[derive(Clone, Debug)]
struct Stat {
    count: u64,
    sum_micros: u128,
    max_micros: u128,
    min_micros: u128,
}
impl Stat {
    fn new() -> Self {
        Self { count: 0, sum_micros: 0, max_micros: 0, min_micros: u128::MAX }
    }
    fn record(&mut self, d: Duration) {
        let m = d.as_micros();
        self.count += 1;
        self.sum_micros += m;
        if m > self.max_micros { self.max_micros = m; }
        if m < self.min_micros { self.min_micros = m; }
    }
    fn avg_micros(&self) -> u128 {
        if self.count == 0 { 0 } else { self.sum_micros / (self.count as u128) }
    }
}

#[derive(Clone, Debug)]
pub struct SatelliteSummary {
    sensor_jitter: HashMap<String, Stat>,
    insert_latency: HashMap<String, Stat>,
    scheduling_drift: Stat,
    missed_deadlines: u64,
    dropped_samples: u64,
    downlink_sent: u64,
    downlink_dropped: u64,
    backlog_sum: u128,
    backlog_count: u64,
    backlog_max: usize,
    backlog_min: usize,
    packets_generated: u64,
    packets_processed: u64,
    fault_recoveries: u64,
    fault_recovery_sum_ms: u128,
}
impl SatelliteSummary {
    fn new() -> Self {
        Self {
            sensor_jitter: HashMap::new(),
            insert_latency: HashMap::new(),
            scheduling_drift: Stat::new(),
            missed_deadlines: 0,
            dropped_samples: 0,
            downlink_sent: 0,
            downlink_dropped: 0,
            backlog_sum: 0,
            backlog_count: 0,
            backlog_max: 0,
            backlog_min: usize::MAX,
            packets_generated: 0,
            packets_processed: 0,
            fault_recoveries: 0,
            fault_recovery_sum_ms: 0,
        }
    }

    // Recording helpers
    fn record_sensor_jitter(&mut self, name: &str, d: Duration) {
        let e = self.sensor_jitter.entry(name.to_string()).or_insert_with(Stat::new);
        e.record(d);
    }
    fn record_insert_latency(&mut self, name: &str, d: Duration) {
        let e = self.insert_latency.entry(name.to_string()).or_insert_with(Stat::new);
        e.record(d);
    }
    fn record_sched_drift(&mut self, d: Duration) {
        self.scheduling_drift.record(d);
    }
    fn record_missed_deadline(&mut self) { self.missed_deadlines += 1; }
    fn record_buffer_drop(&mut self) { self.dropped_samples += 1; }
    fn record_downlink_sent(&mut self) { self.downlink_sent += 1; }
    fn record_downlink_dropped(&mut self) { self.downlink_dropped += 1; }
    fn record_backlog(&mut self, len: usize) {
        self.backlog_sum += len as u128;
        self.backlog_count += 1;
        if len > self.backlog_max { self.backlog_max = len; }
        if len < self.backlog_min { self.backlog_min = len; }
    }
    fn record_packet_generated(&mut self) { self.packets_generated += 1; }
    fn record_fault_recovery(&mut self, ms: u128) {
        self.fault_recoveries += 1;
        self.fault_recovery_sum_ms += ms;
    }
}

// Public wrapper functions (thread-safe)
pub fn record_sensor_jitter(name: &str, d: Duration) {
    let mut m = MONITOR_DATA.lock().unwrap();
    m.sat_metrics.record_sensor_jitter(name, d);
}
pub fn record_insert_latency(name: &str, d: Duration) {
    let mut m = MONITOR_DATA.lock().unwrap();
    m.sat_metrics.record_insert_latency(name, d);
}
pub fn record_sched_drift(d: Duration) {
    let mut m = MONITOR_DATA.lock().unwrap();
    m.sat_metrics.record_sched_drift(d);
}
pub fn record_missed_deadline() {
    let mut m = MONITOR_DATA.lock().unwrap();
    m.sat_metrics.record_missed_deadline();
}
pub fn record_buffer_drop() {
    let mut m = MONITOR_DATA.lock().unwrap();
    m.sat_metrics.record_buffer_drop();
}
pub fn record_downlink_sent() {
    let mut m = MONITOR_DATA.lock().unwrap();
    m.sat_metrics.record_downlink_sent();
}
pub fn record_downlink_dropped() {
    let mut m = MONITOR_DATA.lock().unwrap();
    m.sat_metrics.record_downlink_dropped();
}
pub fn record_backlog(len: usize) {
    let mut m = MONITOR_DATA.lock().unwrap();
    m.sat_metrics.record_backlog(len);
}
pub fn record_packet_generated() {
    let mut m = MONITOR_DATA.lock().unwrap();
    m.sat_metrics.record_packet_generated();
}
pub fn record_packet_processed() {
    let mut m = MONITOR_DATA.lock().unwrap();

    // Only increment if total processed < total generated
    if m.sat_metrics.packets_processed < m.sat_metrics.packets_generated {
        m.sat_metrics.packets_processed += 1;

        log_event(&format!(
            "[{}] ✅ Packet PROCESSED | Total: {}",
            current_timestamp(),
            m.sat_metrics.packets_processed
        ));
    } else {
        log_event(&format!(
            "[{}] ⚠️ Skipping processed increment: processed={}, generated={}",
            current_timestamp(),
            m.sat_metrics.packets_processed,
            m.sat_metrics.packets_generated
        ));
    }
}

pub fn record_fault_recovery(ms: u128) {
    let mut m = MONITOR_DATA.lock().unwrap();
    m.sat_metrics.record_fault_recovery(ms);
}

pub fn record_missed_downlink(sensor_name: &str, latency_ms: i64) {
    info!("📡 Missed downlink for {} | latency={}ms", sensor_name, latency_ms);
}

pub fn record_downlink_latency(sensor_name: &str, latency_ms: i64) {
    info!("📊 Downlink latency recorded for {} | {}ms", sensor_name, latency_ms);
}

pub fn trigger_degraded_mode(usage: f32) {
    warn!("⚠️ Degraded mode triggered! Buffer usage: {:.0}%", usage * 100.0);
}

/// Call this at the end of the simulation from main.rs.
/// cpu_active: Duration fetched from the scheduler (task.rs).
pub fn print_satellite_report(start_time: Instant, cpu_active: Duration) {
    let m = MONITOR_DATA.lock().unwrap().sat_metrics.clone();

    let run_dur = start_time.elapsed();
    let run_secs = run_dur.as_secs_f64();

    // Sensor jitter summary (aggregate across sensors)
    let mut total_jitter_count = 0u64;
    let mut total_jitter_sum = 0u128;
    let mut max_jitter_u = 0u128;
    let mut min_jitter_u = u128::MAX;
    for (_k, s) in m.sensor_jitter.iter() {
        total_jitter_count += s.count;
        total_jitter_sum += s.sum_micros;
        if s.max_micros > max_jitter_u { max_jitter_u = s.max_micros; }
        if s.min_micros < min_jitter_u { min_jitter_u = s.min_micros; }
    }
    let avg_sensor_jitter_us = if total_jitter_count == 0 { 0 } else { total_jitter_sum / total_jitter_count as u128 };

    // Scheduling drift
    let sched_avg = m.scheduling_drift.avg_micros();
    let sched_max = m.scheduling_drift.max_micros;
    let sched_min = if m.scheduling_drift.min_micros == u128::MAX { 0 } else { m.scheduling_drift.min_micros };

    // Backlog avg
    let avg_backlog = if m.backlog_count == 0 { 0.0 } else { (m.backlog_sum as f64) / (m.backlog_count as f64) };

    // Fault recovery avg ms
    let avg_fault_recovery = if m.fault_recoveries == 0 { 0 } else { m.fault_recovery_sum_ms / (m.fault_recoveries as u128) };

    // CPU utilization
    let cpu_active_ms = cpu_active.as_secs_f64() * 1000.0;
    let cpu_total_ms = run_dur.as_secs_f64() * 1000.0;
    let cpu_usage = if cpu_total_ms > 0.0 { (cpu_active_ms / cpu_total_ms) * 100.0 } else { 0.0 };

    println!("\n=========== 📊 SATELLITE SYSTEM PERFORMANCE REPORT ===========");
    println!("🕒 Monitoring Duration: {:.2}s", run_secs);
    println!("⏱️  Avg Sensor Jitter: {} µs | Max: {} µs | Min: {} µs", avg_sensor_jitter_us, max_jitter_u, if min_jitter_u==u128::MAX {0} else {min_jitter_u});
    println!("📉 Scheduling Drift:\n   • Avg: {} µs\n   • Max: {} µs\n   • Min: {} µs", sched_avg, sched_max, sched_min);
    println!("❗ Missed Deadlines: {}", m.missed_deadlines);
    println!("🚨 Safety Alerts / Fault Recoveries: {} (avg recovery: {} ms)", m.fault_recoveries, avg_fault_recovery);
    println!("📦 Buffer / Downlink Stats:\n   • Downlink Sent: {}\n   • Downlink Dropped: {}\n   • Dropped Samples: {}\n   • Avg Backlog: {:.1} | Max: {} | Min: {}", 
        m.downlink_sent, m.downlink_dropped, m.dropped_samples, avg_backlog, m.backlog_max, if m.backlog_min==usize::MAX {0} else {m.backlog_min});
    println!("📦 Packets Generated: {} packets", m.packets_generated);
    println!("🧠 CPU Utilization:\n   • Active Time: {:.0} ms\n   • Total Time:  {:.2} s\n   • Usage:       {:.2}%", cpu_active_ms, run_secs, cpu_usage);
    println!("=============================================================\n");
}
