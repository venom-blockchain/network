use std::sync::Arc;

use anyhow::Result;
use everscale_crypto::ed25519;
use ton_api::ton::{self, TLObject};

pub use self::overlay_shard::{
    IncomingBroadcastInfo, OutgoingBroadcastInfo, OverlayShard, OverlayShardMetrics,
    OverlayShardOptions, ReceivedPeersMap,
};
use crate::adnl_node::*;
use crate::proto;
use crate::subscriber::*;
use crate::utils::*;

mod broadcast_receiver;
mod overlay_shard;

pub struct OverlayNode {
    adnl: Arc<AdnlNode>,
    node_key: Arc<StoredAdnlNodeKey>,
    shards: FxDashMap<OverlayIdShort, Arc<OverlayShard>>,
    subscribers: FxDashMap<OverlayIdShort, Arc<dyn OverlaySubscriber>>,
    zero_state_file_hash: [u8; 32],
}

impl OverlayNode {
    pub fn new(
        adnl: Arc<AdnlNode>,
        zero_state_file_hash: [u8; 32],
        key_tag: usize,
    ) -> Result<Arc<Self>> {
        let node_key = adnl.key_by_tag(key_tag)?;
        Ok(Arc::new(Self {
            adnl,
            node_key,
            shards: Default::default(),
            subscribers: Default::default(),
            zero_state_file_hash,
        }))
    }

    pub fn metrics(&self) -> impl Iterator<Item = (OverlayIdShort, OverlayShardMetrics)> + '_ {
        self.shards.iter().map(|item| (*item.id(), item.metrics()))
    }

    pub fn adnl(&self) -> &Arc<AdnlNode> {
        &self.adnl
    }

    pub fn add_subscriber(
        &self,
        overlay_id: OverlayIdShort,
        subscriber: Arc<dyn OverlaySubscriber>,
    ) -> bool {
        use dashmap::mapref::entry::Entry;

        match self.subscribers.entry(overlay_id) {
            Entry::Vacant(entry) => {
                entry.insert(subscriber);
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    pub fn add_private_peers(
        &self,
        local_id: &AdnlNodeIdShort,
        peers: &[(AdnlAddressUdp, ed25519::PublicKey)],
    ) -> Result<Vec<AdnlNodeIdShort>> {
        let mut new_peers = Vec::new();

        for (peer_ip_address, public_key) in peers {
            let (peer_full_id, peer_id) = public_key.compute_node_ids();

            let is_new_peer = self.adnl.add_peer(
                PeerContext::PrivateOverlay,
                local_id,
                &peer_id,
                *peer_ip_address,
                peer_full_id,
            )?;

            if is_new_peer {
                new_peers.push(peer_id);
            }
        }

        Ok(new_peers)
    }

    pub fn delete_private_peers(
        &self,
        local_id: &AdnlNodeIdShort,
        peers: &[AdnlNodeIdShort],
    ) -> Result<bool> {
        let mut changed = false;
        for peer_id in peers {
            changed |= self.adnl.delete_peer(local_id, peer_id)?;
        }
        Ok(changed)
    }

    pub fn add_public_overlay(
        &self,
        overlay_id: &OverlayIdShort,
        options: OverlayShardOptions,
    ) -> (Arc<OverlayShard>, bool) {
        self.add_overlay_shard(overlay_id, None, options)
    }

    pub fn add_private_overlay(
        &self,
        overlay_id: &OverlayIdShort,
        overlay_key: &Arc<StoredAdnlNodeKey>,
        peers: &[AdnlNodeIdShort],
        options: OverlayShardOptions,
    ) -> bool {
        let (shard, new) = self.add_overlay_shard(overlay_id, Some(overlay_key.clone()), options);
        if new {
            shard.add_known_peers(peers);
        }
        new
    }

    pub fn delete_private_overlay(&self, overlay_id: &OverlayIdShort) -> Result<bool> {
        use dashmap::mapref::entry::Entry;

        match self.shards.entry(*overlay_id) {
            Entry::Occupied(entry) => {
                if !entry.get().is_private() {
                    return Err(OverlayNodeError::DeletingPublicOverlay.into());
                }
                entry.remove();
                Ok(true)
            }
            Entry::Vacant(_) => Ok(false),
        }
    }

    pub fn compute_overlay_id(&self, workchain: i32) -> OverlayIdFull {
        compute_overlay_id(workchain, self.zero_state_file_hash)
    }

    pub fn compute_overlay_short_id(&self, workchain: i32) -> OverlayIdShort {
        self.compute_overlay_id(workchain).compute_short_id()
    }

    pub fn get_overlay_shard(&self, overlay_id: &OverlayIdShort) -> Result<Arc<OverlayShard>> {
        match self.shards.get(overlay_id) {
            Some(shard) => Ok(shard.clone()),
            None => Err(OverlayNodeError::UnknownOverlay.into()),
        }
    }

    fn add_overlay_shard(
        &self,
        overlay_id: &OverlayIdShort,
        overlay_key: Option<Arc<StoredAdnlNodeKey>>,
        options: OverlayShardOptions,
    ) -> (Arc<OverlayShard>, bool) {
        use dashmap::mapref::entry::Entry;

        match self.shards.entry(*overlay_id) {
            Entry::Vacant(entry) => {
                let overlay_shard = entry
                    .insert(OverlayShard::new(
                        self.adnl.clone(),
                        self.node_key.clone(),
                        *overlay_id,
                        overlay_key,
                        options,
                    ))
                    .clone();
                (overlay_shard, true)
            }
            Entry::Occupied(entry) => (entry.get().clone(), false),
        }
    }
}

#[async_trait::async_trait]
impl Subscriber for OverlayNode {
    async fn try_consume_custom(
        &self,
        local_id: &AdnlNodeIdShort,
        peer_id: &AdnlNodeIdShort,
        data: &[u8],
    ) -> Result<bool> {
        let (message, broadcast) =
            match tl_proto::deserialize::<(proto::overlay::Message, proto::overlay::Broadcast)>(
                data,
            ) {
                Ok(bundle) => bundle,
                Err(_) => return Ok(false),
            };

        let overlay_id = OverlayIdShort::from(*message.overlay);
        let shard = self.get_overlay_shard(&overlay_id)?;

        match broadcast {
            proto::overlay::Broadcast::Broadcast(broadcast) => {
                shard
                    .receive_broadcast(local_id, peer_id, broadcast, data)
                    .await?;
                Ok(true)
            }
            proto::overlay::Broadcast::BroadcastFec(broadcast) => {
                shard
                    .receive_fec_broadcast(local_id, peer_id, broadcast, data)
                    .await?;
                Ok(true)
            }
            _ => Err(OverlayNodeError::UnsupportedOverlayBroadcastMessage.into()),
        }

        /* UNUSED UNTIL VALIDATOR LOGIC WILL BE NEEDED

        // Extract messages
        let catchain_update = match bundle.remove(0).downcast::<ton::catchain::Update>() {
            Ok(ton::catchain::Update::Catchain_BlockUpdate(message)) => *message,
            _ => return Err(OverlayNodeError::UnsupportedPrivateOverlayMessage.into()),
        };

        let validator_session_update = match bundle
            .remove(0)
            .downcast::<ton::validator_session::BlockUpdate>(
        ) {
            Ok(ton::validator_session::BlockUpdate::ValidatorSession_BlockUpdate(
                message,
            )) => *message,
            _ => return Err(OverlayNodeError::UnsupportedPrivateOverlayMessage.into()),
        };

        // Notify waiters
        shard.push_catchain(CatchainUpdate {
            peer_id: *peer_id,
            catchain_update,
            validator_session_update,
        });

        // Done
        Ok(true)

        */
    }

    async fn try_consume_query_bundle(
        &self,
        local_id: &AdnlNodeIdShort,
        peer_id: &AdnlNodeIdShort,
        mut queries: Vec<TLObject>,
    ) -> Result<QueryBundleConsumingResult> {
        if queries.len() != 2 {
            return Ok(QueryBundleConsumingResult::Rejected(queries));
        }

        let overlay_id = match queries.remove(0).downcast::<ton::rpc::overlay::Query>() {
            Ok(query) => query.into(),
            Err(query) => {
                queries.insert(0, query);
                return Ok(QueryBundleConsumingResult::Rejected(queries));
            }
        };

        let query = match queries
            .remove(0)
            .downcast::<ton::rpc::overlay::GetRandomPeers>()
        {
            Ok(query) => {
                let shard = self.get_overlay_shard(&overlay_id)?;
                return QueryBundleConsumingResult::consume(shard.process_get_random_peers(query));
            }
            Err(query) => query,
        };

        let consumer = match self.subscribers.get(&overlay_id) {
            Some(consumer) => consumer.clone(),
            None => return Err(OverlayNodeError::NoConsumerFound.into()),
        };

        match consumer.try_consume_query(local_id, peer_id, query).await? {
            QueryConsumingResult::Consumed(result) => {
                Ok(QueryBundleConsumingResult::Consumed(result))
            }
            QueryConsumingResult::Rejected(_) => Err(OverlayNodeError::UnsupportedQuery.into()),
        }
    }
}

pub const MAX_OVERLAY_PEERS: usize = 65536;

#[derive(thiserror::Error, Debug)]
enum OverlayNodeError {
    #[error("Unsupported overlay broadcast message")]
    UnsupportedOverlayBroadcastMessage,
    #[error("Unknown overlay")]
    UnknownOverlay,
    #[error("Cannot delete public overlay")]
    DeletingPublicOverlay,
    #[error("No consumer for message in overlay")]
    NoConsumerFound,
    #[error("Unsupported query")]
    UnsupportedQuery,
}
