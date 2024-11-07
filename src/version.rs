use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Serialize, Deserialize, Debug)]
pub struct Version(AtomicU64);

impl Version {
    pub fn new(value: u64) -> Self {
        Version(AtomicU64::new(value))
    }

    pub fn increment(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst)
    }

    pub fn load(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
