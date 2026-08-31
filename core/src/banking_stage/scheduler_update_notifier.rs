use {
    super::{
        DecisionState, PostPackConfirmation, PostPackConfirmationConfig,
        PostPackConfirmationConfigStatus,
        postpack_confirmation_config::load_postpack_confirmation_config,
        reward_distributor::LatestBankPair,
    },
    crate::proxy::auth::AuthInterceptor,
    arc_swap::ArcSwap,
    crossbeam_channel::{Receiver, Sender, unbounded},
    jito_protos::proto::{
        auth::{Token, auth_service_client::AuthServiceClient},
        block_engine::{
            ExpiringPacketBatch, PacketBatchUpdate,
            block_engine_relayer_client::BlockEngineRelayerClient, packet_batch_update::Msg,
        },
        packet::{Meta as ProtoMeta, Packet as ProtoPacket, PacketBatch as ProtoPacketBatch},
        shared::{Header, Heartbeat},
    },
    log::{error, warn},
    prost_types::Timestamp,
    serde::{Deserialize, Serialize},
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_message::v0::LoadedAddresses,
    solana_perf::packet::BytesPacket,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
    std::{
        collections::{HashMap, HashSet},
        sync::{
            Arc, Mutex, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant, SystemTime},
    },
    tokio::{
        runtime::Handle,
        sync::mpsc,
        time::{self, interval},
    },
    tokio_stream::wrappers::ReceiverStream,
    tonic::{
        codegen::InterceptedService,
        transport::{Channel, Endpoint},
    },
};

#[cfg(feature = "build_validator")]
unsafe extern "C" {
    #[allow(improper_ctypes)]
    #[allow(improper_ctypes_definitions)]
    pub fn clear_postpack_conf_signatures();
}

fn tin_connection_state_log(url: String, uuid: String, primary: bool, state: String) {
    #[cfg(feature = "build_validator")]
    {
        if unsafe { super::rakurai_enabled() } {
            let name: &'static str = "rakurai_tin_connection_state";
            let datapoint = solana_metrics::create_datapoint!(
                @point name,
                ("url", url, String),
                ("uuid", uuid, String),
                ("source", "p2c", String),
                ("primary", primary, bool),
                ("state", state, String),
            );
            solana_metrics::submit(datapoint, log::Level::Info);
            return;
        }
    }
    let _ = (url, uuid, primary, state);
}

fn publish_tin_log_epoch(tin_log_epochs: &Mutex<HashMap<String, Arc<AtomicU64>>>, epoch: u64) {
    for shared_epoch in tin_log_epochs.lock().unwrap().values() {
        shared_epoch.store(epoch, Ordering::Relaxed);
    }
}

fn maybe_log_tin_connection_for_epoch(
    shared_epoch: &AtomicU64,
    local_epoch: &mut u64,
    url: &str,
    uuid: &str,
) {
    let epoch = shared_epoch.load(Ordering::Relaxed);
    if epoch != 0 && epoch != *local_epoch {
        *local_epoch = epoch;
        tin_connection_state_log(
            url.to_string(),
            uuid.to_string(),
            false,
            "connected".to_string(),
        );
    }
}

const GRPC_QUEUE_CAPACITY: usize = 1_000;
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(5_000);
const AUTH_REFRESH_INTERVAL: Duration = Duration::from_secs(300);
const AUTH_CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_REFRESH_WITHIN_S: u64 = 5 * 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerUpdate {
    pub txn: VersionedTransaction,
    pub loaded_addresses: LoadedAddresses,
    pub packet: BytesPacket,
}
/// Transactions scheduled for consume; converted to gRPC updates in the notifier thread.
pub struct SchedulerUpdateWork<Tx> {
    pub transactions: Vec<Tx>,
    pub packet_batches: Vec<BytesPacket>,
    /// Final strings for gRPC `Meta.addr`, parallel to `packet_batches`.
    pub hash_addrs: Vec<String>,
}

pub type SerializedSchedulerUpdate = (BytesPacket, String);

/// P2C update channel sender paired with the forward gate set by reward distributor.
#[derive(Clone)]
pub struct P2cUpdateSender {
    pub sender: Sender<SerializedSchedulerUpdate>,
    pub forward_to_p2c: Arc<AtomicBool>,
}

impl P2cUpdateSender {
    pub fn unbounded() -> (Self, Receiver<SerializedSchedulerUpdate>) {
        let forward_to_p2c = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = unbounded();
        (
            Self {
                sender,
                forward_to_p2c,
            },
            receiver,
        )
    }
}

pub struct SchedulerUpdateNotifier<Tx> {
    shared_decision: (Arc<RwLock<DecisionState>>, Arc<AtomicBool>),
    shared_bank_update: Arc<RwLock<LatestBankPair>>,
    cluster_info: Option<Arc<ClusterInfo>>,
    vote_account: Pubkey,
    postpack_confirmation_config: Arc<RwLock<PostPackConfirmationConfig>>,
    postpack_confirmation_active_entries: Arc<ArcSwap<PostPackConfirmationConfigStatus>>,
    post_pack_confirmation_uuid_blocklist: Arc<ArcSwap<Vec<String>>>,
    receiver: Receiver<SchedulerUpdateWork<Tx>>,
    p2c_update_receiver: Option<Receiver<SerializedSchedulerUpdate>>,
    exit: Arc<AtomicBool>,
}

impl<Tx> SchedulerUpdateNotifier<Tx>
where
    Tx: TransactionWithMeta + Send + 'static,
{
    pub fn new(
        shared_decision: (Arc<RwLock<DecisionState>>, Arc<AtomicBool>),
        shared_bank_update: Arc<RwLock<LatestBankPair>>,
        cluster_info: Option<Arc<ClusterInfo>>,
        vote_account: Pubkey,
        postpack_confirmation_config: Arc<RwLock<PostPackConfirmationConfig>>,
        postpack_confirmation_active_entries: Arc<ArcSwap<PostPackConfirmationConfigStatus>>,
        post_pack_confirmation_uuid_blocklist: Arc<ArcSwap<Vec<String>>>,
        receiver: Receiver<SchedulerUpdateWork<Tx>>,
        p2c_update_receiver: Option<Receiver<SerializedSchedulerUpdate>>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        Self {
            shared_decision,
            shared_bank_update,
            cluster_info,
            vote_account,
            postpack_confirmation_config,
            postpack_confirmation_active_entries,
            post_pack_confirmation_uuid_blocklist,
            receiver,
            p2c_update_receiver,
            exit,
        }
    }

    pub fn run(&mut self) {
        // Background tokio workers run gRPC connections while this thread blocks on recv.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .thread_name("solSchedTokio")
            .enable_all()
            .build()
            .expect("failed to create tokio runtime for SchedulerUpdateNotifier");
        let handle = rt.handle().clone();

        let Some(cluster_info) = self.cluster_info.clone() else {
            warn!(
                "SchedulerUpdateNotifier: cluster_info unavailable; postpack-confirmation gRPC disabled"
            );
            return;
        };

        let mut current_entries: Vec<PostPackConfirmation> = Vec::new();
        let mut connection_tasks = Vec::new();
        let task_exits = Arc::new(Mutex::new(HashMap::<String, Arc<AtomicBool>>::new()));
        let tin_log_epochs = Arc::new(Mutex::new(HashMap::<String, Arc<AtomicU64>>::new()));
        let update_senders = Arc::new(Mutex::new(HashMap::<String, EndpointSender>::new()));
        let mut last_published_tin_log_epoch = super::current_tin_connection_log_epoch();

        sync_postpack_confirmation_config(
            &handle,
            &self.shared_bank_update,
            &self.vote_account,
            &self.postpack_confirmation_config,
            &self.postpack_confirmation_active_entries,
            &self.post_pack_confirmation_uuid_blocklist,
            &cluster_info,
            &self.exit,
            &task_exits,
            &tin_log_epochs,
            &update_senders,
            &mut connection_tasks,
            &mut current_entries,
        );

        let keypair = cluster_info.keypair();
        let mut last_postpack_sync = Instant::now();
        const POSTPACK_SYNC_INTERVAL: Duration = Duration::from_secs(30);

        while !self.exit.load(Ordering::Relaxed) {
            if let Some(p2c_update_receiver) = &self.p2c_update_receiver {
                while let Ok(update) = p2c_update_receiver.try_recv() {
                    let senders = update_senders.lock().unwrap().clone();
                    dispatch_serialized_scheduler_update(keypair.clone(), &senders, update);
                }
            }

            while let Ok(work) = self.receiver.try_recv() {
                let senders = update_senders.lock().unwrap().clone();
                dispatch_scheduler_update_work(&senders, work);
            }

            if last_postpack_sync.elapsed() >= POSTPACK_SYNC_INTERVAL {
                let tin_log_epoch = crate::banking_stage::current_tin_connection_log_epoch();
                if tin_log_epoch != 0 && tin_log_epoch != last_published_tin_log_epoch {
                    last_published_tin_log_epoch = tin_log_epoch;
                    publish_tin_log_epoch(&tin_log_epochs, tin_log_epoch);
                }
                last_postpack_sync = Instant::now();
                match self.shared_decision.0.read().unwrap().clone() {
                    DecisionState::Consume(_) => {}
                    _ => {
                        sync_postpack_confirmation_config(
                            &handle,
                            &self.shared_bank_update,
                            &self.vote_account,
                            &self.postpack_confirmation_config,
                            &self.postpack_confirmation_active_entries,
                            &self.post_pack_confirmation_uuid_blocklist,
                            &cluster_info,
                            &self.exit,
                            &task_exits,
                            &tin_log_epochs,
                            &update_senders,
                            &mut connection_tasks,
                            &mut current_entries,
                        );
                        #[cfg(feature = "build_validator")]
                        unsafe {
                            clear_postpack_conf_signatures();
                        }
                        connection_tasks.retain(|task| !task.is_finished());
                    }
                }
            }
        }

        for task_exit in task_exits.lock().unwrap().drain().map(|(_, v)| v) {
            task_exit.store(true, Ordering::Relaxed);
        }
        rt.block_on(async {
            for task in connection_tasks {
                let _ = task.await;
            }
        });
        log::info!("SchedulerUpdateNotifier: exiting");
    }
}

#[derive(Clone)]
struct EndpointSender {
    sender: mpsc::Sender<SerializedSchedulerUpdate>,
    url: String,
}

fn sync_postpack_confirmation_config(
    handle: &Handle,
    shared_bank_update: &Arc<RwLock<LatestBankPair>>,
    vote_account: &Pubkey,
    postpack_confirmation_config: &Arc<RwLock<PostPackConfirmationConfig>>,
    postpack_confirmation_active_entries: &Arc<ArcSwap<PostPackConfirmationConfigStatus>>,
    post_pack_confirmation_uuid_blocklist: &Arc<ArcSwap<Vec<String>>>,
    cluster_info: &Arc<ClusterInfo>,
    exit: &Arc<AtomicBool>,
    task_exits: &Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    tin_log_epochs: &Arc<Mutex<HashMap<String, Arc<AtomicU64>>>>,
    update_senders: &Arc<Mutex<HashMap<String, EndpointSender>>>,
    connection_tasks: &mut Vec<tokio::task::JoinHandle<()>>,
    current_entries: &mut Vec<PostPackConfirmation>,
) {
    let pda_entries = shared_bank_update
        .read()
        .ok()
        .and_then(|bank_pair| {
            load_postpack_confirmation_config(bank_pair.working_bank.as_ref(), vote_account)
        })
        .map(|config| config.entries)
        .unwrap_or_default();

    let admin_entries = postpack_confirmation_config.read().unwrap().entries.clone();
    let blocklisted_uuids = post_pack_confirmation_uuid_blocklist
        .load()
        .as_ref()
        .clone();
    let merged_entries = union_postpack_confirmation_entries(&pda_entries, &admin_entries);
    let blocklisted_entries = blocklisted_entries_from_merged(&merged_entries, &blocklisted_uuids);
    let active_entries = filter_blocklisted_uuids(&merged_entries, &blocklisted_uuids);
    let blocklist: HashSet<String> = blocklisted_uuids.iter().cloned().collect();

    postpack_confirmation_active_entries.store(Arc::new(PostPackConfirmationConfigStatus {
        admin_entries: admin_entries.clone(),
        onchain_entries: pda_entries.clone(),
        blocklisted_uuids,
        blocklisted_entries,
        active_entries: active_entries.clone(),
    }));

    let mut uuids_to_remove: HashSet<String> = current_entries
        .iter()
        .filter(|entry| !active_entries.contains(entry))
        .map(|entry| entry.uuid.clone())
        .collect();

    {
        let senders = update_senders.lock().unwrap();
        for (uuid, endpoint) in senders.iter() {
            if blocklist.contains(uuid) {
                uuids_to_remove.insert(uuid.clone());
                continue;
            }
            let Some(active) = active_entries.iter().find(|entry| entry.uuid == *uuid) else {
                uuids_to_remove.insert(uuid.clone());
                continue;
            };
            if active.url != endpoint.url {
                uuids_to_remove.insert(uuid.clone());
            }
        }
    }

    let entries_to_add: Vec<PostPackConfirmation> = {
        let senders = update_senders.lock().unwrap();
        active_entries
            .iter()
            .filter(|entry| !senders.contains_key(&entry.uuid))
            .cloned()
            .collect()
    };

    if uuids_to_remove.is_empty() && entries_to_add.is_empty() {
        *current_entries = active_entries;
        return;
    }

    for uuid in uuids_to_remove {
        let _connected_url = update_senders
            .lock()
            .unwrap()
            .get(&uuid)
            .map(|endpoint| endpoint.url.clone());
        if let Some(task_exit) = task_exits.lock().unwrap().remove(&uuid) {
            task_exit.store(true, Ordering::Relaxed);
        }
        tin_log_epochs.lock().unwrap().remove(&uuid);
        update_senders.lock().unwrap().remove(&uuid);
    }

    for entry in entries_to_add {
        let uuid = entry.uuid.clone();
        let url = entry.url.clone();
        let (update_sender, update_receiver) = mpsc::channel(GRPC_QUEUE_CAPACITY);
        let task_exit = Arc::new(AtomicBool::new(false));
        let shared_tin_log_epoch =
            Arc::new(AtomicU64::new(super::current_tin_connection_log_epoch()));

        task_exits
            .lock()
            .unwrap()
            .insert(uuid.clone(), task_exit.clone());
        tin_log_epochs
            .lock()
            .unwrap()
            .insert(uuid.clone(), shared_tin_log_epoch.clone());
        update_senders.lock().unwrap().insert(
            uuid.clone(),
            EndpointSender {
                sender: update_sender,
                url: url.clone(),
            },
        );

        let cluster_info = cluster_info.clone();
        let exit = exit.clone();
        connection_tasks.push(handle.spawn(async move {
            run_postpack_confirmation_connection(
                url,
                uuid,
                cluster_info,
                update_receiver,
                task_exit,
                shared_tin_log_epoch,
                exit,
            )
            .await;
        }));
    }

    *current_entries = active_entries;
}

fn dispatch_scheduler_update_work<Tx>(
    senders: &HashMap<String, EndpointSender>,
    work: SchedulerUpdateWork<Tx>,
) where
    Tx: TransactionWithMeta,
{
    if senders.is_empty() {
        return;
    }

    for (packet, hash_addr) in work.packet_batches.into_iter().zip(work.hash_addrs) {
        for (_uuid, endpoint) in senders {
            if let Err(error) = endpoint
                .sender
                .try_send((packet.clone(), hash_addr.clone()))
            {
                warn!(
                    "SchedulerUpdateNotifier: failed to enqueue update for {}: {error}",
                    endpoint.url
                );
            }
        }
    }
}

fn dispatch_serialized_scheduler_update(
    keypair: Arc<Keypair>,
    senders: &HashMap<String, EndpointSender>,
    update: SerializedSchedulerUpdate,
) {
    if senders.is_empty() {
        return;
    }

    // Sign the txn-sig string; forward packet bytes with the signed proof as Meta.addr.
    let signed_sig = keypair.sign_message(update.1.as_bytes());
    let update = (update.0, signed_sig.to_string());
    for (_uuid, endpoint) in senders {
        if let Err(error) = endpoint.sender.try_send(update.clone()) {
            warn!(
                "SchedulerUpdateNotifier: failed to enqueue update for {}: {error}",
                endpoint.url
            );
        }
    }
}

#[allow(dead_code)]
fn scheduler_updates_from_work<Tx>(work: SchedulerUpdateWork<Tx>) -> Vec<SchedulerUpdate>
where
    Tx: TransactionWithMeta,
{
    work.transactions
        .into_iter()
        .zip(work.packet_batches.into_iter())
        .map(|(tx, packet)| {
            let sanitized = tx.as_sanitized_transaction();
            SchedulerUpdate {
                txn: sanitized.to_versioned_transaction(),
                loaded_addresses: sanitized.get_loaded_addresses(),
                packet: packet,
            }
        })
        .collect()
}

async fn run_postpack_confirmation_connection(
    url: String,
    uuid: String,
    cluster_info: Arc<ClusterInfo>,
    mut update_receiver: mpsc::Receiver<SerializedSchedulerUpdate>,
    task_exit: Arc<AtomicBool>,
    shared_tin_log_epoch: Arc<AtomicU64>,
    exit: Arc<AtomicBool>,
) {
    let mut logged_connect_failure = false;
    let mut local_tin_log_epoch = shared_tin_log_epoch.load(Ordering::Relaxed);
    while !exit.load(Ordering::Relaxed) && !task_exit.load(Ordering::Relaxed) {
        match auth_and_connect(
            &url,
            &uuid,
            &cluster_info,
            &mut update_receiver,
            &task_exit,
            &exit,
            &shared_tin_log_epoch,
            &mut local_tin_log_epoch,
        )
        .await
        {
            Ok(()) => {
                logged_connect_failure = false;
            }
            Err(error) => {
                error!("SchedulerUpdateNotifier: connection failed url={url} uuid={uuid}: {error}");
                if !logged_connect_failure {
                    tin_connection_state_log(
                        url.clone(),
                        uuid.clone(),
                        false,
                        format!("disconnected:error={error}"),
                    );
                    logged_connect_failure = true;
                }
                time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

#[derive(Debug)]
enum NotifierError {
    Auth(String),
    Connect(String),
    Stream(String),
}

impl std::fmt::Display for NotifierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotifierError::Auth(msg) => write!(f, "auth error: {msg}"),
            NotifierError::Connect(msg) => write!(f, "connect error: {msg}"),
            NotifierError::Stream(msg) => write!(f, "stream error: {msg}"),
        }
    }
}

async fn auth_and_connect(
    url: &str,
    uuid: &str,
    cluster_info: &Arc<ClusterInfo>,
    update_receiver: &mut mpsc::Receiver<SerializedSchedulerUpdate>,
    task_exit: &Arc<AtomicBool>,
    exit: &Arc<AtomicBool>,
    shared_tin_log_epoch: &Arc<AtomicU64>,
    local_tin_log_epoch: &mut u64,
) -> Result<(), NotifierError> {
    let keypair = cluster_info.keypair();
    let endpoint = endpoint_from_url(url).map_err(|error| NotifierError::Connect(error))?;

    let channel = time::timeout(AUTH_CONNECTION_TIMEOUT, endpoint.connect())
        .await
        .map_err(|_| NotifierError::Connect("connection timeout".to_string()))?
        .map_err(|error| NotifierError::Connect(error.to_string()))?;

    let mut auth_client = AuthServiceClient::new(channel.clone());
    let (access_token, refresh_token) = time::timeout(
        AUTH_CONNECTION_TIMEOUT,
        generate_auth_tokens_relayer(&mut auth_client, keypair.as_ref()),
    )
    .await
    .map_err(|_| NotifierError::Auth("auth timeout".to_string()))?
    .map_err(|error| NotifierError::Auth(error))?;

    let shared_access_token = Arc::new(arc_swap::ArcSwap::from_pointee(access_token));
    let auth_interceptor = AuthInterceptor::new(shared_access_token.clone());

    let block_engine_channel = time::timeout(AUTH_CONNECTION_TIMEOUT, endpoint.connect())
        .await
        .map_err(|_| NotifierError::Connect("block engine connection timeout".to_string()))?
        .map_err(|error| NotifierError::Connect(error.to_string()))?;

    let mut block_engine_client: BlockEngineRelayerClient<
        InterceptedService<Channel, AuthInterceptor>,
    > = BlockEngineRelayerClient::with_interceptor(block_engine_channel, auth_interceptor);

    let (grpc_sender, grpc_receiver) = mpsc::channel(GRPC_QUEUE_CAPACITY);
    let _response = block_engine_client
        .start_expiring_packet_stream(ReceiverStream::new(grpc_receiver))
        .await
        .map_err(|error: tonic::Status| NotifierError::Stream(error.to_string()))?;

    tin_connection_state_log(
        url.to_string(),
        uuid.to_string(),
        false,
        "connected".to_string(),
    );
    match handle_postpack_confirmation_stream(
        grpc_sender,
        update_receiver,
        auth_client,
        cluster_info,
        refresh_token,
        shared_access_token,
        task_exit,
        exit,
        url,
        uuid,
        shared_tin_log_epoch,
        local_tin_log_epoch,
    )
    .await
    {
        Ok(()) => {
            tin_connection_state_log(
                url.to_string(),
                uuid.to_string(),
                false,
                "disconnected".to_string(),
            );
            Ok(())
        }
        Err(error) => {
            error!(
                "SchedulerUpdateNotifier: stream ended with error url={url} uuid={uuid}: {error}"
            );
            tin_connection_state_log(
                url.to_string(),
                uuid.to_string(),
                false,
                format!("disconnected:error={error}"),
            );
            Err(error)
        }
    }
}

async fn handle_postpack_confirmation_stream(
    grpc_sender: mpsc::Sender<PacketBatchUpdate>,
    update_receiver: &mut mpsc::Receiver<SerializedSchedulerUpdate>,
    mut auth_client: AuthServiceClient<Channel>,
    cluster_info: &Arc<ClusterInfo>,
    mut refresh_token: Token,
    shared_access_token: Arc<arc_swap::ArcSwap<Token>>,
    task_exit: &Arc<AtomicBool>,
    exit: &Arc<AtomicBool>,
    url: &str,
    uuid: &str,
    shared_tin_log_epoch: &Arc<AtomicU64>,
    local_tin_log_epoch: &mut u64,
) -> Result<(), NotifierError> {
    while update_receiver.try_recv().is_ok() {}

    let mut heartbeat_interval = interval(HEARTBEAT_INTERVAL);
    let mut auth_refresh_interval = interval(AUTH_REFRESH_INTERVAL);
    let mut heartbeat_count = 0u64;

    while !exit.load(Ordering::Relaxed) && !task_exit.load(Ordering::Relaxed) {
        tokio::select! {
            _ = heartbeat_interval.tick() => {
                maybe_log_tin_connection_for_epoch(
                    shared_tin_log_epoch,
                    local_tin_log_epoch,
                    url,
                    uuid,
                );
                grpc_sender
                    .send(PacketBatchUpdate {
                        msg: Some(Msg::Heartbeat(Heartbeat { count: heartbeat_count })),
                    })
                    .await
                    .map_err(|error| NotifierError::Stream(error.to_string()))?;
                heartbeat_count = heartbeat_count.saturating_add(1);
            }

            maybe_update = update_receiver.recv() => {
                let Some(update) = maybe_update else {
                    return Err(NotifierError::Stream("update receiver disconnected".to_string()));
                };

                forward_serialized_scheduler_update(&grpc_sender, &update).await?;
            }

            _ = auth_refresh_interval.tick() => {
                match maybe_refresh_relayer_auth(
                    &mut auth_client,
                    keypair_from_cluster(cluster_info),
                    &shared_access_token,
                    &refresh_token,
                )
                .await
                {
                    Ok(Some(new_refresh_token)) => refresh_token = new_refresh_token,
                    Ok(None) => {}
                    Err(error) => {
                        warn!("SchedulerUpdateNotifier: auth refresh failed: {error}");
                        return Err(NotifierError::Auth(error));
                    }
                }
            }
        }
    }

    Ok(())
}

async fn forward_serialized_scheduler_update(
    grpc_sender: &mpsc::Sender<PacketBatchUpdate>,
    update: &SerializedSchedulerUpdate,
) -> Result<(), NotifierError> {
    let packet = ProtoPacket {
        data: update.0.buffer().to_vec().into(),
        meta: Some(ProtoMeta {
            size: update.0.buffer().len() as u64,
            addr: update.1.clone(),
            port: 0,
            flags: None,
            sender_stake: 0,
        }),
    };

    grpc_sender
        .send(PacketBatchUpdate {
            msg: Some(Msg::Batches(ExpiringPacketBatch {
                header: Some(Header {
                    ts: Some(Timestamp::from(SystemTime::now())),
                }),
                batch: Some(ProtoPacketBatch {
                    packets: vec![packet],
                }),
                expiry_ms: 0,
            })),
        })
        .await
        .map_err(|error| NotifierError::Stream(error.to_string()))
}

fn keypair_from_cluster(cluster_info: &Arc<ClusterInfo>) -> Arc<solana_keypair::Keypair> {
    cluster_info.keypair().clone()
}

async fn maybe_refresh_relayer_auth(
    auth_client: &mut AuthServiceClient<Channel>,
    keypair: Arc<solana_keypair::Keypair>,
    shared_access_token: &Arc<arc_swap::ArcSwap<Token>>,
    refresh_token: &Token,
) -> Result<Option<Token>, String> {
    use jito_protos::proto::auth::RefreshAccessTokenRequest;

    let access_token = shared_access_token.load();
    let access_token_expiration = access_token
        .expires_at_utc
        .as_ref()
        .ok_or_else(|| "missing access token expiration".to_string())?;
    let access_token_expiration =
        SystemTime::try_from(access_token_expiration.clone()).map_err(|error| error.to_string())?;
    let access_token_duration_left = access_token_expiration.duration_since(SystemTime::now());

    let refresh_token_expiration = refresh_token
        .expires_at_utc
        .as_ref()
        .ok_or_else(|| "missing refresh token expiration".to_string())?;
    let refresh_token_expiration = SystemTime::try_from(refresh_token_expiration.clone())
        .map_err(|error| error.to_string())?;
    let refresh_token_duration_left = refresh_token_expiration.duration_since(SystemTime::now());

    let is_access_token_expiring_soon = match access_token_duration_left {
        Ok(duration) => duration < Duration::from_secs(AUTH_REFRESH_WITHIN_S),
        Err(_) => true,
    };
    let is_refresh_token_expiring_soon = match refresh_token_duration_left {
        Ok(duration) => duration < Duration::from_secs(AUTH_REFRESH_WITHIN_S),
        Err(_) => true,
    };

    match (
        is_refresh_token_expiring_soon,
        is_access_token_expiring_soon,
    ) {
        (true, _) => {
            let (access_token, new_refresh_token) =
                generate_auth_tokens_relayer(auth_client, keypair.as_ref()).await?;
            shared_access_token.store(Arc::new(access_token));
            Ok(Some(new_refresh_token))
        }
        (false, true) => {
            let response = auth_client
                .refresh_access_token(RefreshAccessTokenRequest {
                    refresh_token: refresh_token.value.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;

            let access_token = response
                .into_inner()
                .access_token
                .ok_or_else(|| "missing access token".to_string())?;
            shared_access_token.store(Arc::new(access_token));
            Ok(None)
        }
        (false, false) => Ok(None),
    }
}

async fn generate_auth_tokens_relayer(
    auth_client: &mut AuthServiceClient<Channel>,
    keypair: &solana_keypair::Keypair,
) -> Result<(Token, Token), String> {
    use {
        jito_protos::proto::auth::{
            GenerateAuthChallengeRequest, GenerateAuthTokensRequest, GenerateAuthTokensResponse,
            Role,
        },
        solana_signer::Signer,
    };

    let auth_response = auth_client
        .generate_auth_challenge(GenerateAuthChallengeRequest {
            role: Role::Relayer.into(),
            pubkey: keypair.pubkey().to_bytes().to_vec(),
        })
        .await
        .map_err(|error| error.to_string())?;

    let challenge = format!(
        "{}-{}",
        keypair.pubkey(),
        auth_response.into_inner().challenge
    );
    let signed_challenge = keypair.sign_message(challenge.as_bytes()).as_ref().to_vec();

    let GenerateAuthTokensResponse {
        access_token: maybe_access_token,
        refresh_token: maybe_refresh_token,
    } = auth_client
        .generate_auth_tokens(GenerateAuthTokensRequest {
            challenge,
            client_pubkey: keypair.pubkey().as_ref().to_vec(),
            signed_challenge,
        })
        .await
        .map_err(|error| error.to_string())?
        .into_inner();

    let access_token = maybe_access_token.ok_or_else(|| "missing access token".to_string())?;
    let refresh_token = maybe_refresh_token.ok_or_else(|| "missing refresh token".to_string())?;

    if access_token.expires_at_utc.is_none() || refresh_token.expires_at_utc.is_none() {
        return Err("auth tokens missing expiration".to_string());
    }

    Ok((access_token, refresh_token))
}

fn endpoint_from_url(url: &str) -> Result<Endpoint, String> {
    let mut endpoint = Endpoint::from_shared(url.to_owned()).map_err(|error| error.to_string())?;
    if url.starts_with("https") {
        endpoint = endpoint
            .tls_config(tonic::transport::ClientTlsConfig::new())
            .map_err(|error| error.to_string())?;
    }
    Ok(endpoint)
}

fn blocklisted_entries_from_merged(
    merged_entries: &[PostPackConfirmation],
    blocklisted_uuids: &[String],
) -> Vec<PostPackConfirmation> {
    let blocklist: HashSet<&str> = blocklisted_uuids.iter().map(String::as_str).collect();
    merged_entries
        .iter()
        .filter(|entry| blocklist.contains(entry.uuid.as_str()))
        .cloned()
        .collect()
}

fn filter_blocklisted_uuids(
    entries: &[PostPackConfirmation],
    blocklisted_uuids: &[String],
) -> Vec<PostPackConfirmation> {
    let blocklist: HashSet<&str> = blocklisted_uuids.iter().map(String::as_str).collect();
    entries
        .iter()
        .filter(|entry| !blocklist.contains(entry.uuid.as_str()))
        .cloned()
        .collect()
}

fn union_postpack_confirmation_entries(
    pda_entries: &[PostPackConfirmation],
    admin_entries: &[PostPackConfirmation],
) -> Vec<PostPackConfirmation> {
    let mut by_url = HashMap::new();
    for entry in pda_entries {
        by_url.insert(entry.url.clone(), entry.clone());
    }
    for entry in admin_entries {
        by_url.insert(entry.url.clone(), entry.clone());
    }
    by_url.into_values().collect()
}
