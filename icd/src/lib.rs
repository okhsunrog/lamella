#![no_std]

use ergot::{endpoint, topic};
use heapless::Vec;
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};

// Maximum Ethernet frame size (MTU 1500 + headers)
pub const MAX_FRAME_SIZE: usize = 1514;

// Get WiFi MAC address from ESP32
endpoint!(GetMacEndpoint, (), [u8; 6], "wifi/mac");

#[derive(Serialize, Deserialize, Schema, Clone, Debug)]
pub struct WifiFrame {
    pub data: Vec<u8, MAX_FRAME_SIZE>,
}

// Reliably hand one Ethernet frame to the ESP32 WiFi driver. The response is
// sent only after the driver has accepted the frame for transmission.
endpoint!(WifiTxEndpoint, WifiFrame, (), "wifi/tx");

// Topic for frames coming from WiFi (ESP32-S3 publishes, host subscribes)
topic!(WifiRxTopic, WifiFrame, "wifi/rx");

// Legacy best-effort path used by the ESP32-S3 firmware.
topic!(WifiTxTopic, WifiFrame, "wifi/tx");

// Ping topic for testing
topic!(PingTopic, u64, "ping/data");
