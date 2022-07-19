use async_trait::async_trait;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::future::Fuse;
use futures::{select, FutureExt, SinkExt, StreamExt};
pub use json::JsonNetworkingParametersProvider;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use lru::LruCache;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, Sleep};
use tracing::trace;

// Size of the LRU cache for peers.
const PEER_CACHE_SIZE: usize = 100;
// Pause duration between network parameters save.
const DATA_FLUSH_DURATION_SECS: u64 = 5;

/// An alias for the reference to a `NetworkingParametersProvider` implementation.
pub type NetworkingParametersHandler =
    Arc<dyn NetworkingParametersProvider + Send + Sync + 'static>;

/// Networking parameters container.
#[derive(Debug)]
pub struct NetworkingParameters {
    /// LRU cache for the known peers and their addresses
    pub known_peers: LruCache<PeerId, HashSet<Multiaddr>>,
}

impl Clone for NetworkingParameters {
    fn clone(&self) -> Self {
        let mut known_peers = LruCache::new(self.known_peers.cap());

        for (peer_id, addresses) in self.known_peers.iter() {
            known_peers.push(*peer_id, addresses.clone());
        }

        Self { known_peers }
    }
}

impl NetworkingParameters {
    /// A type constructor. Cache cap defines LRU cache size for known peers.
    fn new(cache_cap: usize) -> Self {
        Self {
            known_peers: LruCache::new(cache_cap),
        }
    }

    /// Add peer with its addresses to the cache.
    fn add_known_peer(&mut self, peer_id: PeerId, addr_set: HashSet<Multiaddr>) {
        if let Some(addresses) = self.known_peers.get_mut(&peer_id) {
            *addresses = addresses.union(&addr_set).cloned().collect()
        } else {
            self.known_peers.push(peer_id, addr_set);
        }
    }

    /// Returns a collection of peer ID and address from the cache.
    fn get_known_peer_addresses(&self, peer_number: usize) -> Vec<(PeerId, Multiaddr)> {
        self.known_peers
            .iter()
            .take(peer_number)
            .flat_map(|(peer_id, addresses)| addresses.iter().map(|addr| (*peer_id, addr.clone())))
            .collect()
    }
}

/// Defines operations with the networking parameters.
#[async_trait]
pub trait NetworkingParametersRegistry {
    /// Registers a peer ID and associated addresses
    async fn add_known_peer(&mut self, peer_id: PeerId, addresses: Vec<Multiaddr>);

    /// Returns bootstrap addresses from networking parameters DB. It optionally removes p2p-protocol
    /// suffix. Peer number parameter limits peers to process.
    fn bootstrap_addresses(
        &self,
        remove_p2p_protocol_suffix: bool,
        peer_number: usize,
    ) -> Vec<(PeerId, Multiaddr)>;

    /// Drive async work in the persistence provider
    async fn run(&mut self);
}

/// Defines networking parameters persistence operations
pub trait NetworkingParametersProvider: Send {
    /// Loads networking parameters from the underlying DB implementation.
    fn load(&self) -> anyhow::Result<NetworkingParameters>;

    /// Saves networking to the underlying DB implementation.
    fn save(&self, params: &NetworkingParameters) -> anyhow::Result<()>;
}

/// Handles networking parameters. It manages network parameters set and its persistence.
pub struct NetworkingParametersManager<P: NetworkingParametersProvider> {
    // Sends data to the cache operator job.
    tx: UnboundedSender<(PeerId, HashSet<Multiaddr>)>,
    // Receives data from the cache operator job.
    rx: UnboundedReceiver<(PeerId, HashSet<Multiaddr>)>,
    // Persistence provider for the networking parameters.
    network_parameters_persistence_handler: NetworkingParametersHandler,
    // Networking paramters working cache.
    networking_params: NetworkingParameters,
    // Period between networking parameters saves.
    networking_parameters_save_delay: Pin<Box<Fuse<Sleep>>>,
    // Defines `NetworkingParametersProvider` implementation.
    _persistence_marker: PhantomData<P>,
}

impl<P: NetworkingParametersProvider + Send> NetworkingParametersManager<P> {
    /// Object constructor. It accepts `NetworkingParametersProvider` implementation as a parameter.
    /// On object creation it starts a job for networking parameters cache handling.
    pub fn new(
        network_parameters_persistence_handler: NetworkingParametersHandler,
    ) -> NetworkingParametersManager<P> {
        let (tx, rx) = mpsc::unbounded();

        let networking_params = network_parameters_persistence_handler
            .load()
            .unwrap_or_else(|_| NetworkingParameters::new(PEER_CACHE_SIZE));

        NetworkingParametersManager {
            tx,
            rx,
            network_parameters_persistence_handler,
            networking_params,
            networking_parameters_save_delay: Self::default_delay(),
            _persistence_marker: PhantomData,
        }
    }

    // Create default delay for networking parameters.
    fn default_delay() -> Pin<Box<Fuse<Sleep>>> {
        Box::pin(sleep(Duration::from_secs(DATA_FLUSH_DURATION_SECS)).fuse())
    }
}

#[async_trait]
impl<P: NetworkingParametersProvider + Send> NetworkingParametersRegistry
    for NetworkingParametersManager<P>
{
    async fn add_known_peer(&mut self, peer_id: PeerId, addresses: Vec<Multiaddr>) {
        let addr_set = addresses.iter().cloned().collect::<HashSet<_>>();

        // Ignore the send result.
        let _ = self.tx.send((peer_id, addr_set)).await;
    }

    /// Returns bootstrap addresses from networking parameters DB. It optionally removes p2p-protocol
    /// suffix.
    fn bootstrap_addresses(
        &self,
        remove_p2p_protocol_suffix: bool,
        peer_number: usize,
    ) -> Vec<(PeerId, Multiaddr)> {
        self.networking_params
            .get_known_peer_addresses(peer_number)
            .into_iter()
            .map(|(peer_id, addr)| {
                remove_p2p_protocol_suffix
                    .then(|| {
                        // remove p2p-protocol suffix if any
                        let mut modified_address = addr.clone();

                        if let Some(Protocol::P2p(_)) = modified_address.pop() {
                            Some((peer_id, modified_address))
                        } else {
                            None
                        }
                    })
                    .flatten()
                    .unwrap_or((peer_id, addr)) // keep the original
            })
            .collect()
    }

    async fn run(&mut self) {
        select! {
            _ = &mut self.networking_parameters_save_delay => {
                if let Err(err) = self.network_parameters_persistence_handler.save(
                    &self.networking_params.clone()
                ) {
                    trace!(error=%err, "Error on saving network parameters");
                }
                self.networking_parameters_save_delay = NetworkingParametersManager::<P>::default_delay();
            },
            data = self.rx.next() => {
                if let Some((peer_id, addr_set)) = data {
                    trace!("New networking parameters received: {:?}", (peer_id, &addr_set));

                    self.networking_params.add_known_peer(peer_id, addr_set);
                }
            }
        };
    }
}

mod json {
    use super::{NetworkingParameters, NetworkingParametersProvider, PEER_CACHE_SIZE};
    use anyhow::Context;
    use libp2p::{Multiaddr, PeerId};
    use lru::LruCache;
    use serde::{Deserialize, Serialize};
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::PathBuf;
    use tracing::trace;

    // Helper struct for JsonNetworkingPersistence
    #[derive(Default, Debug, Serialize, Deserialize)]
    struct JsonNetworkingParameters {
        pub known_peers: HashMap<PeerId, HashSet<Multiaddr>>,
    }

    impl From<&NetworkingParameters> for JsonNetworkingParameters {
        fn from(cache: &NetworkingParameters) -> Self {
            Self {
                known_peers: cache
                    .known_peers
                    .iter()
                    .map(|(peer_id, addresses)| (*peer_id, addresses.clone()))
                    .collect(),
            }
        }
    }

    impl From<JsonNetworkingParameters> for NetworkingParameters {
        fn from(params: JsonNetworkingParameters) -> Self {
            let mut known_peers = LruCache::<PeerId, HashSet<Multiaddr>>::new(PEER_CACHE_SIZE);

            for (peer_id, addresses) in params.known_peers.iter() {
                known_peers.push(*peer_id, addresses.clone());
            }

            Self { known_peers }
        }
    }

    /// JSON implementation for the networking parameters provider.
    #[derive(Clone)]
    pub struct JsonNetworkingParametersProvider {
        /// JSON file path
        path: PathBuf,
    }

    impl JsonNetworkingParametersProvider {
        /// Constructor
        pub fn new(path: PathBuf) -> Self {
            trace!(?path, "JSON networking parameters created.");

            JsonNetworkingParametersProvider { path }
        }
    }

    impl NetworkingParametersProvider for JsonNetworkingParametersProvider {
        fn load(&self) -> anyhow::Result<NetworkingParameters> {
            let data = fs::read(&self.path).context("Unable to read file")?;

            let result: JsonNetworkingParameters = serde_json::from_slice(&data)
                .context("Cannot deserialize networking parameters to JSON")?;

            trace!("Networking parameters loaded from JSON file");

            Ok(result.into())
        }

        fn save(&self, params: &NetworkingParameters) -> anyhow::Result<()> {
            let params: JsonNetworkingParameters = params.into();
            let data = serde_json::to_string(&params)
                .context("Cannot serialize networking parameters to JSON")?;

            fs::write(&self.path, data)
                .context("Unable to write file with networking parameters")?;

            trace!("Networking parameters saved to JSON file");

            Ok(())
        }
    }
}

/// The default implementation for networking provider stub. It doesn't save or load anything and
/// only logs attempts.
#[derive(Clone)]
pub struct NetworkingParametersProviderStub;

impl NetworkingParametersProvider for NetworkingParametersProviderStub {
    fn load(&self) -> anyhow::Result<NetworkingParameters> {
        trace!("Default network parameters used");

        Ok(NetworkingParameters::new(PEER_CACHE_SIZE))
    }

    fn save(&self, _: &NetworkingParameters) -> anyhow::Result<()> {
        trace!("Network parameters saving skipped.");

        Ok(())
    }
}
