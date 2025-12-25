#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QoS {
    Lossy,
    Reliable { timeout_ms: u64 },
}

impl QoS {
    pub fn reliable_default() -> Self {
        QoS::Reliable { timeout_ms: 20 }
    }
}
