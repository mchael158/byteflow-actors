//! Map bytecode [`OutboundHop::node`](crate::OutboundHop) values to socket
//! peers. Byteflow does not run a cluster; the embedder owns routing.

use std::collections::HashMap;
use std::net::SocketAddr;

use crate::OutboundHop;

use super::TransportError;

/// Resolve a host-defined node id to a TCP/UDP peer.
pub trait NodeRouter: Send + Sync {
    fn resolve(&self, node: u64) -> Result<SocketAddr, TransportError>;
}

/// Fixed table for tests and small deployments.
#[derive(Clone, Debug, Default)]
pub struct StaticNodeRouter {
    peers: HashMap<u64, SocketAddr>,
}

impl StaticNodeRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, node: u64, addr: SocketAddr) {
        self.peers.insert(node, addr);
    }

    pub fn with_peer(mut self, node: u64, addr: SocketAddr) -> Self {
        self.insert(node, addr);
        self
    }
}

impl NodeRouter for StaticNodeRouter {
    fn resolve(&self, node: u64) -> Result<SocketAddr, TransportError> {
        self.peers
            .get(&node)
            .copied()
            .ok_or_else(|| TransportError::InvalidConfig("node router: unknown node id"))
    }
}

/// Convenience when draining [`crate::DistributionBridge`] manually.
pub fn resolve_hop_peer<R: NodeRouter + ?Sized>(
    hop: &OutboundHop,
    router: &R,
) -> Result<SocketAddr, TransportError> {
    router.resolve(hop.node)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_router_resolves() -> Result<(), TransportError> {
        let addr: SocketAddr = "127.0.0.1:9"
            .parse()
            .map_err(|_| TransportError::InvalidConfig("test addr parse"))?;
        let router = StaticNodeRouter::new().with_peer(2, addr);
        assert_eq!(router.resolve(2)?, addr);
        assert!(router.resolve(99).is_err());
        Ok(())
    }
}
