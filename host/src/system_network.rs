use log::{info, warn};
use std::{
    io,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, select, time::sleep};
use tokio_util::sync::CancellationToken;

const TAP_INTERFACE: &str = "esp32tap";

pub async fn run(route_metric: u32, cancel: CancellationToken) -> io::Result<()> {
    wait_for_tap(&cancel).await?;
    if cancel.is_cancelled() {
        return Ok(());
    }

    command_output("nmcli", &["general", "status"])
        .await
        .map_err(|err| io::Error::other(format!("NetworkManager is required: {err}")))?;

    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let profile = format!("lamella-esp32tap-{}-{started}", std::process::id());
    let metric = route_metric.to_string();
    let add_args = [
        "connection",
        "add",
        "type",
        "tun",
        "ifname",
        TAP_INTERFACE,
        "con-name",
        profile.as_str(),
        "mode",
        "tap",
        "connection.autoconnect",
        "no",
        "ipv4.method",
        "auto",
        "ipv4.route-metric",
        metric.as_str(),
        "ipv4.never-default",
        "no",
        "ipv6.method",
        "disabled",
    ];

    command_output("nmcli", &add_args).await?;
    info!("Created temporary NetworkManager profile {profile}");

    let result = configure_activate_and_monitor(&profile, &cancel).await;
    if let Err(err) = command_output("nmcli", &["connection", "delete", "id", &profile]).await {
        warn!("Failed to remove NetworkManager profile {profile}: {err}");
    } else {
        info!("Removed temporary NetworkManager profile {profile}");
    }

    result
}

async fn wait_for_tap(cancel: &CancellationToken) -> io::Result<()> {
    info!("System network mode enabled; waiting for {TAP_INTERFACE}");
    loop {
        select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = sleep(Duration::from_millis(100)) => {
                let status = Command::new("ip")
                    .args(["link", "show", "dev", TAP_INTERFACE])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .map_err(|err| io::Error::other(format!("Failed to run ip: {err}")))?;
                if status.success() {
                    return Ok(());
                }
            }
        }
    }
}

async fn configure_activate_and_monitor(
    profile: &str,
    cancel: &CancellationToken,
) -> io::Result<()> {
    // NetworkManager otherwise assigns a new random MAC while adopting an
    // existing TAP. Preserve the ESP32 WiFi MAC installed by the bridge.
    command_output(
        "nmcli",
        &[
            "connection",
            "modify",
            "id",
            profile,
            "ethernet.cloned-mac-address",
            "preserve",
        ],
    )
    .await?;

    let activation_args = [
        "--wait",
        "45",
        "connection",
        "up",
        "id",
        profile,
        "ifname",
        TAP_INTERFACE,
    ];
    let activation = command_output("nmcli", &activation_args);

    select! {
        result = activation => {
            result?;
        }
        _ = cancel.cancelled() => return Ok(()),
    }

    let details = command_output(
        "nmcli",
        &[
            "--fields",
            "GENERAL.STATE,GENERAL.HWADDR,IP4.ADDRESS,IP4.GATEWAY,IP4.DNS",
            "device",
            "show",
            TAP_INTERFACE,
        ],
    )
    .await?;
    info!("Lamella system network is ready on {TAP_INTERFACE}:\n{details}");

    match command_output("ip", &["-4", "route", "get", "1.1.1.1"]).await {
        Ok(route) if route.contains(&format!("dev {TAP_INTERFACE}")) => {
            info!("IPv4 internet route uses {TAP_INTERFACE}: {route}");
        }
        Ok(route) => {
            info!("IPv4 internet route is policy-routed or uses another interface: {route}");
            report_wireguard_routes().await;
        }
        Err(err) => warn!("Failed to inspect the IPv4 internet route: {err}"),
    }

    cancel.cancelled().await;
    Ok(())
}

async fn report_wireguard_routes() {
    let Ok(interfaces) = command_output("wg", &["show", "interfaces"]).await else {
        return;
    };

    for interface in interfaces.split_whitespace() {
        let Ok(endpoints) = command_output("wg", &["show", interface, "endpoints"]).await else {
            continue;
        };
        let fwmark = command_output("wg", &["show", interface, "fwmark"])
            .await
            .unwrap_or_else(|_| "off".to_owned());
        for endpoint in endpoints
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
        {
            let host = endpoint_host(endpoint);
            let family = if host.contains(':') { "-6" } else { "-4" };
            let mut args = vec![family, "route", "get", host];
            let mark;
            if fwmark != "off" {
                mark = fwmark.clone();
                args.extend(["mark", mark.as_str()]);
            }
            match command_output("ip", &args).await {
                Ok(route) => info!("WireGuard {interface} endpoint route: {route}"),
                Err(err) => warn!("Failed to inspect WireGuard {interface} endpoint: {err}"),
            }
        }
    }
}

fn endpoint_host(endpoint: &str) -> &str {
    if let Some(rest) = endpoint.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        &rest[..end]
    } else {
        endpoint.rsplit_once(':').map_or(endpoint, |(host, _)| host)
    }
}

async fn command_output(program: &str, args: &[&str]) -> io::Result<String> {
    let output = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|err| io::Error::other(format!("Failed to run {program}: {err}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(io::Error::other(format!(
            "{program} {} failed: {stderr}",
            args.join(" ")
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::endpoint_host;

    #[test]
    fn extracts_endpoint_hosts() {
        assert_eq!(endpoint_host("192.0.2.1:51820"), "192.0.2.1");
        assert_eq!(endpoint_host("[2001:db8::1]:51820"), "2001:db8::1");
    }
}
