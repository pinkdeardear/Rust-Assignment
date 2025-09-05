use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::cmp::Ordering;
use std::time::{Duration, Instant};
use crossbeam_channel::Sender;
use std::error::Error;

#[derive(Debug, Clone)]
pub struct SensorData {
    pub name: String,
    pub value: f64,
    pub generated_at: DateTime<Utc>,
    pub queued_at: DateTime<Utc>,
    pub priority: u8,
}


#[derive(Debug)]
pub struct JitterTracker {
    expected_interval: Duration,
    last_time: Option<Instant>,
    pub jitters: VecDeque<Duration>,
    pub violations: u32,
}

impl JitterTracker {
    pub fn new(expected_interval: Duration) -> Self {
        Self {
            expected_interval,
            last_time: None,
            jitters: VecDeque::with_capacity(100),
            violations: 0,
        }
    }

    pub fn record(&mut self, now: Instant) {
        if let Some(last) = self.last_time {
            let elapsed = now.duration_since(last);
            let jitter = if elapsed > self.expected_interval {
                elapsed - self.expected_interval
            } else {
                self.expected_interval - elapsed
            };

            // Threshold: 1ms
            if jitter > Duration::from_millis(1) {
                self.violations += 1;
            }

            if self.jitters.len() == 100 {
                self.jitters.pop_front();
            }

            self.jitters.push_back(jitter);
        }

        self.last_time = Some(now);
    }
}

#[derive(Debug, Clone, Eq)]
pub struct SystemTask {
    pub name: String,
    pub period: Duration,
    pub execution_time: Duration,
    pub priority: u8,
    pub next_release: Instant,
    pub deadline: Instant,
}

impl Ord for SystemTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Lower priority number = higher priority
        other.priority.cmp(&self.priority)
    }
}

impl PartialOrd for SystemTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SystemTask {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

#[derive(Debug, Clone)]
pub struct DownlinkPacket {
    pub id: u64,                      // Unique packet ID
    pub sensor_name: String,         // Which sensor
    pub compressed_value: String,    // Simulated compressed data
    pub timestamp: DateTime<Utc>,    // When it was generated
    pub priority: u8,                // For optional sorting
    pub response_tx: Option<Sender<String>>,
    pub queued_at: std::time::Instant,
    pub window_start_at: DateTime<Utc>,
}

use serde::{Serialize, Deserialize};

/// Minimal payload actually sent over the link (no channels/Instant).
#[derive(Debug, Serialize, Deserialize)]
pub struct WirePacket {
    pub id: u64,
    pub sensor_name: String,
    pub compressed_value: String,
    pub timestamp_rfc3339: String,
    pub priority: u8,
}

impl From<&DownlinkPacket> for WirePacket {
    fn from(p: &DownlinkPacket) -> Self {
        Self {
            id: p.id,
            sensor_name: p.sensor_name.clone(),
            compressed_value: p.compressed_value.clone(),
            // avoid needing chrono's serde: just stringify it
            timestamp_rfc3339: p.timestamp.to_rfc3339(),
            priority: p.priority,
        }
    }
}

/// XOR-encrypt a plaintext string into hex.
pub fn xor_encrypt_to_hex(plain: &str, key: &[u8]) -> String {
    if key.is_empty() {
        return String::new();
    }

    let mut out: Vec<u8> = plain.as_bytes().to_vec();
    for (i, b) in out.iter_mut().enumerate() {
        *b ^= key[i % key.len()];
    }
    out.iter().map(|b| format!("{:02X}", b)).collect::<String>()
}

/// XOR-decrypt a hex string back to plaintext
pub fn xor_decrypt_from_hex(cipher_hex: &str, key: &[u8]) -> Result<String, Box<dyn Error>> {
    if key.is_empty() {
        return Err("empty key".into());
    }

    // Convert hex -> bytes
    let bytes = (0..cipher_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cipher_hex[i..i+2], 16))
        .collect::<Result<Vec<u8>, _>>()?;

    // XOR with key
    let mut out = bytes;
    for (i, b) in out.iter_mut().enumerate() {
        *b ^= key[i % key.len()];
    }

    // Convert back to string
    let plaintext = String::from_utf8(out)?;
    Ok(plaintext)
}

/// Shared secret key
pub const ENC_KEY: &[u8] = b"my-secret-key";


