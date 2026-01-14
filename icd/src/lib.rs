#![no_std]

use ergot::{endpoint, topic};
use heapless::Vec;
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};

// Maximum Ethernet frame size (MTU 1500 + headers)
pub const MAX_FRAME_SIZE: usize = 1514;

// Get WiFi MAC address from ESP32
endpoint!(GetMacEndpoint, (), [u8; 6], "wifi/mac");

// Ethernet frame from WiFi to host (ESP32-S3 -> Host)
#[derive(Serialize, Deserialize, Schema, Clone, Debug)]
pub struct WifiFrame {
    pub data: Vec<u8, MAX_FRAME_SIZE>,
}

// Topic for frames coming from WiFi (ESP32-S3 publishes, host subscribes)
topic!(WifiRxTopic, WifiFrame, "wifi/rx");

// Topic for frames going to WiFi (host publishes, ESP32-S3 subscribes)
topic!(WifiTxTopic, WifiFrame, "wifi/tx");

// Ping topic for testing
topic!(PingTopic, u64, "ping/data");
