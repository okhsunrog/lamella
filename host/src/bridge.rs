//! Shared bridge functionality for WiFi <-> TAP forwarding
//!
//! This module provides common functionality shared between nusb and serial transports.

use ergot::{
    Address,
    interface_manager::{InterfaceState, Profile},
    net_stack::ReqRespError,
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

pub struct EndpointWaitResult<T> {
    pub result: Result<T, ReqRespError>,
    pub stalled_for: Option<Duration>,
    pub backup_sent: bool,
    pub discarded_errors: u8,
}

pub enum EndpointWaitOutcome<T> {
    Completed(EndpointWaitResult<T>),
    RecoveryRequired { stalled_for: Duration },
    Cancelled,
}

/// Await one Ergot request while retaining its response socket on a slow peer.
///
/// One idempotent backup request covers a request or response lost before the
/// timeout. Both response sockets then remain alive, so a late response is
/// still deliverable and the remote bounded endpoint can contain at most one
/// duplicate instead of an unbounded retry storm.
pub async fn await_endpoint_response<F, B, M, T>(
    primary: F,
    make_backup: M,
    direction: &'static str,
    transaction: WifiTransaction,
    cancel: &CancellationToken,
    recovery_timeout: Option<Duration>,
) -> EndpointWaitOutcome<T>
where
    F: Future<Output = Result<T, ReqRespError>>,
    B: Future<Output = Result<T, ReqRespError>>,
    M: FnOnce() -> B,
{
    await_endpoint_response_with_timeouts(
        primary,
        make_backup,
        direction,
        transaction,
        cancel,
        ENDPOINT_STALL_THRESHOLD,
        recovery_timeout,
    )
    .await
}

async fn await_endpoint_response_with_timeouts<F, B, M, T>(
    primary: F,
    make_backup: M,
    direction: &'static str,
    transaction: WifiTransaction,
    cancel: &CancellationToken,
    stall_threshold: Duration,
    recovery_timeout: Option<Duration>,
) -> EndpointWaitOutcome<T>
where
    F: Future<Output = Result<T, ReqRespError>>,
    B: Future<Output = Result<T, ReqRespError>>,
    M: FnOnce() -> B,
{
    let started = Instant::now();
    let mut primary = pin!(primary);
    let warning = sleep(stall_threshold);
    let mut warning = pin!(warning);
    let recovery_deadline = async {
        match recovery_timeout {
            Some(duration) => sleep(duration).await,
            None => std::future::pending().await,
        }
    };
    let mut recovery_deadline = pin!(recovery_deadline);

    select! {
        result = &mut primary => {
            return EndpointWaitOutcome::Completed(EndpointWaitResult {
                result,
                stalled_for: None,
                backup_sent: false,
                discarded_errors: 0,
            });
        }
        _ = &mut warning => {}
        _ = &mut recovery_deadline => {
            return EndpointWaitOutcome::RecoveryRequired {
                stalled_for: started.elapsed(),
            };
        }
        _ = cancel.cancelled() => return EndpointWaitOutcome::Cancelled,
    }

    warn!(
        "{direction} response still pending after {}ms; sending one backup for transaction {:?}",
        started.elapsed().as_millis(),
        transaction,
    );
    warning
        .as_mut()
        .reset(Instant::now() + ENDPOINT_STALL_LOG_INTERVAL);
    let mut backup = pin!(make_backup());

    enum ResponseSource<T> {
        Primary(Result<T, ReqRespError>),
        Backup(Result<T, ReqRespError>),
    }

    let first = loop {
        select! {
            result = &mut primary => break ResponseSource::Primary(result),
            result = &mut backup => break ResponseSource::Backup(result),
            _ = &mut recovery_deadline => {
                return EndpointWaitOutcome::RecoveryRequired {
                    stalled_for: started.elapsed(),
                };
            }
            _ = &mut warning => {
                warn!(
                    "{direction} response still pending after {}ms for transaction {:?}",
                    started.elapsed().as_millis(),
                    transaction,
                );
                warning.as_mut().reset(Instant::now() + ENDPOINT_STALL_LOG_INTERVAL);
            }
            _ = cancel.cancelled() => return EndpointWaitOutcome::Cancelled,
        }
    };

    let result = match first {
        ResponseSource::Primary(Ok(response)) | ResponseSource::Backup(Ok(response)) => {
            return EndpointWaitOutcome::Completed(EndpointWaitResult {
                result: Ok(response),
                stalled_for: Some(started.elapsed()),
                backup_sent: true,
                discarded_errors: 0,
            });
        }
        ResponseSource::Primary(Err(err)) => {
            warn!("{direction} primary request failed while backup is pending: {err:?}");
            loop {
                select! {
                    result = &mut backup => break result,
                    _ = &mut recovery_deadline => {
                        return EndpointWaitOutcome::RecoveryRequired {
                            stalled_for: started.elapsed(),
                        };
                    }
                    _ = &mut warning => {
                        warn!(
                            "{direction} backup response still pending after {}ms for transaction {:?}",
                            started.elapsed().as_millis(),
                            transaction,
                        );
                        warning.as_mut().reset(Instant::now() + ENDPOINT_STALL_LOG_INTERVAL);
                    }
                    _ = cancel.cancelled() => return EndpointWaitOutcome::Cancelled,
                }
            }
        }
        ResponseSource::Backup(Err(err)) => {
            warn!("{direction} backup request failed while primary is pending: {err:?}");
            loop {
                select! {
                    result = &mut primary => break result,
                    _ = &mut recovery_deadline => {
                        return EndpointWaitOutcome::RecoveryRequired {
                            stalled_for: started.elapsed(),
                        };
                    }
                    _ = &mut warning => {
                        warn!(
                            "{direction} primary response still pending after {}ms for transaction {:?}",
                            started.elapsed().as_millis(),
                            transaction,
                        );
                        warning.as_mut().reset(Instant::now() + ENDPOINT_STALL_LOG_INTERVAL);
                    }
                    _ = cancel.cancelled() => return EndpointWaitOutcome::Cancelled,
                }
            }
        }
    };

    EndpointWaitOutcome::Completed(EndpointWaitResult {
        result,
        stalled_for: Some(started.elapsed()),
        backup_sent: true,
        discarded_errors: 1,
    })
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

        let result = await_endpoint_response_with_timeouts(
            async {
                sleep(Duration::from_millis(30)).await;
                Ok(42)
            },
            || async {
                sleep(Duration::from_millis(50)).await;
                Ok(43)
            },
            "test",
            transaction,
            &cancel,
            Duration::from_millis(10),
            Some(Duration::from_millis(100)),
        )
        .await;

        let EndpointWaitOutcome::Completed(result) = result else {
            panic!("request should complete");
        };
        assert_eq!(result.result, Ok(42));
        assert!(
            result
                .stalled_for
                .is_some_and(|duration| duration >= Duration::from_millis(10))
        );
        assert!(result.backup_sent);
        assert_eq!(result.discarded_errors, 0);
    }

    #[tokio::test]
    async fn backup_recovers_a_lost_primary_request() {
        let cancel = CancellationToken::new();
        let transaction = WifiTransaction { session: 7, id: 10 };

        let result = await_endpoint_response_with_timeouts(
            std::future::pending(),
            || async {
                sleep(Duration::from_millis(10)).await;
                Ok(43)
            },
            "test",
            transaction,
            &cancel,
            Duration::from_millis(10),
            Some(Duration::from_millis(100)),
        )
        .await;

        let EndpointWaitOutcome::Completed(result) = result else {
            panic!("backup should complete");
        };
        assert_eq!(result.result, Ok(43));
        assert!(result.stalled_for.is_some());
        assert!(result.backup_sent);
        assert_eq!(result.discarded_errors, 0);
    }

    #[tokio::test]
    async fn recovery_is_required_when_primary_and_backup_are_lost() {
        let cancel = CancellationToken::new();
        let transaction = WifiTransaction { session: 7, id: 11 };

        let result = await_endpoint_response_with_timeouts::<_, _, _, ()>(
            std::future::pending(),
            std::future::pending,
            "test",
            transaction,
            &cancel,
            Duration::from_millis(5),
            Some(Duration::from_millis(30)),
        )
        .await;

        let EndpointWaitOutcome::RecoveryRequired { stalled_for } = result else {
            panic!("lost primary and backup should require recovery");
        };
        assert!(stalled_for >= Duration::from_millis(30));
    }

    #[tokio::test]
    async fn disabled_recovery_waits_for_cancellation() {
        let cancel = CancellationToken::new();
        let cancel_request = cancel.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(20)).await;
            cancel_request.cancel();
        });

        let result = await_endpoint_response_with_timeouts::<_, _, _, ()>(
            std::future::pending(),
            std::future::pending,
            "test",
            WifiTransaction { session: 7, id: 12 },
            &cancel,
            Duration::from_millis(5),
            None,
        )
        .await;

        assert!(matches!(result, EndpointWaitOutcome::Cancelled));
    }
}
