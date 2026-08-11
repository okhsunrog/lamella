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

#[derive(Serialize, Deserialize, Schema, Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiTransaction {
    pub session: u64,
    pub id: u32,
}

#[derive(Serialize, Deserialize, Schema, Clone, Debug)]
pub struct WifiTxRequest {
    pub transaction: WifiTransaction,
    pub frame: WifiFrame,
}

#[derive(Serialize, Deserialize, Schema, Clone, Copy, Debug)]
pub struct WifiTxResponse {
    pub transaction: WifiTransaction,
}

#[derive(Serialize, Deserialize, Schema, Clone, Copy, Debug)]
pub struct WifiRxRequest {
    pub transaction: WifiTransaction,
}

#[derive(Serialize, Deserialize, Schema, Clone, Debug)]
pub struct WifiRxResponse {
    pub transaction: WifiTransaction,
    pub frame: Option<WifiFrame>,
}

// Reliably hand one Ethernet frame to the ESP32 WiFi driver. The response is
// sent only after the driver has accepted the frame for transmission.
endpoint!(WifiTxEndpoint, WifiTxRequest, WifiTxResponse, "wifi/tx");

// Short-poll one frame received by the ESP32-C3 WiFi driver. The host
// serializes this with WifiTxEndpoint requests so only one request is ever in
// flight on the USB stream.
endpoint!(WifiRxEndpoint, WifiRxRequest, WifiRxResponse, "wifi/rx");

// Legacy best-effort paths used by the ESP32-S3 firmware.
topic!(WifiRxTopic, WifiFrame, "wifi/rx");
topic!(WifiTxTopic, WifiFrame, "wifi/tx");

// Ping topic for testing
topic!(PingTopic, u64, "ping/data");
