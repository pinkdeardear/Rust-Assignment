mod telemetry;
mod fault;
mod monitor;
mod utils;

mod types;
mod sensor;
mod task;
mod downlink;

use utils::logger::{init_logger, log_event};
use utils::timing::current_timestamp;
use crossbeam_channel::{bounded, unbounded};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use std::thread::{self, sleep, spawn};

use crate::monitor::{periodic_monitoring, MONITOR_DATA};
use crate::types::{DownlinkPacket};
use crate::task::{run_system_scheduler, get_total_active_time};
use crate::sensor::run_sensor;
use crate::downlink::{run_ground, start_downlink_manager, report_cpu_utilization};

fn main() {
    // ✅ Initialize logging
    init_logger();
    unsafe { std::env::set_var("RUST_LOG", "info"); }
    env_logger::init();

    log_event(&format!(
        "[{}] >>> Integrated Ground & Sensor Simulation Starting",
        current_timestamp()
    ));

    let start_time = Instant::now();
    let total_duration = Duration::from_secs(30);

    // =====================================================
    // ✅ Part 1: Ground Control (monitoring + telemetry)
    // =====================================================
    // uplink: ground -> satellite ACKs (downlink manager will read these)
    let (uplink_tx, uplink_rx) = bounded::<String>(50);

    // internal channel used by downlink manager to send serialized WirePacket JSON
    // forwarder will duplicate each message to both the ground station and telemetry monitor
    let (ground_internal_tx, ground_internal_rx) = unbounded::<String>();

    // channel consumed by run_ground (ground station receives serialized packets here)
    let (ground_tx, ground_rx) = unbounded::<String>();

    // channel consumed by telemetry monitor (receives same serialized JSON)
    let (monitor_tx, monitor_rx) = unbounded::<String>();

    // ground log channel for internal logs/messages from tasks
    let (ground_log_tx, ground_log_rx) = bounded::<String>(50);

    // Start telemetry system (it expects Receiver<String> of serialized WirePacket JSON)
    let _telemetry_event_tx = telemetry::start_telemetry_system(monitor_rx.clone());

    // Monitoring data and telemetry backlog used by periodic_monitoring
    let monitor_data = Arc::clone(&MONITOR_DATA);
    let telemetry_backlog = Arc::new(Mutex::new(VecDeque::new()));
    let expected_interval = Duration::from_secs(1);

    // Monitoring thread
    {
        let monitor_clone = Arc::clone(&monitor_data);
        let backlog_clone = Arc::clone(&telemetry_backlog);
        spawn(move || {
            periodic_monitoring(monitor_clone, backlog_clone, expected_interval);
        });
    }

    // Forwarder thread: read serialized strings from ground_internal_rx and duplicate to
    // run_ground (ground_rx) and the telemetry monitor (monitor_tx).
    {
        let ground_tx_clone = ground_tx.clone();
        let monitor_tx_clone = monitor_tx.clone();
        thread::spawn(move || {
            while let Ok(serialized) = ground_internal_rx.recv() {
                // forward to ground station
                let _ = ground_tx_clone.send(serialized.clone());
                // forward to telemetry monitoring
                let _ = monitor_tx_clone.send(serialized);
            }
            // when ground_internal_rx closes, forwarder exits
        });
    }

    // =====================================================
    // ✅ Part 2: Downlink System
    // =====================================================
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let critical_sensor_flag = Arc::new(AtomicBool::new(false));
    let (shutdown_tx, shutdown_rx) = bounded::<()>(1);

    // Downlink channel (satellite tasks -> downlink manager)
    let (downlink_tx, downlink_rx) = bounded::<DownlinkPacket>(10);

    // Start downlink manager:
    // - it will send serialized JSON into ground_internal_tx
    // - it will listen for ACKs on uplink_rx
    start_downlink_manager(downlink_rx, ground_internal_tx.clone(), uplink_rx.clone());

    // =====================================================
    // ✅ Part 3: Sensors / Tasks
    // =====================================================
    const SENSOR_BUFFER_SIZE: usize = 64;
    let (sensor_tx, sensor_rx) = bounded(SENSOR_BUFFER_SIZE);

    // Fault flags
    let temp_fault = Arc::new(AtomicBool::new(false));
    let accel_fault = Arc::new(AtomicBool::new(false));
    let gyro_fault = Arc::new(AtomicBool::new(false));

    let temp_fault_start = Arc::new(Mutex::new(None::<Instant>));
    let accel_fault_start = Arc::new(Mutex::new(None::<Instant>));
    let gyro_fault_start = Arc::new(Mutex::new(None::<Instant>));

    let temp_jitter_tracker = Arc::new(Mutex::new(crate::types::JitterTracker::new(Duration::from_secs(1))));

    // Launch sensors
    run_sensor("TempSensor".into(), 1, Duration::from_millis(10), sensor_tx.clone(), temp_fault.clone(), temp_fault_start.clone(), shutdown_flag.clone());
    run_sensor("AccelSensor".into(), 2, Duration::from_millis(12), sensor_tx.clone(), accel_fault.clone(), accel_fault_start.clone(), shutdown_flag.clone());
    run_sensor("GyroSensor".into(), 3, Duration::from_millis(15), sensor_tx.clone(), gyro_fault.clone(), gyro_fault_start.clone(), shutdown_flag.clone());

    // Shared task consuming sensor data (it will send DownlinkPacket into downlink_tx)
    task::run_task(
        "SharedTask".to_string(),
        sensor_rx,
        ground_log_tx.clone(),
        Some(temp_jitter_tracker.clone()),
        Some(downlink_tx.clone()),
        shutdown_flag.clone(),
        critical_sensor_flag.clone(),
    );

    // Scheduler
    run_system_scheduler(ground_log_tx.clone(), shutdown_flag.clone(), critical_sensor_flag.clone());

    // Small logger thread to print messages coming on ground_log_rx channel
    {
        let ground_log_rx_clone = ground_log_rx.clone();
        thread::spawn(move || {
            while let Ok(msg) = ground_log_rx_clone.recv() {
                println!("[GROUND LOG] {}", msg);
            }
        });
    }

    // Ground station: run_ground consumes serialized packets (Receiver<String>) and sends ACKs via uplink_tx
    run_ground(ground_rx, shutdown_rx.clone(), uplink_tx.clone());

    // =====================================================
    // ✅ Part 4: Fault Injector
    // =====================================================
    const FAULT_INJECT_PERIOD_SECS: u64 = 60; // use 60 for assignment runtime, 10 makes local testing quicker
    {
        let temp_fault = temp_fault.clone();
        let accel_fault = accel_fault.clone();
        let gyro_fault = gyro_fault.clone();
        let temp_fault_start = temp_fault_start.clone();
        let accel_fault_start = accel_fault_start.clone();
        let gyro_fault_start = gyro_fault_start.clone();
        let shutdown_flag = shutdown_flag.clone();
        let downlink_tx_clone = Some(downlink_tx.clone());

        fault::start_fault_injector(shutdown_flag.clone(), downlink_tx_clone);

        thread::spawn(move || {
            while !shutdown_flag.load(Ordering::SeqCst) {
                sleep(Duration::from_secs(FAULT_INJECT_PERIOD_SECS));

                let fault_start = Instant::now();
                temp_fault.store(true, Ordering::SeqCst);
                accel_fault.store(true, Ordering::SeqCst);
                gyro_fault.store(true, Ordering::SeqCst);

                {
                    *temp_fault_start.lock().unwrap() = Some(fault_start);
                    *accel_fault_start.lock().unwrap() = Some(fault_start);
                    *gyro_fault_start.lock().unwrap() = Some(fault_start);
                }

                log_event(&format!(
                    "🔧 [FAULT INJECTOR] Faults injected at {}",
                    current_timestamp()
                ));

                // Fault duration
                sleep(Duration::from_millis(300));

                temp_fault.store(false, Ordering::SeqCst);
                accel_fault.store(false, Ordering::SeqCst);
                gyro_fault.store(false, Ordering::SeqCst);

                let fault_end = Instant::now();
                log_event(&format!(
                    "🔧 [FAULT INJECTOR] Faults cleared {}",
                    current_timestamp()
                ));

                // Recovery time calculation
                let recovery_time = fault_end.duration_since(fault_start);
                log_event(&format!(
                    "🔧 [FAULT INJECTOR] Recovery time: {} ms",
                    recovery_time.as_millis()
                ));

                crate::monitor::record_fault_recovery(recovery_time.as_millis());

                if recovery_time > Duration::from_millis(200) {
                    log_event(&format!(
                        "🚨 [FAULT INJECTOR] Recovery time exceeded 200ms! Aborting mission..."
                    ));
                    shutdown_flag.store(true, Ordering::SeqCst);
                    break;
                }
            }
        });
    }

    // =====================================================
    // ✅ Part 5: Main Simulation Loop
    // =====================================================
    while start_time.elapsed() < total_duration && !shutdown_flag.load(Ordering::SeqCst) {
        // Push telemetry packet backlog for monitoring
        {
            let mut backlog = telemetry_backlog.lock().unwrap();
            backlog.push_back(1);
        }
        {
            let mut monitor = monitor_data.lock().unwrap();
            monitor.packets_processed += 1;
        }

        // Handle faults
        fault::handle_faults();

        // Report CPU utilization
        report_cpu_utilization(start_time.elapsed());

        sleep(Duration::from_millis(500));
    }

    // =====================================================
    // ✅ Part 6: Shutdown
    // =====================================================
    shutdown_flag.store(true, Ordering::SeqCst);
    let _ = shutdown_tx.send(());

    log::set_max_level(log::LevelFilter::Off);
    log_event(&format!(
        "[{}] >>> Integrated Simulation Ended",
        current_timestamp()
    ));

    monitor::print_monitoring_report(monitor_data.clone());

    let cpu_active = get_total_active_time();
    monitor::print_satellite_report(start_time, cpu_active);

    println!("✅ All systems shut down.");
}
