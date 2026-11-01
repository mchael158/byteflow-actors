//! Trusted host → flow event delivery via Atomic Hop messages.

use std::sync::Arc;

use byteflow::{FlowId, Message, Runtime, SendError, Value};

/// Helper for injecting driver events into a Byteflow flow.
///
/// Holds an [`Arc<Runtime>`] so MQTT/UART callbacks on foreign threads can
/// retain the router without lifetime ties to a stack frame.
///
/// Always delivers [`Value::Message`]. Bare scalars violate Atomic Hop rules
/// and are rejected by [`Runtime::send`].
#[derive(Clone)]
pub struct EventRouter {
    runtime: Arc<Runtime>,
}

impl std::fmt::Debug for EventRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventRouter").finish_non_exhaustive()
    }
}

impl EventRouter {
    /// Retain a runtime for trusted host-side delivery from any thread.
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    /// Borrow the shared runtime handle.
    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    /// Deliver an event through the authenticated host send path.
    ///
    /// The runtime stamps host sender identity. `target` must come from
    /// trusted host bookkeeping — never from untrusted bytecode in a
    /// multi-tenant embedding.
    ///
    /// # Errors
    ///
    /// Propagates [`SendError`] from [`Runtime::send`].
    pub fn send(
        &self,
        target: FlowId,
        request_id: u64,
        tag: u16,
        payload: Value,
    ) -> Result<(), SendError> {
        let message = Message::request(request_id, tag, payload);
        self.runtime.send(target, Value::Message(message))
    }
}
