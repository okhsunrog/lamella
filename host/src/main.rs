use ergot::{
    toolkits::nusb_v0_1::{find_new_devices, register_router_interface, RouterStack},
    well_known::ErgotPingEndpoint,
    Address,
};
use icd::{LedEndpoint, PingTopic};
use log::{info, warn};
use std::{
    collections::HashSet,
    io,
    pin::pin,
    time::{Duration, Instant},
};
use tokio::time::{interval, sleep, timeout};

const MTU: u16 = 1024;
const OUT_BUFFER_SIZE: usize = 4096;

#[tokio::main]
async fn main() -> io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let stack: RouterStack = RouterStack::new();

    tokio::task::spawn(ping_all(stack.clone()));
    tokio::task::spawn(ping_listener(stack.clone()));
    tokio::task::spawn(led_controller(stack.clone()));

    let mut seen = HashSet::new();

    loop {
        let devices = find_new_devices(&seen).await;

        for dev in devices {
            let info = dev.info.clone();
            info!("Found {:?}, registering", info);
            let _hdl = register_router_interface(&stack, dev, MTU, OUT_BUFFER_SIZE)
                .await
                .unwrap();
            seen.insert(info);
        }

        sleep(Duration::from_secs(3)).await;
    }
}

async fn ping_all(stack: RouterStack) {
    let mut ival = interval(Duration::from_secs(3));
    let mut ctr = 0u32;

    loop {
        ival.tick().await;
        let nets = stack.manage_profile(|im| im.get_nets());
        info!("Nets to ping: {:?}", nets);

        for net in nets {
            let pg = ctr;
            ctr = ctr.wrapping_add(1);

            let addr = Address {
                network_id: net,
                node_id: 2,
                port_id: 0,
            };

            let start = Instant::now();
            let rr = stack
                .endpoints()
                .request_full::<ErgotPingEndpoint>(addr, &pg, None);
            let fut = timeout(Duration::from_millis(100), rr);
            let res = fut.await;
            let elapsed = start.elapsed();
            match &res {
                Ok(Ok(_)) => info!("ping {}.2: {:?}", net, elapsed),
                _ => warn!("ping {}.2 failed: {:?}", net, res),
            }
        }
    }
}

async fn ping_listener(stack: RouterStack) {
    let subber = stack.topics().heap_bounded_receiver::<PingTopic>(64, None);
    let subber = pin!(subber);
    let mut hdl = subber.subscribe();

    loop {
        let msg = hdl.recv().await;
        info!("Received ping broadcast: {:?}", msg);
    }
}

async fn led_controller(stack: RouterStack) {
    sleep(Duration::from_secs(5)).await;
    let mut ival = interval(Duration::from_secs(2));
    let mut on = false;

    loop {
        ival.tick().await;

        // Only send if we have connected networks
        let nets = stack.manage_profile(|im| im.get_nets());
        if nets.is_empty() {
            continue;
        }

        on = !on;

        // Send to all discovered networks
        for net in nets {
            let addr = Address {
                network_id: net,
                node_id: 2,
                port_id: 0,
            };

            let result = timeout(
                Duration::from_millis(100),
                stack
                    .endpoints()
                    .request::<LedEndpoint>(addr, &on, Some("led")),
            )
            .await;

            match result {
                Ok(Ok(_)) => info!("LED on net {} set to {}", net, on),
                Ok(Err(e)) => warn!("LED request error on net {}: {:?}", net, e),
                Err(_) => warn!("LED request timeout on net {}", net),
            }
        }
    }
}
