#![allow(special_module_name)]

mod error;
mod job_declarator;
pub mod mining_downstream;
mod mining_upstream;
mod task_manager;
mod template_receiver;

use job_declarator::JobDeclarator;
use key_utils::Secp256k1PublicKey;
use mining_downstream::DownstreamMiningNode;
use std::sync::atomic::AtomicBool;
use task_manager::TaskManager;
use template_receiver::TemplateRx;

/// Is used by the template receiver and the downstream. When a NewTemplate is received the context
/// that is running the template receiver set this value to false and then the message is sent to
/// the context that is running the Downstream that do something and then set it back to true.
///
/// In the meantime if the context that is running the template receiver receives a SetNewPrevHash
/// it wait until the value of this global is true before doing anything.
///
/// Acuire and Release memory ordering is used.
///
/// Memory Ordering Explanation:
/// We use Acquire-Release ordering instead of SeqCst or Relaxed for the following reasons:
/// 1. Acquire in template receiver context ensures we see all operations before the Release store
///    the downstream.
/// 2. Within the same execution context (template receiver), a Relaxed store followed by an Acquire
///    load is sufficient. This is because operations within the same context execute in the order
///    they appear in the code.
/// 3. The combination of Release in downstream and Acquire in template receiver contexts establishes
///    a happens-before relationship, guaranteeing that we handle the SetNewPrevHash message after
///    that downstream have finished handling the NewTemplate.
/// 3. SeqCst is overkill we only need to synchronize two contexts, a globally agreed-upon order
///    between all the contexts is not necessary.
pub static IS_NEW_TEMPLATE_HANDLED: AtomicBool = AtomicBool::new(true);

use roles_logic_sv2::{parsers::Mining, utils::Mutex};
use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
};

use std::net::ToSocketAddrs;
use tracing::error;

use crate::shared::utils::AbortOnDrop;

pub async fn start(
    receiver: tokio::sync::mpsc::Receiver<Mining<'static>>,
    sender: tokio::sync::mpsc::Sender<Mining<'static>>,
    up_receiver: tokio::sync::mpsc::Receiver<Mining<'static>>,
    up_sender: tokio::sync::mpsc::Sender<Mining<'static>>,
) -> AbortOnDrop {
    initialize_jd(receiver, sender, up_receiver, up_sender).await
}
async fn initialize_jd(
    receiver: tokio::sync::mpsc::Receiver<Mining<'static>>,
    sender: tokio::sync::mpsc::Sender<Mining<'static>>,
    up_receiver: tokio::sync::mpsc::Receiver<Mining<'static>>,
    up_sender: tokio::sync::mpsc::Sender<Mining<'static>>,
) -> AbortOnDrop {
    let task_manager = TaskManager::initialize();
    let abortable = task_manager
        .safe_lock(|t| t.get_aborter())
        .unwrap()
        .unwrap();
    let test_only_do_not_send_solution_to_tp = false;

    // When Downstream receive a share that meets bitcoin target it transformit in a
    // SubmitSolution and send it to the TemplateReceiver
    let (send_solution, recv_solution) = tokio::sync::mpsc::channel(10);

    // Instantiate a new `Upstream` (SV2 Pool)
    let upstream = match mining_upstream::Upstream::new(
        0, // TODO
        crate::POOL_SIGNATURE.to_string(),
        up_sender,
    )
    .await
    {
        Ok(upstream) => upstream,
        Err(e) => {
            error!("Failed to create upstream: {}", e);
            panic!()
        }
    };

    // Initialize JD part
    let tp_address = crate::TP_ADDRESS
        .as_ref()
        .expect("Unreachable code, jdc is not instantiated when TP_ADDRESS not present");
    let mut parts = tp_address.split(':');
    let ip_tp = parts.next().unwrap().to_string();
    let port_tp = parts.next().unwrap().parse::<u16>().unwrap();

    let auth_pub_k: Secp256k1PublicKey = crate::AUTH_PUB_KEY.parse().expect("Invalid public key");
    let (jd, jd_abortable) = match JobDeclarator::new(
        crate::POOL_ADDRESS
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap(),
        auth_pub_k.into_bytes(),
        upstream.clone(),
    )
    .await
    {
        Ok(c) => c,
        Err(_e) => {
            // TODO TODO TODO
            todo!()
        }
    };

    TaskManager::add_job_declarator_task(task_manager.clone(), jd_abortable)
        .await
        .unwrap();

    let donwstream = Arc::new(Mutex::new(DownstreamMiningNode::new(
        sender,
        Some(upstream.clone()),
        send_solution,
        false,
        vec![],
        Some(jd.clone()),
    )));
    let downstream_abortable = DownstreamMiningNode::start(donwstream.clone(), receiver).await;
    TaskManager::add_mining_downtream_task(task_manager.clone(), downstream_abortable)
        .await
        .unwrap();
    upstream
        .safe_lock(|u| u.downstream = Some(donwstream.clone()))
        .expect("Poison lock");

    // Start receiving messages from the SV2 Upstream role
    let upstream_abortable =
        match mining_upstream::Upstream::parse_incoming(upstream.clone(), up_receiver).await {
            Ok(abortable) => abortable,
            Err(e) => {
                error!("failed to create sv2 parser: {}", e);
                panic!()
            }
        };
    TaskManager::add_mining_upstream_task(task_manager.clone(), upstream_abortable)
        .await
        .unwrap();

    let tp_abortable = TemplateRx::connect(
        SocketAddr::new(IpAddr::from_str(ip_tp.as_str()).unwrap(), port_tp),
        recv_solution,
        Some(jd.clone()),
        donwstream,
        vec![],
        None,
        test_only_do_not_send_solution_to_tp,
    )
    .await;
    TaskManager::add_template_receiver_task(task_manager, tp_abortable)
        .await
        .unwrap();
    abortable
}
