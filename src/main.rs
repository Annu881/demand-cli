use jemallocator::Jemalloc;
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

use crate::shared::utils::AbortOnDrop;
use key_utils::Secp256k1PublicKey;
use lazy_static::lazy_static;
use std::net::ToSocketAddrs;
use tokio::sync::mpsc::channel;
use tracing::{error, info};

mod ingress;
pub mod jd_client;
mod minin_pool_connection;
mod router;
mod share_accounter;
mod shared;
mod translator;

const TRANSLATOR_BUFFER_SIZE: usize = 32;
const MIN_EXTRANONCE_SIZE: u16 = 6;
const MIN_EXTRANONCE2_SIZE: u16 = 5;
const UPSTREAM_EXTRANONCE1_SIZE: usize = 15;
const EXPECTED_SV1_HASHPOWER: f32 = 100_000_000_000.0;
//const EXPECTED_SV1_HASHPOWER: f32 = 1_000_000.0;
const SHARE_PER_MIN: f32 = 10.0;
const CHANNEL_DIFF_UPDTATE_INTERVAL: u32 = 10;
const MIN_SV1_DOWSNTREAM_HASHRATE: f32 = 1_000_000_000_000.0;
//const MIN_SV1_DOWSNTREAM_HASHRATE: f32 = 1_000_000.0;
const MAX_LEN_DOWN_MSG: u32 = 10000;
const POOL_ADDRESS: &str = "mining.dmnd.work:2000";
//const POOL_ADDRESS: &str = "0.0.0.0:20000";
const AUTH_PUB_KEY: &str = "9bQHWXsQ2J9TRFTaxRh3KjoxdyLRfWVEy25YHtKF8y8gotLoCZZ";
//const AUTH_PUB_KEY: &str = "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72";
//const TP_ADDRESS: &str = "127.0.0.1:8442";
const DEFAULT_LISTEN_ADDRESS: &str = "0.0.0.0:32767";

lazy_static! {
    static ref SV1_DOWN_LISTEN_ADDR: String =
        std::env::var("SV1_DOWN_LISTEN_ADDR").unwrap_or(DEFAULT_LISTEN_ADDRESS.to_string());
}
lazy_static! {
    static ref TP_ADDRESS: Option<String> = std::env::var("TP_ADDRESS").ok();
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    std::env::var("TOKEN").expect("Missing TOKEN environment variable");
    let auth_pub_k: Secp256k1PublicKey = crate::AUTH_PUB_KEY.parse().expect("Invalid public key");
    let address = POOL_ADDRESS
        .to_socket_addrs()
        .expect("Invalid pool address")
        .next()
        .expect("Invalid pool address");

    // We will add upstream addresses here
    let pool_addresses = vec![address];

    let mut router = router::Router::new(pool_addresses, auth_pub_k, None, None);
    let (send_to_pool, recv_from_pool, pool_connection_abortable) = router
        .connect_pool(None)
        .await
        .expect("Error connecting pool");

    // Monitor and switch upstream when better one becomes available
    tokio::spawn(async move {
        router.monitor_upstream().await;
    });

    let (downs_sv1_tx, downs_sv1_rx) = channel(10);
    let sv1_ingress_abortable = ingress::sv1_ingress::start_listen_for_downstream(downs_sv1_tx);

    let (translator_up_tx, mut translator_up_rx) = channel(10);
    let translator_abortable = translator::start(downs_sv1_rx, translator_up_tx)
        .await
        .expect("Impossible initialize translator");

    let (from_jdc_to_share_accounter_send, from_jdc_to_share_accounter_recv) = channel(10);
    let (from_share_accounter_to_jdc_send, from_share_accounter_to_jdc_recv) = channel(10);
    let (jdc_to_translator_sender, jdc_from_translator_receiver, _) = translator_up_rx
        .recv()
        .await
        .expect("translator failed before initialization");
    let jdc_abortable: Option<AbortOnDrop>;
    let share_accounter_abortable;
    if let Some(_tp_addr) = TP_ADDRESS.as_ref() {
        jdc_abortable = Some(
            jd_client::start(
                jdc_from_translator_receiver,
                jdc_to_translator_sender,
                from_share_accounter_to_jdc_recv,
                from_jdc_to_share_accounter_send,
            )
            .await,
        );
        share_accounter_abortable = share_accounter::start(
            from_jdc_to_share_accounter_recv,
            from_share_accounter_to_jdc_send,
            recv_from_pool,
            send_to_pool,
        )
        .await;
    } else {
        jdc_abortable = None;

        share_accounter_abortable = share_accounter::start(
            jdc_from_translator_receiver,
            jdc_to_translator_sender,
            recv_from_pool,
            send_to_pool,
        )
        .await;
    };

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        if pool_connection_abortable.is_finished() {
            error!("Upstream mining connnection closed");
            break;
        }
        if sv1_ingress_abortable.is_finished() {
            error!("Downtream mining socket unavailable");
            break;
        }
        if translator_abortable.is_finished() {
            error!("Translator error");
            break;
        }
        if let Some(ref jdc) = jdc_abortable {
            if jdc.is_finished() {
                error!("Jdc error");
                break;
            }
        }
        if share_accounter_abortable.is_finished() {
            error!("Share accounter error");
            break;
        }
    }
    info!("exiting");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
}
