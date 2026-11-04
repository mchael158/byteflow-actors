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

/// Send every hop to one socket (same as [`super::spawn_tcp_outbound`] with a router).
///
/// Ignores [`OutboundHop::node`]; use when all remote traffic targets one peer.
#[derive(Clone, Copy, Debug)]
pub struct FixedPeerRouter(pub SocketAddr);

impl NodeRouter for FixedPeerRouter {
    fn resolve(&self, _node: u64) -> Result<SocketAddr, TransportError> {
        Ok(self.0)
    }
}

/// Fixed table for tests and small deployments.
///
/// Build peers with [`Self::with_peer`] before wrapping in [`Arc`](std::sync::Arc);
/// [`Self::insert`] needs `&mut self` and is not usable through a shared handle.
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
            .ok_or(TransportError::UnknownNode { node })
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
        match router.resolve(99) {
            Err(TransportError::UnknownNode { node: 99 }) => {}
            _ => {
                return Err(TransportError::InvalidConfig(
                    "expected UnknownNode for missing id",
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn fixed_peer_router_ignores_node_id() -> Result<(), TransportError> {
        let addr: SocketAddr = "127.0.0.1:9"
            .parse()
            .map_err(|_| TransportError::InvalidConfig("test addr parse"))?;
        let router = FixedPeerRouter(addr);
        assert_eq!(router.resolve(0)?, addr);
        assert_eq!(router.resolve(u64::MAX)?, addr);
        Ok(())
    }
}
