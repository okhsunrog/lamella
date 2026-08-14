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
use icd::WifiTransaction;
use log::{info, warn};
use std::{future::Future, io, pin::pin, time::Duration};
use tokio::{
    select,
    time::{Instant, sleep, timeout},
};
use tokio_util::sync::CancellationToken;

use crate::{ESP32_NODE_ID, MAC_QUERY_RETRIES, MAC_QUERY_RETRY_DELAY_MS, MAC_QUERY_TIMEOUT_MS};

const ENDPOINT_STALL_THRESHOLD: Duration = Duration::from_millis(250);
const ENDPOINT_STALL_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// Await one Ergot request without dropping its response socket on a slow peer.
///
/// Dropping and recreating the request future on a timeout gives every retry a
/// new response port. A late response is then undeliverable, while duplicate
/// requests fill the remote bounded endpoint. Keep the original future alive
/// and use the timer only for stall diagnostics.
pub async fn await_endpoint_response<F>(
    response: F,
    direction: &'static str,
    transaction: WifiTransaction,
    cancel: &CancellationToken,
) -> Option<(F::Output, Option<Duration>)>
where
    F: Future,
{
    let started = Instant::now();
    let mut response = pin!(response);
    let warning = sleep(ENDPOINT_STALL_THRESHOLD);
    let mut warning = pin!(warning);
    let mut stalled = false;

    loop {
        select! {
            result = &mut response => {
                let elapsed = started.elapsed();
                return Some((result, stalled.then_some(elapsed)));
            }
            _ = &mut warning => {
                stalled = true;
                warn!(
                    "{direction} response still pending after {}ms for transaction {:?}",
                    started.elapsed().as_millis(),
                    transaction,
                );
                warning.as_mut().reset(Instant::now() + ENDPOINT_STALL_LOG_INTERVAL);
            }
            _ = cancel.cancelled() => return None,
        }
    }
}

/// Query MAC address with retries (NUSB transport)
pub async fn query_mac_with_retry_nusb(
    stack: &NusbRouterStack,
    interface_id: u8,
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
    interface_id: u8,
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

/// Resolve the ESP32 peer address for an active serial interface.
pub fn serial_peer_address(stack: &SerialRouterStack, interface_id: u8) -> io::Result<Address> {
    let net_id = stack
        .manage_profile(|im| im.interface_state(interface_id))
        .and_then(|state| match state {
            InterfaceState::Active { net_id, node_id: _ } => Some(net_id),
            _ => None,
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "No active interface"))?;

    Ok(Address {
        network_id: net_id,
        node_id: ESP32_NODE_ID,
        port_id: 0,
    })
}

/// Resolve the ESP32 peer address for an active USB interface.
pub fn nusb_peer_address(stack: &NusbRouterStack, interface_id: u8) -> io::Result<Address> {
    let net_id = stack
        .manage_profile(|im| im.interface_state(interface_id))
        .and_then(|state| match state {
            InterfaceState::Active { net_id, node_id: _ } => Some(net_id),
            _ => None,
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "No active interface"))?;

    Ok(Address {
        network_id: net_id,
        node_id: ESP32_NODE_ID,
        port_id: 0,
    })
}

async fn query_mac_for_interface_nusb(
    stack: &NusbRouterStack,
    interface_id: u8,
) -> io::Result<[u8; 6]> {
    let addr = nusb_peer_address(stack, interface_id)?;

    stack
        .endpoints()
        .request::<GetMacEndpoint>(addr, &(), Some("mac"))
        .await
        .map_err(|err| io::Error::other(format!("{:?}", err)))
}

async fn query_mac_for_interface_serial(
    stack: &SerialRouterStack,
    interface_id: u8,
) -> io::Result<[u8; 6]> {
    let addr = serial_peer_address(stack, interface_id)?;

    stack
        .endpoints()
        .request::<GetMacEndpoint>(addr, &(), Some("mac"))
        .await
        .map_err(|err| io::Error::other(format!("{:?}", err)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn slow_endpoint_response_is_not_replaced_after_warning() {
        let cancel = CancellationToken::new();
        let transaction = WifiTransaction { session: 7, id: 9 };

        let result = await_endpoint_response(
            async {
                sleep(Duration::from_millis(300)).await;
                42
            },
            "test",
            transaction,
            &cancel,
        )
        .await;

        let (value, stalled_for) = result.expect("request should complete");
        assert_eq!(value, 42);
        assert!(stalled_for.is_some_and(|duration| duration >= ENDPOINT_STALL_THRESHOLD));
    }
}
