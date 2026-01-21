//! Shared bridge functionality for WiFi <-> TAP forwarding
//!
//! This module provides common functionality shared between nusb and serial transports.

use ergot::{
    Address,
    interface_manager::{InterfaceState, Profile},
    toolkits::{
        nusb_v0_1::RouterStack as NusbRouterStack,
        tokio_serial_v5::RouterStack as SerialRouterStack,
    },
};
use icd::GetMacEndpoint;
use log::info;
use std::{io, time::Duration};
use tokio::time::{sleep, timeout};

use crate::{ESP32_NODE_ID, MAC_QUERY_RETRIES, MAC_QUERY_RETRY_DELAY_MS, MAC_QUERY_TIMEOUT_MS};

/// Query MAC address with retries (NUSB transport)
pub async fn query_mac_with_retry_nusb(
    stack: &NusbRouterStack,
    interface_id: u64,
) -> io::Result<[u8; 6]> {
    let mut last_err: Option<io::Error> = None;
    for attempt in 1..=MAC_QUERY_RETRIES {
        info!(
            "Querying WiFi MAC from ESP32 (attempt {}/{})...",
            attempt, MAC_QUERY_RETRIES
        );
        match timeout(
            Duration::from_millis(MAC_QUERY_TIMEOUT_MS),
            query_mac_for_interface_nusb(stack, interface_id),
        )
        .await
        {
            Ok(Ok(mac)) => return Ok(mac),
            Ok(Err(err)) => {
                last_err = Some(err);
            }
            Err(_) => {
                last_err = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Timed out waiting for ESP32 MAC response",
                ));
            }
        }
        sleep(Duration::from_millis(MAC_QUERY_RETRY_DELAY_MS)).await;
    }

    Err(last_err.unwrap_or_else(|| io::Error::other("Failed to query ESP32 MAC")))
}

/// Query MAC address with retries (Serial transport)
pub async fn query_mac_with_retry_serial(
    stack: &SerialRouterStack,
    interface_id: u64,
) -> io::Result<[u8; 6]> {
    let mut last_err: Option<io::Error> = None;
    for attempt in 1..=MAC_QUERY_RETRIES {
        info!(
            "Querying WiFi MAC from ESP32 (attempt {}/{})...",
            attempt, MAC_QUERY_RETRIES
        );
        match timeout(
            Duration::from_millis(MAC_QUERY_TIMEOUT_MS),
            query_mac_for_interface_serial(stack, interface_id),
        )
        .await
        {
            Ok(Ok(mac)) => return Ok(mac),
            Ok(Err(err)) => {
                last_err = Some(err);
            }
            Err(_) => {
                last_err = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Timed out waiting for ESP32 MAC response",
                ));
            }
        }
        sleep(Duration::from_millis(MAC_QUERY_RETRY_DELAY_MS)).await;
    }

    Err(last_err.unwrap_or_else(|| io::Error::other("Failed to query ESP32 MAC")))
}

async fn query_mac_for_interface_nusb(
    stack: &NusbRouterStack,
    interface_id: u64,
) -> io::Result<[u8; 6]> {
    let net_id = stack
        .manage_profile(|im| im.interface_state(interface_id))
        .and_then(|state| match state {
            InterfaceState::Active { net_id, node_id: _ } => Some(net_id),
            _ => None,
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "No active interface"))?;

    let addr = Address {
        network_id: net_id,
        node_id: ESP32_NODE_ID,
        port_id: 0,
    };

    stack
        .endpoints()
        .request::<GetMacEndpoint>(addr, &(), Some("mac"))
        .await
        .map_err(|err| io::Error::other(format!("{:?}", err)))
}

async fn query_mac_for_interface_serial(
    stack: &SerialRouterStack,
    interface_id: u64,
) -> io::Result<[u8; 6]> {
    let net_id = stack
        .manage_profile(|im| im.interface_state(interface_id))
        .and_then(|state| match state {
            InterfaceState::Active { net_id, node_id: _ } => Some(net_id),
            _ => None,
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "No active interface"))?;

    let addr = Address {
        network_id: net_id,
        node_id: ESP32_NODE_ID,
        port_id: 0,
    };

    stack
        .endpoints()
        .request::<GetMacEndpoint>(addr, &(), Some("mac"))
        .await
        .map_err(|err| io::Error::other(format!("{:?}", err)))
}
