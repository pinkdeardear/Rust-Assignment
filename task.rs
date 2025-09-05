use crate::types::{SensorData, JitterTracker, SystemTask};
use chrono::Utc;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use log::{info, warn};
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicBool, Ordering};
use crate::types::{DownlinkPacket};

lazy_static::lazy_static! {
    static ref SYSTEM_START_TIME: Instant = Instant::now();
    static ref TOTAL_ACTIVE_TIME: Arc<Mutex<Duration>> = Arc::new(Mutex::new(Duration::ZERO));
    static ref LAST_UTILIZATION_LOG: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now()));
    static ref SENSOR_BUFFER_COUNTER: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    static ref CURRENT_RUNNING_TASK: Arc<Mutex<Option<SystemTask>>> = Arc::new(Mutex::new(None));
    static ref MISSED_CYCLES: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    pub static ref DOWNLINK_BUFFER: Mutex<VecDeque<crate::types::DownlinkPacket>> = Mutex::new(VecDeque::new());

}

pub const MAX_DOWNLINK_BUFFER: usize = 128; 


pub fn run_task(
    task_name: String,
    sensor_rx: Receiver<SensorData>,
    ground_logger_tx: Sender<String>,
    jitter_tracker: Option<Arc<Mutex<JitterTracker>>>,
    downlink_tx: Option<Sender<DownlinkPacket>>,
    shutdown_flag: Arc<AtomicBool>,
    critical_sensor_flag: Arc<AtomicBool>,
) {
    // Keep a local priority queue for samples
    let buffer_limit = 512;
    let mut buffer: VecDeque<SensorData> = VecDeque::new();

    thread::spawn(move || {
        // Drive a 1ms processing cadence, but allow burst catch-up
        let mut expected_time = Instant::now();
        let tick = Duration::from_millis(1);

        let mut last_received_instant: Option<Instant> = None;
        let mut missed_cycles = 0usize;

        loop {
            if shutdown_flag.load(Ordering::SeqCst) {
                info!("[{}] 🛑 Shutting down task thread.", task_name);
                break;
            }

            // === ingest samples (non-blocking) ===================================================
            while let Ok(mut data) = sensor_rx.try_recv() {
                data.queued_at = Utc::now();

                let insertion_latency = data.queued_at.signed_duration_since(data.generated_at);
                info!(
                    "[{}] 📦 [{}] Buffer: {} | Insert latency: {} ms",
                    task_name,
                    data.name,
                    buffer.len(),
                    insertion_latency.num_milliseconds()
                );

                crate::monitor::record_insert_latency(
                    &data.name,
                    Duration::from_millis(insertion_latency.num_milliseconds().max(0) as u64),
                );
                crate::monitor::record_backlog(buffer.len());

                last_received_instant = Some(Instant::now());
                missed_cycles = 0;

                // Capacity management: drop the lowest-priority resident if full
                if buffer.len() >= buffer_limit {
                    if let Some(lowest) = buffer.iter().min_by_key(|d| d.priority) {
                        if data.priority < lowest.priority {
                            // incoming is lower priority than our lowest resident → drop incoming
                            warn!(
                                "🚩 [{}] Dropping low-priority sample ({}: {:.2}, P: {})",
                                task_name, data.name, data.value, data.priority
                            );
                            crate::monitor::record_buffer_drop();
                            continue;
                        }
                        if let Some(pos) = buffer.iter().position(|x| x.priority == lowest.priority) {
                            let dropped = buffer.remove(pos).unwrap();
                            warn!(
                                "🚨 [{}] Buffer full! Dropped ({}: {:.2}, P: {})",
                                task_name, dropped.name, dropped.value, dropped.priority
                            );
                            crate::monitor::record_buffer_drop();
                        }
                    }
                }

                // Priority insert (larger priority = earlier)
                let insert_pos = buffer
                    .iter()
                    .position(|d| data.priority > d.priority)
                    .unwrap_or(buffer.len());
                buffer.insert(insert_pos, data);
            }

            // === missed data / safety check ======================================================
            if let Some(last) = last_received_instant {
                let elapsed = Instant::now().duration_since(last);
                let last_sensor = buffer.back().map(|d| d.name.clone()).unwrap_or_default();
                let expected_interval = match last_sensor.as_str() {
                    "TempSensor" => Duration::from_millis(10),
                    "AccelSensor" => Duration::from_millis(12),
                    "GyroSensor"  => Duration::from_millis(15),
                    _             => Duration::from_millis(30),
                };

                if elapsed > expected_interval * 3 {
                    missed_cycles += 1;
                    if last_sensor == "TempSensor" && missed_cycles >= 3 {
                        warn!(
                            "[{}] 🚨 SAFETY ALERT: No {} data for {:.2} ms (>3 cycles)!",
                            task_name, last_sensor, elapsed.as_secs_f64() * 1000.0
                        );
                    }
                } else {
                    missed_cycles = 0;
                }
            }

            // === processing window ===============================================================
            // adapt how much we drain this tick; if backlog is big, drain more
            let backlog = buffer.len();
            let max_per_loop = if backlog > 256 {
                256
            } else if backlog > 96 {
                128
            } else if backlog > 32 {
                64
            } else {
                32
            };

            let mut handled = 0;
            while Instant::now() >= expected_time && handled < max_per_loop {
                expected_time += tick;
                handled += 1;

                if let Some(data) = buffer.pop_front() {
                    let now = Instant::now();
                    let adjusted_expected = expected_time.checked_sub(tick).unwrap_or(expected_time);
                    let processing_drift = now.duration_since(adjusted_expected);

                    let e2e = Utc::now().signed_duration_since(data.generated_at);
                    let init_delay_ms = e2e.num_milliseconds().max(0) as u64;

                    info!(
                        "📅 [{}] Processing {} {:.2} (P{}) | Drift: {}ms | E2E: {}ms | Buf rem: {}",
                        task_name,
                        data.name,
                        data.value,
                        data.priority,
                        processing_drift.as_millis(),
                        e2e.num_milliseconds(),
                        buffer.len()
                    );
                    crate::monitor::record_sched_drift(processing_drift);

                    if init_delay_ms > 5 {
                        warn!(
                            "[{}] 🚫 MISSED DOWNLINK INIT for {}: {} ms (>5ms)",
                            task_name, data.name, init_delay_ms
                        );
                        crate::monitor::record_missed_downlink(&data.name, init_delay_ms as i64);
                    }

                    // flag critical when we see TempSensor
                    if data.name == "TempSensor" {
                        critical_sensor_flag.store(true, Ordering::SeqCst);
                    }

                    // Simulate execution w/ short preemptible quanta (no 1ms coarse sleeps)
                    let exec_budget = Duration::from_millis(1); // nominal per-sample work
                    let exec_start = Instant::now();
                    let exec_end   = exec_start + exec_budget;

                    // pretend to do some math work
                    let _dummy = data.value * 1.0;

                    loop {
                        if shutdown_flag.load(Ordering::SeqCst) { break; }

                        // Give scheduler a chance to run and reduce OS jitter
                        thread::sleep(Duration::from_micros(200));

                        // done?
                        if Instant::now() >= exec_end { break; }

                        // If TempSensor flagged, let the system scheduler handle preemption
                        if critical_sensor_flag.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    let exec_duration = exec_start.elapsed();

                    let exec_jitter = if exec_duration > exec_budget {
                        exec_duration - exec_budget
                    } else {
                        exec_budget - exec_duration
                    };

                    info!(
                        "[{}] [{}] Exec took {} ms | Jitter: {} ms",
                        task_name,
                        data.name,
                        exec_duration.as_millis(),
                        exec_jitter.as_millis()
                    );

                    if let Some(tracker) = &jitter_tracker {
                        if let Ok(mut jt) = tracker.lock() {
                            jt.record(now);
                        }
                    }

                    // === Downlink (non-blocking ACK) ============================================
                    if let Some(downlink_tx) = &downlink_tx {
                        let (response_tx, response_rx) = crossbeam_channel::bounded::<String>(1);

                        let cur_buf_len = {
                            let guard = DOWNLINK_BUFFER.lock().unwrap();
                            guard.len()
                        };

                        let packet: DownlinkPacket = crate::downlink::compress_and_packetize(
                            data.clone(),
                            response_tx.clone(),
                            cur_buf_len,
                            MAX_DOWNLINK_BUFFER,
                        );

                        let prep_elapsed = (data.queued_at - data.generated_at).num_milliseconds();
                        if prep_elapsed > 30 {
                            warn!(
                                "[{}] 🚫 MISSED COMMUNICATION: Packet prepared {} ms after generation (>30ms) for {}",
                                task_name, prep_elapsed, data.name
                            );
                            crate::monitor::record_missed_downlink(&data.name, prep_elapsed);
                        }

                        {
                            let mut ring = DOWNLINK_BUFFER.lock().unwrap();
                            if ring.len() >= MAX_DOWNLINK_BUFFER {
                                warn!(
                                    "❌ DOWNLINK BUFFER FULL: Dropping {} packet | buffer={}/{}",
                                    data.name, ring.len(), MAX_DOWNLINK_BUFFER
                                );
                                crate::monitor::record_buffer_drop();
                            } else {
                                ring.push_back(packet.clone());
                                let usage = ring.len() as f32 / MAX_DOWNLINK_BUFFER as f32;
                                if usage > 0.8 {
                                    warn!(
                                        "⚠️ DOWNLINK DEGRADED MODE: buffer at {:.0}% ({} data)",
                                        usage * 100.0, data.name
                                    );
                                    crate::monitor::trigger_degraded_mode(usage);
                                }
                            }
                        }

                        match downlink_tx.try_send(packet.clone()) {
                            Ok(_) => {
                                let prep_t = Instant::now().duration_since(*SYSTEM_START_TIME);
                                info!(
                                    "📨 [{}] [{}] Downlink queued: id={} | Prep latency ≈ {}ms",
                                    task_name, data.name, packet.id, prep_t.as_millis()
                                );
                                crate::monitor::record_downlink_sent();

                                // NON-BLOCKING ACK: do not park the task thread
                                match response_rx.try_recv() {
                                    Ok(resp) => {
                                        let rtt = Instant::now().duration_since(*SYSTEM_START_TIME);
                                        let msg = format!(
                                            "📬 [{}] [{}] Ground response received for pkt id={} | resp='{}' | RTT ≈ {} ms",
                                            task_name, data.name, packet.id, resp, rtt.as_millis()
                                        );
                                        info!("{}", msg);
                                        let _ = ground_logger_tx.send(msg);

                                        let cmd_exec_msg = format!(
                                            "[{}] Executing command from ground: '{}'",
                                            task_name, resp
                                        );
                                        info!("{}", cmd_exec_msg);
                                        let _ = ground_logger_tx.send(cmd_exec_msg);
                                    }
                                    Err(TryRecvError::Empty) => {
                                        // ok, keep flowing
                                    }
                                    Err(e) => {
                                        warn!(
                                            "[{}] [{}] Error receiving ground response for pkt id={}: {:?}",
                                            task_name, data.name, packet.id, e
                                        );
                                        crate::monitor::record_downlink_dropped();
                                    }
                                }
                            }
                            Err(err) => {
                                warn!(
                                    "🚫 [{}] [{}] Downlink packet DROPPED: id={} | Reason: {}",
                                    task_name, data.name, packet.id, err
                                );
                                crate::monitor::record_downlink_dropped();
                            }
                        }
                    }
                } else {
                    // nothing to process for this 1ms slot
                    continue;
                }
            }

            // adaptive sleep: smaller when backlog exists
            if buffer.is_empty() {
                thread::sleep(Duration::from_millis(2));
            } else {
                thread::sleep(Duration::from_micros(200));
            }
        }
    });
}


fn _simulate_work(task_name: &str, ms: u64, ground_logger_tx: &Sender<String>) {
    let start = Instant::now();
    thread::sleep(Duration::from_millis(ms));
    let elapsed = start.elapsed();

    let msg = format!("✅ [{}] Completed in {:?}", task_name, elapsed);
    info!("{}", msg);
    ground_logger_tx.send(msg).ok();
}

pub fn run_system_scheduler(
    ground_logger_tx: Sender<String>,
    shutdown_flag: Arc<AtomicBool>,
    critical_sensor_flag: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let cycle_duration = Duration::from_secs(1);
        let base_time = Instant::now();

        let mut task_list = vec![
            SystemTask {
                name: "Data Compression".into(),
                period: Duration::from_millis(100),
                execution_time: Duration::from_millis(30),
                priority: 2,
                next_release: base_time + Duration::from_millis(20),
                deadline:     base_time + Duration::from_millis(120),
            },
            SystemTask {
                name: "Health Monitoring".into(),
                period: Duration::from_millis(200),
                execution_time: Duration::from_millis(7),
                priority: 1,
                next_release: base_time,
                deadline:     base_time + Duration::from_millis(200),
            },
            SystemTask {
                name: "Antenna Alignment".into(),
                period: Duration::from_millis(300),
                execution_time: Duration::from_millis(100),
                priority: 3,
                next_release: base_time,
                deadline:     base_time + Duration::from_millis(300),
            },
        ];

        loop {
            if shutdown_flag.load(Ordering::SeqCst) {
                info!("🛑 [Scheduler] Shutdown signal received. Exiting scheduler thread.");
                break;
            }

            let cycle_start = Instant::now();
            let mut cycle_active_time = Duration::ZERO;
            let mut ready_queue: BinaryHeap<SystemTask> = BinaryHeap::new();

            let now = Instant::now();

            // Rate Monotonic: shorter period ⇒ higher priority (1 is highest)
            {
                let mut order: Vec<(usize, Duration)> = task_list
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (i, t.period))
                    .collect();
                order.sort_by_key(|(_, p)| *p);
                for (pri_index, (idx, _)) in order.iter().enumerate() {
                    task_list[*idx].priority = (pri_index + 1) as u8;
                }
            }

            // Releases: at most one job per task per tick to avoid release storms
            for task in &mut task_list {
                if now >= task.next_release {
                    task.deadline     = task.next_release + task.period;
                    ready_queue.push(task.clone());
                    task.next_release += task.period;
                }
            }

            // Execute ready jobs with preemption points ~200µs
            while let Some(task) = ready_queue.pop() {
                if shutdown_flag.load(Ordering::SeqCst) {
                    info!("🛑 [Scheduler] Shutdown during task execution.");
                    return;
                }

                let start_time = Instant::now();
                let release_time = task.deadline
                    .checked_sub(task.period)
                    .unwrap_or(task.deadline);

                if start_time > task.deadline {
                    warn!("⚠️ [Scheduler] START Violation: {} missed deadline", task.name);
                    crate::monitor::record_missed_deadline();
                }

                let start_drift = start_time.saturating_duration_since(release_time);
                info!("⏰ [Scheduler] Task {} drifted by {}ms", task.name, start_drift.as_millis());
                if start_drift > Duration::from_millis(5) {
                    warn!("⚠️ [Scheduler] High start drift detected on task {}: {}ms",
                          task.name, start_drift.as_millis());
                }

                // Preemptible run until end_time
                let mut preempted = false;
                let end_time = start_time + task.execution_time;

                while Instant::now() < end_time {
                    if shutdown_flag.load(Ordering::SeqCst) { return; }

                    // mid-cycle releases
                    let now = Instant::now();
                    for t in &mut task_list {
                        if now >= t.next_release {
                            info!("🔄 [Scheduler] Releasing {} mid-cycle", t.name);
                            t.deadline = t.next_release + t.period;
                            ready_queue.push(t.clone());
                            t.next_release += t.period;
                        }
                    }

                    // critical sensor preemption
                    if critical_sensor_flag.load(Ordering::SeqCst) {
                        info!("🔴 [Scheduler] Preempting '{}' for TempSensor (critical)!", task.name);
                        let remaining = end_time.saturating_duration_since(Instant::now());
                        if remaining > Duration::from_millis(0) {
                            ready_queue.push(SystemTask {
                                name: task.name.clone(),
                                period: task.period,
                                execution_time: remaining,
                                priority: task.priority,
                                next_release: Instant::now(),
                                deadline: task.deadline,
                            });
                        }
                        // simulate quick thermal handling
                        let thermal = Duration::from_millis(5);
                        thread::sleep(thermal);
                        cycle_active_time += thermal;
                        *TOTAL_ACTIVE_TIME.lock().unwrap() += thermal;

                        critical_sensor_flag.store(false, Ordering::SeqCst);
                        preempted = true;
                        break;
                    }

                    // higher-priority job available?
                    if let Some(high) = ready_queue.peek() {
                        if high.priority < task.priority {
                            info!("🔄 [Scheduler] Preempting '{}' for higher priority '{}'",
                                  task.name, high.name);
                            let remaining = end_time.saturating_duration_since(Instant::now());
                            if remaining > Duration::from_millis(0) {
                                ready_queue.push(SystemTask {
                                    name: task.name.clone(),
                                    period: task.period,
                                    execution_time: remaining,
                                    priority: task.priority,
                                    next_release: now + Duration::from_millis(5),
                                    deadline: task.deadline,
                                });
                            }
                            preempted = true;
                            break;
                        }
                    }

                    // fine-grain wait to reduce jitter vs 1ms ticks
                    thread::sleep(Duration::from_micros(200));
                }

                if !preempted {
                    let exec_duration = Instant::now().saturating_duration_since(start_time);
                    cycle_active_time += exec_duration;
                    *TOTAL_ACTIVE_TIME.lock().unwrap() += exec_duration;

                    let end_time = Instant::now();
                    if end_time > task.deadline {
                        warn!("⚠️ [Scheduler] END Violation: {} completed late", task.name);
                    }

                    // execution jitter vs planned exec time
                    let expected = task.execution_time;
                    let actual   = exec_duration;
                    let task_jitter = if actual > expected { actual - expected } else { expected - actual };

                    let msg = format!(
                        "✅ [{}] Completed in {:?}ms | Exec jitter: {}ms",
                        task.name, exec_duration.as_millis(), task_jitter.as_millis()
                    );
                    if task_jitter > Duration::from_millis(5) {
                        warn!("⚠️ [Scheduler] High execution jitter on task {}: {}ms",
                              task.name, task_jitter.as_millis());
                    }
                    info!("{}", msg);
                    let _ = ground_logger_tx.send(msg);
                }
            }

            // cycle accounting + sleep
            let cycle_elapsed   = cycle_start.elapsed();
            let cycle_idle_time = cycle_duration.saturating_sub(cycle_active_time);
            let utilization =
                (cycle_active_time.as_secs_f64() / cycle_duration.as_secs_f64()) * 100.0;

            info!(
                "⏱️ [Scheduler] Cycle: {:?} | Active: {}ms | Idle: {}ms | Utilization: {:.2}%",
                cycle_elapsed,
                cycle_active_time.as_millis(),
                cycle_idle_time.as_millis(),
                utilization
            );

            // sleep until next cycle boundary
            let sleep_end = Instant::now() + cycle_duration.saturating_sub(cycle_elapsed);
            while Instant::now() < sleep_end {
                if shutdown_flag.load(Ordering::SeqCst) {
                    info!("🛑 [Scheduler] Shutdown during sleep.");
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    });

    start_utilization_logger();
}

pub fn start_utilization_logger() {
    thread::spawn(|| loop {
        thread::sleep(Duration::from_secs(1));
        let active = TOTAL_ACTIVE_TIME.lock().unwrap().as_secs_f64() * 1000.0;
        let total  = SYSTEM_START_TIME.elapsed().as_secs_f64() * 1000.0;
        let util   = if total > 0.0 { (active / total) * 100.0 } else { 0.0 };
        info!("📊 [CPU UTIL] Active: {:.2}ms / Total: {:.2}ms = {:.2}%", active, total, util);
    });
}



pub fn get_total_active_time() -> Duration {
    TOTAL_ACTIVE_TIME.lock().unwrap().clone()
}

