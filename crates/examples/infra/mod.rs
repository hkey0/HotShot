#![allow(clippy::panic)]
use async_compatibility_layer::art::async_sleep;
use async_compatibility_layer::logging::{setup_backtrace, setup_logging};
use async_lock::RwLock;
use async_trait::async_trait;
use clap::Parser;
use futures::StreamExt;
use hotshot::traits::implementations::{CombinedNetworks, UnderlyingCombinedNetworks};
use hotshot::{
    traits::{
        implementations::{Libp2pNetwork, MemoryStorage, NetworkingMetricsValue, WebServerNetwork},
        NodeImplementation,
    },
    types::{SignatureKey, SystemContextHandle},
    Memberships, Networks, SystemContext,
};
use hotshot_example_types::{
    block_types::{TestBlockHeader, TestBlockPayload, TestTransaction},
    state_types::TestInstanceState,
};
use hotshot_orchestrator::config::NetworkConfigSource;
use hotshot_orchestrator::{
    self,
    client::{OrchestratorClient, ValidatorArgs},
    config::{NetworkConfig, NetworkConfigFile, WebServerConfig},
};
use hotshot_types::message::Message;
use hotshot_types::traits::network::ConnectedNetwork;
use hotshot_types::ValidatorConfig;
use hotshot_types::{
    consensus::ConsensusMetricsValue,
    data::{Leaf, TestableLeaf},
    event::{Event, EventType},
    traits::{
        block_contents::TestableBlock,
        election::Membership,
        node_implementation::{ConsensusTime, NodeType},
        states::TestableState,
    },
    HotShotConfig,
};
use libp2p_identity::{
    ed25519::{self, SecretKey},
    Keypair,
};
use libp2p_networking::{
    network::{MeshParams, NetworkNodeConfigBuilder, NetworkNodeType},
    reexport::Multiaddr,
};
use rand::rngs::StdRng;
use rand::SeedableRng;
use std::marker::PhantomData;
use std::time::Duration;
use std::{collections::BTreeSet, sync::Arc};
use std::{num::NonZeroUsize, str::FromStr};
use surf_disco::Url;

use libp2p_identity::PeerId;
use std::fmt::Debug;
use std::{fs, time::Instant};
use tracing::{error, info, warn};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "Multi-machine consensus",
    about = "Simulates consensus among multiple machines"
)]
/// Arguments passed to the orchestrator
pub struct OrchestratorArgs {
    /// The url the orchestrator runs on; this should be in the form of `http://localhost:5555` or `http://0.0.0.0:5555`
    pub url: Url,
    /// The configuration file to be used for this run
    pub config_file: String,
}

#[derive(Parser, Debug, Clone)]
#[command(
    name = "Multi-machine consensus",
    about = "Simulates consensus among multiple machines"
)]
/// The configuration file to be used for this run
pub struct ConfigArgs {
    /// The configuration file to be used for this run
    pub config_file: String,
}

/// Reads a network configuration from a given filepath
/// # Panics
/// if unable to convert the config file into toml
/// # Note
/// This derived config is used for initialization of orchestrator,
/// therefore `known_nodes_with_stake` will be an initialized
/// vector full of the node's own config.
/// `my_own_validator_config` will be generated from seed here
/// for loading config from orchestrator,
/// or else it will be loaded from file.
#[must_use]
pub fn load_config_from_file<TYPES: NodeType>(
    config_file: &str,
) -> NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType> {
    let config_file_as_string: String = fs::read_to_string(config_file)
        .unwrap_or_else(|_| panic!("Could not read config file located at {config_file}"));
    let config_toml: NetworkConfigFile<TYPES::SignatureKey> =
        toml::from_str::<NetworkConfigFile<TYPES::SignatureKey>>(&config_file_as_string)
            .expect("Unable to convert config file to TOML");

    let mut config: NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType> =
        config_toml.into();

    // my_own_validator_config would be best to load from file,
    // but its type is too complex to load so we'll generate it from seed now
    config.config.my_own_validator_config =
        ValidatorConfig::generated_from_seed_indexed(config.seed, config.node_index, 1);
    let my_own_validator_config_with_stake = config
        .config
        .my_own_validator_config
        .public_key
        .get_stake_table_entry(1u64);
    // initialize it with size for better assignment of other peers' config
    config.config.known_nodes_with_stake =
        vec![my_own_validator_config_with_stake; config.config.total_nodes.get() as usize];

    config
}

/// Runs the orchestrator
pub async fn run_orchestrator<
    TYPES: NodeType,
    DACHANNEL: ConnectedNetwork<Message<TYPES>, TYPES::SignatureKey> + Debug,
    QUORUMCHANNEL: ConnectedNetwork<Message<TYPES>, TYPES::SignatureKey> + Debug,
    NODE: NodeImplementation<TYPES, Storage = MemoryStorage<TYPES>>,
>(
    OrchestratorArgs { url, config_file }: OrchestratorArgs,
) {
    error!("Starting orchestrator",);
    let run_config = load_config_from_file::<TYPES>(&config_file);
    let _result = hotshot_orchestrator::run_orchestrator::<
        TYPES::SignatureKey,
        TYPES::ElectionConfigType,
    >(run_config, url)
    .await;
}

/// Helper function to calculate the nuymber of transactions to send per node per round
#[allow(clippy::cast_possible_truncation)]
fn calculate_num_tx_per_round(
    node_index: u64,
    total_num_nodes: usize,
    transactions_per_round: usize,
) -> usize {
    transactions_per_round / total_num_nodes
        + usize::from(
            (total_num_nodes - 1 - node_index as usize)
                < (transactions_per_round % total_num_nodes),
        )
}

/// create a web server network from a config file + public key
/// # Panics
/// Panics if the web server config doesn't exist in `config`
fn webserver_network_from_config<TYPES: NodeType>(
    config: NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
    pub_key: TYPES::SignatureKey,
) -> WebServerNetwork<TYPES> {
    // Get the configuration for the web server
    let WebServerConfig {
        url,
        wait_between_polls,
    }: WebServerConfig = config.web_server_config.unwrap();

    WebServerNetwork::create(url, wait_between_polls, pub_key, false)
}

#[allow(clippy::cast_possible_truncation)]
#[allow(clippy::cast_lossless)]
#[allow(clippy::too_many_lines)]
/// Create a libp2p network from a config file and public key
/// # Panics
/// If unable to create bootstrap nodes multiaddres or the libp2p config is invalid
async fn libp2p_network_from_config<TYPES: NodeType>(
    config: NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
    pub_key: TYPES::SignatureKey,
) -> Libp2pNetwork<Message<TYPES>, TYPES::SignatureKey> {
    let mut config = config;
    let libp2p_config = config
        .libp2p_config
        .take()
        .expect("Configuration is not for a Libp2p network");
    let bs_len = libp2p_config.bootstrap_nodes.len();
    let bootstrap_nodes: Vec<(PeerId, Multiaddr)> = libp2p_config
        .bootstrap_nodes
        .iter()
        .map(|(addr, pair)| {
            let kp = Keypair::from_protobuf_encoding(pair).unwrap();
            let peer_id = PeerId::from_public_key(&kp.public());
            let multiaddr =
                Multiaddr::from_str(&format!("/ip4/{}/udp/{}/quic-v1", addr.ip(), addr.port()))
                    .unwrap();
            (peer_id, multiaddr)
        })
        .collect();
    let identity = libp2p_generate_indexed_identity(config.seed, config.node_index);
    let node_type = if (config.node_index as usize) < bs_len {
        NetworkNodeType::Bootstrap
    } else {
        NetworkNodeType::Regular
    };
    let node_index = config.node_index;
    let port_index = if libp2p_config.index_ports {
        node_index
    } else {
        0
    };
    let bound_addr: Multiaddr = format!(
        "/{}/{}/udp/{}/quic-v1",
        if libp2p_config.public_ip.is_ipv4() {
            "ip4"
        } else {
            "ip6"
        },
        libp2p_config.public_ip,
        libp2p_config.base_port as u64 + port_index
    )
    .parse()
    .unwrap();

    // generate network
    let mut config_builder = NetworkNodeConfigBuilder::default();
    assert!(config.config.total_nodes.get() > 2);
    let replicated_nodes = NonZeroUsize::new(config.config.total_nodes.get() - 2).unwrap();
    config_builder.replication_factor(replicated_nodes);
    config_builder.identity(identity.clone());

    config_builder.bound_addr(Some(bound_addr.clone()));

    let to_connect_addrs = bootstrap_nodes
        .iter()
        .map(|(peer_id, multiaddr)| (Some(*peer_id), multiaddr.clone()))
        .collect();

    config_builder.to_connect_addrs(to_connect_addrs);

    let mesh_params =
        // NOTE I'm arbitrarily choosing these.
        match node_type {
            NetworkNodeType::Bootstrap => MeshParams {
                mesh_n_high: libp2p_config.bootstrap_mesh_n_high,
                mesh_n_low: libp2p_config.bootstrap_mesh_n_low,
                mesh_outbound_min: libp2p_config.bootstrap_mesh_outbound_min,
                mesh_n: libp2p_config.bootstrap_mesh_n,
            },
            NetworkNodeType::Regular => MeshParams {
                mesh_n_high: libp2p_config.mesh_n_high,
                mesh_n_low: libp2p_config.mesh_n_low,
                mesh_outbound_min: libp2p_config.mesh_outbound_min,
                mesh_n: libp2p_config.mesh_n,
            },
            NetworkNodeType::Conductor => unreachable!(),
        };
    config_builder.mesh_params(Some(mesh_params));

    let mut all_keys = BTreeSet::new();
    let mut da_keys = BTreeSet::new();
    for i in 0..config.config.total_nodes.get() as u64 {
        let privkey = TYPES::SignatureKey::generated_from_seed_indexed([0u8; 32], i).1;
        let pub_key = TYPES::SignatureKey::from_private(&privkey);
        if i < config.config.da_committee_size as u64 {
            da_keys.insert(pub_key.clone());
        }
        all_keys.insert(pub_key);
    }
    let node_config = config_builder.build().unwrap();

    #[allow(clippy::cast_possible_truncation)]
    Libp2pNetwork::new(
        NetworkingMetricsValue::default(),
        node_config,
        pub_key.clone(),
        Arc::new(RwLock::new(
            bootstrap_nodes
                .iter()
                .map(|(peer_id, addr)| (Some(*peer_id), addr.clone()))
                .collect(),
        )),
        bs_len,
        config.node_index as usize,
        // NOTE: this introduces an invariant that the keys are assigned using this indexed
        // function
        all_keys,
        None,
        da_keys.clone(),
        da_keys.contains(&pub_key),
    )
    .await
    .unwrap()
}

/// Defines the behavior of a "run" of the network with a given configuration
#[async_trait]
pub trait RunDA<
    TYPES: NodeType<InstanceState = TestInstanceState>,
    DANET: ConnectedNetwork<Message<TYPES>, TYPES::SignatureKey> + Debug,
    QUORUMNET: ConnectedNetwork<Message<TYPES>, TYPES::SignatureKey> + Debug,
    NODE: NodeImplementation<
        TYPES,
        QuorumNetwork = QUORUMNET,
        CommitteeNetwork = DANET,
        Storage = MemoryStorage<TYPES>,
    >,
> where
    <TYPES as NodeType>::ValidatedState: TestableState,
    <TYPES as NodeType>::BlockPayload: TestableBlock,
    TYPES: NodeType<Transaction = TestTransaction>,
    Leaf<TYPES>: TestableLeaf,
    Self: Sync,
{
    /// Initializes networking, returns self
    async fn initialize_networking(
        config: NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
    ) -> Self;

    /// Initializes the genesis state and HotShot instance; does not start HotShot consensus
    /// # Panics if it cannot generate a genesis block, fails to initialize HotShot, or cannot
    /// get the anchored view
    /// Note: sequencing leaf does not have state, so does not return state
    async fn initialize_state_and_hotshot(&self) -> SystemContextHandle<TYPES, NODE> {
        let initializer = hotshot::HotShotInitializer::<TYPES>::from_genesis(&TestInstanceState {})
            .expect("Couldn't generate genesis block");

        let config = self.get_config();

        // Get KeyPair for certificate Aggregation
        let pk = config.config.my_own_validator_config.public_key.clone();
        let sk = config.config.my_own_validator_config.private_key.clone();
        let known_nodes_with_stake = config.config.known_nodes_with_stake.clone();

        let da_network = self.get_da_channel();
        let quorum_network = self.get_quorum_channel();

        // Since we do not currently pass the election config type in the NetworkConfig, this will always be the default election config
        let quorum_election_config = config.config.election_config.clone().unwrap_or_else(|| {
            TYPES::Membership::default_election_config(config.config.total_nodes.get() as u64)
        });

        let committee_election_config = TYPES::Membership::default_election_config(
            config.config.da_committee_size.try_into().unwrap(),
        );
        let networks_bundle = Networks {
            quorum_network: quorum_network.clone().into(),
            da_network: da_network.clone().into(),
            _pd: PhantomData,
        };

        let memberships = Memberships {
            quorum_membership: <TYPES as NodeType>::Membership::create_election(
                known_nodes_with_stake.clone(),
                quorum_election_config.clone(),
            ),
            da_membership: <TYPES as NodeType>::Membership::create_election(
                known_nodes_with_stake.clone(),
                committee_election_config,
            ),
            vid_membership: <TYPES as NodeType>::Membership::create_election(
                known_nodes_with_stake.clone(),
                quorum_election_config.clone(),
            ),
            view_sync_membership: <TYPES as NodeType>::Membership::create_election(
                known_nodes_with_stake.clone(),
                quorum_election_config,
            ),
        };

        SystemContext::init(
            pk,
            sk,
            config.node_index,
            config.config,
            MemoryStorage::empty(),
            memberships,
            networks_bundle,
            initializer,
            ConsensusMetricsValue::default(),
        )
        .await
        .expect("Could not init hotshot")
        .0
    }

    /// Starts HotShot consensus, returns when consensus has finished
    async fn run_hotshot(
        &self,
        context: SystemContextHandle<TYPES, NODE>,
        transactions: &mut Vec<TestTransaction>,
        transactions_to_send_per_round: u64,
    ) {
        let NetworkConfig {
            rounds,
            node_index,
            start_delay_seconds,
            ..
        } = self.get_config();

        let mut total_transactions_committed = 0;
        let mut total_transactions_sent = 0;

        error!("Sleeping for {start_delay_seconds} seconds before starting hotshot!");
        async_sleep(Duration::from_secs(start_delay_seconds)).await;

        error!("Starting HotShot example!");
        let start = Instant::now();

        let mut event_stream = context.get_event_stream();
        let mut anchor_view: TYPES::Time = <TYPES::Time as ConsensusTime>::genesis();
        let mut num_successful_commits = 0;

        context.hotshot.start_consensus().await;

        loop {
            match event_stream.next().await {
                None => {
                    panic!("Error! Event stream completed before consensus ended.");
                }
                Some(Event { event, .. }) => {
                    match event {
                        EventType::Error { error } => {
                            error!("Error in consensus: {:?}", error);
                            // TODO what to do here
                        }
                        EventType::Decide {
                            leaf_chain,
                            qc: _,
                            block_size,
                        } => {
                            // this might be a obob
                            if let Some((leaf, _)) = leaf_chain.first() {
                                info!("Decide event for leaf: {}", *leaf.view_number);

                                let new_anchor = leaf.view_number;
                                if new_anchor >= anchor_view {
                                    anchor_view = leaf.view_number;
                                }

                                // send transactions
                                for _ in 0..transactions_to_send_per_round {
                                    let tx = transactions.remove(0);

                                    () = context.submit_transaction(tx).await.unwrap();
                                    total_transactions_sent += 1;
                                }
                            }

                            if let Some(size) = block_size {
                                total_transactions_committed += size;
                            }

                            num_successful_commits += leaf_chain.len();
                            if num_successful_commits >= rounds {
                                break;
                            }

                            if leaf_chain.len() > 1 {
                                warn!("Leaf chain is greater than 1 with len {}", leaf_chain.len());
                            }
                            // when we make progress, submit new events
                        }
                        EventType::ReplicaViewTimeout { view_number } => {
                            warn!("Timed out as a replicas in view {:?}", view_number);
                        }
                        EventType::NextLeaderViewTimeout { view_number } => {
                            warn!("Timed out as the next leader in view {:?}", view_number);
                        }
                        _ => {}
                    }
                }
            }
        }

        // Output run results
        let total_time_elapsed = start.elapsed();
        error!("[{node_index}]: {rounds} rounds completed in {total_time_elapsed:?} - Total transactions sent: {total_transactions_sent} - Total transactions committed: {total_transactions_committed} - Total commitments: {num_successful_commits}");
    }

    /// Returns the da network for this run
    fn get_da_channel(&self) -> DANET;

    /// Returns the quorum network for this run
    fn get_quorum_channel(&self) -> QUORUMNET;

    /// Returns the config for this run
    fn get_config(&self) -> NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>;
}

// WEB SERVER

/// Represents a web server-based run
pub struct WebServerDARun<TYPES: NodeType> {
    /// the network configuration
    config: NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
    /// quorum channel
    quorum_channel: WebServerNetwork<TYPES>,
    /// data availability channel
    da_channel: WebServerNetwork<TYPES>,
}

#[async_trait]
impl<
        TYPES: NodeType<
            Transaction = TestTransaction,
            BlockPayload = TestBlockPayload,
            BlockHeader = TestBlockHeader,
            InstanceState = TestInstanceState,
        >,
        NODE: NodeImplementation<
            TYPES,
            QuorumNetwork = WebServerNetwork<TYPES>,
            CommitteeNetwork = WebServerNetwork<TYPES>,
            Storage = MemoryStorage<TYPES>,
        >,
    > RunDA<TYPES, WebServerNetwork<TYPES>, WebServerNetwork<TYPES>, NODE> for WebServerDARun<TYPES>
where
    <TYPES as NodeType>::ValidatedState: TestableState,
    <TYPES as NodeType>::BlockPayload: TestableBlock,
    Leaf<TYPES>: TestableLeaf,
    Self: Sync,
{
    async fn initialize_networking(
        config: NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
    ) -> WebServerDARun<TYPES> {
        // Get our own key
        let pub_key = config.config.my_own_validator_config.public_key.clone();

        // extract values from config (for DA network)
        let WebServerConfig {
            url,
            wait_between_polls,
        }: WebServerConfig = config.clone().da_web_server_config.unwrap();

        // create and wait for underlying network
        let underlying_quorum_network =
            webserver_network_from_config::<TYPES>(config.clone(), pub_key.clone());

        underlying_quorum_network.wait_for_ready().await;

        let da_channel: WebServerNetwork<TYPES> =
            WebServerNetwork::create(url.clone(), wait_between_polls, pub_key.clone(), true);

        WebServerDARun {
            config,
            quorum_channel: underlying_quorum_network,
            da_channel,
        }
    }

    fn get_da_channel(&self) -> WebServerNetwork<TYPES> {
        self.da_channel.clone()
    }

    fn get_quorum_channel(&self) -> WebServerNetwork<TYPES> {
        self.quorum_channel.clone()
    }

    fn get_config(&self) -> NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType> {
        self.config.clone()
    }
}

// Libp2p

/// Represents a libp2p-based run
pub struct Libp2pDARun<TYPES: NodeType> {
    /// the network configuration
    config: NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
    /// quorum channel
    quorum_channel: Libp2pNetwork<Message<TYPES>, TYPES::SignatureKey>,
    /// data availability channel
    da_channel: Libp2pNetwork<Message<TYPES>, TYPES::SignatureKey>,
}

#[async_trait]
impl<
        TYPES: NodeType<
            Transaction = TestTransaction,
            BlockPayload = TestBlockPayload,
            BlockHeader = TestBlockHeader,
            InstanceState = TestInstanceState,
        >,
        NODE: NodeImplementation<
            TYPES,
            QuorumNetwork = Libp2pNetwork<Message<TYPES>, TYPES::SignatureKey>,
            CommitteeNetwork = Libp2pNetwork<Message<TYPES>, TYPES::SignatureKey>,
            Storage = MemoryStorage<TYPES>,
        >,
    >
    RunDA<
        TYPES,
        Libp2pNetwork<Message<TYPES>, TYPES::SignatureKey>,
        Libp2pNetwork<Message<TYPES>, TYPES::SignatureKey>,
        NODE,
    > for Libp2pDARun<TYPES>
where
    <TYPES as NodeType>::ValidatedState: TestableState,
    <TYPES as NodeType>::BlockPayload: TestableBlock,
    Leaf<TYPES>: TestableLeaf,
    Self: Sync,
{
    async fn initialize_networking(
        config: NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
    ) -> Libp2pDARun<TYPES> {
        let pub_key = config.config.my_own_validator_config.public_key.clone();

        // create and wait for underlying network
        let quorum_channel = libp2p_network_from_config::<TYPES>(config.clone(), pub_key).await;

        let da_channel = quorum_channel.clone();
        quorum_channel.wait_for_ready().await;

        Libp2pDARun {
            config,
            quorum_channel,
            da_channel,
        }
    }

    fn get_da_channel(&self) -> Libp2pNetwork<Message<TYPES>, TYPES::SignatureKey> {
        self.da_channel.clone()
    }

    fn get_quorum_channel(&self) -> Libp2pNetwork<Message<TYPES>, TYPES::SignatureKey> {
        self.quorum_channel.clone()
    }

    fn get_config(&self) -> NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType> {
        self.config.clone()
    }
}

// Combined network

/// Represents a combined-network-based run
pub struct CombinedDARun<TYPES: NodeType> {
    /// the network configuration
    config: NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
    /// quorum channel
    quorum_channel: CombinedNetworks<TYPES>,
    /// data availability channel
    da_channel: CombinedNetworks<TYPES>,
}

#[async_trait]
impl<
        TYPES: NodeType<
            Transaction = TestTransaction,
            BlockPayload = TestBlockPayload,
            BlockHeader = TestBlockHeader,
            InstanceState = TestInstanceState,
        >,
        NODE: NodeImplementation<
            TYPES,
            Storage = MemoryStorage<TYPES>,
            QuorumNetwork = CombinedNetworks<TYPES>,
            CommitteeNetwork = CombinedNetworks<TYPES>,
        >,
    > RunDA<TYPES, CombinedNetworks<TYPES>, CombinedNetworks<TYPES>, NODE> for CombinedDARun<TYPES>
where
    <TYPES as NodeType>::ValidatedState: TestableState,
    <TYPES as NodeType>::BlockPayload: TestableBlock,
    Leaf<TYPES>: TestableLeaf,
    Self: Sync,
{
    async fn initialize_networking(
        config: NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
    ) -> CombinedDARun<TYPES> {
        // generate our own key
        let (pub_key, _privkey) =
            <<TYPES as NodeType>::SignatureKey as SignatureKey>::generated_from_seed_indexed(
                config.seed,
                config.node_index,
            );

        // create and wait for libp2p network
        let libp2p_underlying_quorum_network =
            libp2p_network_from_config::<TYPES>(config.clone(), pub_key.clone()).await;

        libp2p_underlying_quorum_network.wait_for_ready().await;

        // extract values from config (for webserver DA network)
        let WebServerConfig {
            url,
            wait_between_polls,
        }: WebServerConfig = config.clone().da_web_server_config.unwrap();

        // create and wait for underlying webserver network
        let web_quorum_network =
            webserver_network_from_config::<TYPES>(config.clone(), pub_key.clone());

        let web_da_network = WebServerNetwork::create(url, wait_between_polls, pub_key, true);

        web_quorum_network.wait_for_ready().await;

        // combine the two communication channel

        let da_channel = CombinedNetworks::new(Arc::new(UnderlyingCombinedNetworks(
            web_da_network.clone(),
            libp2p_underlying_quorum_network.clone(),
        )));
        let quorum_channel = CombinedNetworks::new(Arc::new(UnderlyingCombinedNetworks(
            web_quorum_network.clone(),
            libp2p_underlying_quorum_network.clone(),
        )));

        CombinedDARun {
            config,
            quorum_channel,
            da_channel,
        }
    }

    fn get_da_channel(&self) -> CombinedNetworks<TYPES> {
        self.da_channel.clone()
    }

    fn get_quorum_channel(&self) -> CombinedNetworks<TYPES> {
        self.quorum_channel.clone()
    }

    fn get_config(&self) -> NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType> {
        self.config.clone()
    }
}

/// Main entry point for validators
/// # Panics
/// if unable to get the local ip address
pub async fn main_entry_point<
    TYPES: NodeType<
        Transaction = TestTransaction,
        BlockPayload = TestBlockPayload,
        BlockHeader = TestBlockHeader,
        InstanceState = TestInstanceState,
    >,
    DACHANNEL: ConnectedNetwork<Message<TYPES>, TYPES::SignatureKey> + Debug,
    QUORUMCHANNEL: ConnectedNetwork<Message<TYPES>, TYPES::SignatureKey> + Debug,
    NODE: NodeImplementation<
        TYPES,
        QuorumNetwork = QUORUMCHANNEL,
        CommitteeNetwork = DACHANNEL,
        Storage = MemoryStorage<TYPES>,
    >,
    RUNDA: RunDA<TYPES, DACHANNEL, QUORUMCHANNEL, NODE>,
>(
    args: ValidatorArgs,
) where
    <TYPES as NodeType>::ValidatedState: TestableState,
    <TYPES as NodeType>::BlockPayload: TestableBlock,
    Leaf<TYPES>: TestableLeaf,
{
    setup_logging();
    setup_backtrace();

    error!("Starting validator");

    // see what our public identity will be
    let public_ip = match args.public_ip {
        Some(ip) => ip,
        None => local_ip_address::local_ip().unwrap(),
    };

    let orchestrator_client: OrchestratorClient =
        OrchestratorClient::new(args.clone(), public_ip.to_string());

    // conditionally save/load config from file or orchestrator
    let (mut run_config, source) =
        NetworkConfig::<TYPES::SignatureKey, TYPES::ElectionConfigType>::from_file_or_orchestrator(
            &orchestrator_client,
            args.clone().network_config_file,
        )
        .await;

    let node_index = run_config.node_index;
    error!("Retrieved config; our node index is {node_index}");

    // one more round of orchestrator here to get peer's public key/config
    let updated_config: NetworkConfig<TYPES::SignatureKey, TYPES::ElectionConfigType> =
        orchestrator_client
            .post_and_wait_all_public_keys::<TYPES::SignatureKey, TYPES::ElectionConfigType>(
                run_config.node_index,
                run_config.config.my_own_validator_config.public_key.clone(),
            )
            .await;
    run_config.config.known_nodes_with_stake = updated_config.config.known_nodes_with_stake;

    error!("Initializing networking");
    let run = RUNDA::initialize_networking(run_config.clone()).await;
    let hotshot = run.initialize_state_and_hotshot().await;

    // pre-generate transactions
    let NetworkConfig {
        transaction_size,
        rounds,
        transactions_per_round,
        node_index,
        config: HotShotConfig { total_nodes, .. },
        ..
    } = run_config;

    let mut txn_rng = StdRng::seed_from_u64(node_index);
    let transactions_to_send_per_round =
        calculate_num_tx_per_round(node_index, total_nodes.get(), transactions_per_round);
    let mut transactions = Vec::new();

    for round in 0..rounds {
        for _ in 0..transactions_to_send_per_round {
            let mut txn = <TYPES::ValidatedState>::create_random_transaction(
                None,
                &mut txn_rng,
                transaction_size as u64,
            );

            // prepend destined view number to transaction
            let view_execute_number: u64 = round as u64 + 4;
            txn.0[0..8].copy_from_slice(&view_execute_number.to_be_bytes());

            transactions.push(txn);
        }
    }

    if let NetworkConfigSource::Orchestrator = source {
        error!("Waiting for the start command from orchestrator");
        orchestrator_client
            .wait_for_all_nodes_ready(run_config.clone().node_index)
            .await;
    }

    error!("Starting HotShot");
    run.run_hotshot(
        hotshot,
        &mut transactions,
        transactions_to_send_per_round as u64,
    )
    .await;
}

/// generate a libp2p identity based on a seed and idx
/// # Panics
/// if unable to create a secret key out of bytes
#[must_use]
pub fn libp2p_generate_indexed_identity(seed: [u8; 32], index: u64) -> Keypair {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&seed);
    hasher.update(&index.to_le_bytes());
    let new_seed = *hasher.finalize().as_bytes();
    let sk_bytes = SecretKey::try_from_bytes(new_seed).unwrap();
    <ed25519::Keypair as From<SecretKey>>::from(sk_bytes).into()
}
