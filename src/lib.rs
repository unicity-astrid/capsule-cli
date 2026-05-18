use std::collections::HashMap;

use astrid_sdk::net::{
    StreamHandle, TryRecvError, accept, bind_unix, close, send, try_accept, try_recv,
};
use astrid_sdk::prelude::*;

#[derive(Default)]
struct CliProxy;

#[capsule]
impl CliProxy {
    #[astrid::run]
    fn run(&self) -> Result<(), SysError> {
        // 1. Subscribe to TUI-relevant IPC topics only.
        // IMPORTANT: If a new event topic is consumed by the TUI, add it here.
        // Internal pipeline events (LLM requests, tool dispatch, identity builds)
        // must NOT be forwarded to the CLI socket.
        let topics = [
            "agent.v1.response",
            "astrid.v1.onboarding.required",
            "astrid.v1.elicit.*",
            "astrid.v1.approval",
            "astrid.v1.response.*",
            "astrid.v1.capsules_loaded",
            "registry.v1.response.*",
            "registry.v1.active_model_changed",
            "registry.v1.selection.*",
            "session.v1.response.*",
            // Layer 6 admin response fan-out (issue #672) — per-noun
            // because the matcher requires equal segment counts.
            "astrid.v1.admin.response.agent.*",
            "astrid.v1.admin.response.group.*",
            "astrid.v1.admin.response.caps.*",
            "astrid.v1.admin.response.quota.*",
        ];
        let sub_handles: Vec<_> = topics
            .iter()
            .map(|t| ipc::subscribe(t).map_err(|e| SysError::ApiError(e.to_string())))
            .collect::<Result<Vec<_>, _>>()?;

        // Signal readiness so the kernel can proceed with loading dependent capsules.
        // Best-effort: failure means the host mutex is poisoned (unrecoverable).
        let _ = runtime::signal_ready();

        // 2. Resolve the socket path from the kernel-injected config.
        // bind_unix is a no-op on the host side (the kernel pre-binds the socket),
        // but the path is used for logging and future diagnostics.
        let path = runtime::socket_path()
            .map_err(|e| SysError::ApiError(format!("Failed to resolve socket path: {e}")))?;

        log::info(format!("CLI Proxy: accepting connections on {path}"));
        let listener = bind_unix().map_err(|e| SysError::ApiError(e.to_string()))?;

        // 3. Multi-connection accept loop.
        // Supports up to 8 concurrent CLI clients (enforced at host level).
        // Session-scoped IPC events are routed to the owning client; topic-
        // scoped broadcasts (capsules_loaded, model changed) fan out to all.
        // Ownership is recorded the first time a client publishes a payload
        // with a `session_id` field — typically on the user.v1.prompt that
        // opens the session.
        let mut streams: Vec<StreamHandle> = Vec::new();
        let mut session_owners: HashMap<String, u64> = HashMap::new();

        'proxy: loop {
            // Phase A: block until at least one client is connected.
            if streams.is_empty() {
                let stream = match accept(&listener) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn(format!("Accept error: {e:?}, backing off"));
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        continue;
                    }
                };
                log::info("CLI client connected to proxy");
                streams.push(stream);
            }

            // Phase B: poll for one additional connection (non-blocking).
            // Max one per iteration to bound handshake stall to ~5s worst case.
            if let Ok(Some(new_stream)) = try_accept(&listener) {
                log::info("Additional CLI client connected to proxy");
                streams.push(new_stream);
            }

            // Phase C: read from all streams.
            // NOTE: 50ms timeout per stream = linear scaling (N*50ms per iteration).
            // Acceptable for CLI use (2-3 typical, 8 max = 400ms worst case).
            let mut dead_indices: Vec<usize> = Vec::new();
            for (i, stream) in streams.iter().enumerate() {
                let stream_id = stream.id();
                match try_recv(stream) {
                    Ok(bytes) => handle_ingress(&bytes, stream_id, &mut session_owners),
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Closed) => {
                        log::info("CLI client disconnected from proxy");
                        dead_indices.push(i);
                    }
                }
            }

            // Remove dead streams in reverse order to preserve indices.
            // close() is required to release the host-side active_streams entry.
            // Without it, active_streams.len() grows monotonically and poll_accept
            // refuses new connections after MAX_ACTIVE_STREAMS cumulative disconnects.
            // Each dead stream's session bindings get evicted so a future client
            // can reuse the session_id without inheriting the stale ownership.
            // Collect every dead stream's id first so the eviction below
            // is a single linear pass over `session_owners` rather than
            // one full scan per dead stream (small N today, but the
            // O(N*M) shape is easy to avoid).
            let dead_ids: Vec<u64> = dead_indices.iter().map(|&i| streams[i].id()).collect();
            for &i in dead_indices.iter().rev() {
                let dead = streams.remove(i);
                let _ = close(&dead);
            }
            session_owners.retain(|_, owner_id| !dead_ids.contains(owner_id));

            // Phase D: poll IPC subscriptions and route to the owning stream(s).
            // NOTE: broadcast_dead indices are into streams AFTER Phase C removals.
            let mut broadcast_dead: Vec<usize> = Vec::new();
            for handle in &sub_handles {
                match ipc::poll(handle) {
                    Ok(result) => {
                        if !result.messages.is_empty() {
                            route_poll_messages(
                                &streams,
                                &session_owners,
                                &result,
                                &mut broadcast_dead,
                            );
                        }
                    }
                    Err(_) => {
                        log::error("IPC subscription error, proxy shutting down");
                        break 'proxy;
                    }
                }
            }

            // Remove streams that failed during broadcast.
            // Multiple subscriptions may flag the same stream as dead in one
            // iteration. sort + dedup before removal prevents double-removal panics.
            broadcast_dead.sort_unstable();
            broadcast_dead.dedup();
            // Same pattern as Phase C: snapshot dead ids first so the
            // session-owner eviction is a single retain pass.
            let dead_ids: Vec<u64> = broadcast_dead.iter().map(|&i| streams[i].id()).collect();
            for &i in broadcast_dead.iter().rev() {
                let dead = streams.remove(i);
                let _ = close(&dead);
                log::info("CLI client disconnected during broadcast");
            }
            session_owners.retain(|_, owner_id| !dead_ids.contains(owner_id));
        }

        // Reached only when an IPC subscription fails (break 'proxy above).
        Err(SysError::ApiError(
            "IPC subscription failed, proxy terminated".to_string(),
        ))
    }
}

/// Parse an incoming client message and publish it to the IPC bus if the
/// topic passes the ingress allowlist. Records session ownership so that
/// session-scoped responses are routed back to this stream alone.
fn handle_ingress(bytes: &[u8], stream_id: u64, session_owners: &mut HashMap<String, u64>) {
    let msg = match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(v) => v,
        Err(_) => {
            log::warn("Received malformed IPC payload from socket");
            return;
        }
    };

    let (Some(topic), Some(payload)) = (
        msg.get("topic").and_then(|t| t.as_str()),
        msg.get("payload"),
    ) else {
        log::warn("Dropped ingress message: missing topic or payload");
        return;
    };

    if !is_allowed_ingress_topic(topic) {
        log::warn(format!("Dropped ingress message to blocked topic: {topic}"));
        return;
    }

    // Last-writer-wins: a re-attaching client takes over the session, and
    // the original stream simply stops receiving session-scoped responses.
    // Reconnection on the same stream replays the same `(session_id,
    // stream_id)` pair and is a no-op.
    if let Some(session_id) = extract_session_id(payload) {
        session_owners.insert(session_id.to_string(), stream_id);
    }

    if let Err(e) = ipc::publish_json(topic, payload) {
        log::error(format!("Failed to publish IPC: {e:?}"));
    }
}

/// Extract `session_id` from a payload value if present and a string.
/// Used both on ingress (record ownership) and egress (pick recipient).
fn extract_session_id(payload: &serde_json::Value) -> Option<&str> {
    payload.get("session_id").and_then(|s| s.as_str())
}

/// Route each IPC message from a `PollResult` to the owning stream when
/// the payload carries a `session_id`, or fan out to all live streams
/// when it does not (e.g. `astrid.v1.capsules_loaded`,
/// `registry.v1.active_model_changed`).
///
/// Tracks failed stream indices in `dead`. Dropped events from the bus
/// are reported once per poll batch.
fn route_poll_messages(
    streams: &[StreamHandle],
    session_owners: &HashMap<String, u64>,
    poll_result: &ipc::PollResult,
    dead: &mut Vec<usize>,
) {
    if poll_result.dropped > 0 {
        log::warn(format!(
            "Event bus dropped {} messages - TUI may be stale",
            poll_result.dropped
        ));
    }

    for msg in &poll_result.messages {
        // Parse the payload string back to a JSON value so the TUI
        // receives an embedded object, not an escaped string. If parsing
        // fails (raw text payload), fall through to broadcast — there's
        // no session_id to extract from a non-object value.
        let payload = serde_json::from_str::<serde_json::Value>(&msg.payload)
            .unwrap_or_else(|_| serde_json::Value::String(msg.payload.clone()));

        // Session-scoped messages route to the single owning stream.
        // Topic-scoped broadcasts (no session_id in payload, or a session
        // we've never seen — e.g. a response to a request published by a
        // capsule rather than a socket client) fan out to every live
        // stream.
        let target_owner: Option<u64> =
            extract_session_id(&payload).and_then(|sid| session_owners.get(sid).copied());

        let frame = match serde_json::to_vec(&serde_json::json!({
            "topic": &msg.topic,
            "payload": payload,
            "source_id": &msg.source_id,
        })) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };

        for (i, stream) in streams.iter().enumerate() {
            if dead.contains(&i) {
                continue;
            }
            // `let_chains` (`if let X && Y`) is nightly-only — use the
            // stable `is_some_and` equivalent so this builds on the
            // capsule's pinned wasm32-wasip1 stable toolchain.
            if target_owner.is_some_and(|owner_id| stream.id() != owner_id) {
                continue;
            }
            if let Err(e) = send(stream, &frame) {
                log::warn(format!(
                    "Socket send error, client likely disconnected: {e:?}"
                ));
                dead.push(i);
            }
        }
    }
}

/// Exact topics the CLI is allowed to publish to the internal IPC bus.
/// Note: `client.v1.disconnect` is NOT here - the authoritative disconnect
/// event is published by `close()` (via `net_close_stream_impl`) to avoid
/// double-counting in the idle monitor.
const ALLOWED_INGRESS_EXACT: &[&str] = &["user.v1.prompt", "cli.v1.command.execute"];

/// Topic prefixes the CLI is allowed to publish (suffix-routed topics).
/// IMPORTANT: Update this list when adding new CLI-originated topic prefixes.
const ALLOWED_INGRESS_PREFIXES: &[&str] = &[
    "astrid.v1.request.",
    "astrid.v1.elicit.response.",
    "astrid.v1.approval.response.",
    "registry.v1.selection.",
    "session.v1.request.",
    // Layer 6 admin IPC (issue #672) — kernel-side authorization in
    // `authorize_request` enforces per-caller capability checks.
    "astrid.v1.admin.",
];

fn is_allowed_ingress_topic(topic: &str) -> bool {
    ALLOWED_INGRESS_EXACT.contains(&topic)
        || ALLOWED_INGRESS_PREFIXES
            .iter()
            .any(|p| topic.starts_with(p))
}
