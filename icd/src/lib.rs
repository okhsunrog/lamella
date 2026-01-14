#![no_std]

use ergot::{endpoint, topic};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};

// LED control endpoint
endpoint!(LedEndpoint, bool, (), "led/set");

// Ping topic for testing
topic!(PingTopic, u64, "ping/data");

// Example data structure for network status
#[derive(Serialize, Deserialize, Schema, Default, Clone, Debug)]
pub struct NetworkStatus {
    pub connected: bool,
    pub ip_address: [u8; 4],
    pub packets_rx: u32,
    pub packets_tx: u32,
}

topic!(NetworkStatusTopic, NetworkStatus, "network/status");

// Generic command endpoint
#[derive(Serialize, Deserialize, Schema, Clone, Debug)]
pub enum Command {
    Reset,
    GetStatus,
    SetConfig { key: u8, value: u32 },
}

#[derive(Serialize, Deserialize, Schema, Clone, Debug)]
pub enum CommandResponse {
    Ok,
    Error(u8),
    Status(NetworkStatus),
}

endpoint!(CommandEndpoint, Command, CommandResponse, "cmd");
