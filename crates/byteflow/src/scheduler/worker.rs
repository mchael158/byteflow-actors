//! Worker threads: drive flows, apply scheduler effects, enforce hop identity
//! and **FlowCap** resolution.
//!
//! # Authenticated Atomic Hop + capabilities
//!
//! Before mailbox delivery, outgoing hops are stamped (`Message.sender`) and
//! granted a **SEND**-only `reply_cap` (stable per recipient→sender pair).
//! `Send` / `Ask` resolve
//! [`Value::Cap`] through [`CapTable`](super::capability::CapTable); raw
//! [`Value::Pid`] is not an address.

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::bytecode::{CapId, Message, Value};
use crate::log;
use crate::vm::VmResult;
use crossbeam_deque::{Steal, Worker as LocalDeque};

use super::capability::{CapError, CapRights};
use super::error::report_fault;
use super::finalize::{finalize_flow, request_kill, resume_or_fail, DuplicateAsk};
use super::link::LinkId;
use super::mailbox::{AdmitSender, Delivery, Mailbox, ParkSender, WaitFilter};
use super::metrics::RuntimeMetrics;
use super::monitor::{FlowExitReason, MonitorRef};
use super::process::{Flow, FlowId, FlowOutcome, PendingSend};
use super::registry::RegistryName;
use super::runtime::{flow_id_from_u64, spawn_on, wake_workers, BytecodeSpawn, Shared};
use super::sync_lock;

/// S1 + reply grant: single choke-point before mailbox `push` on bytecode hops.
///
/// `Message.sender` / `reply_cap` are register/native data until this call.
/// The scheduler owns the executing flow’s identity and **assigns** both:
/// it does not “check equality” against forgeable fields (wrong security
/// primitive). Every future opcode that delivers a [`Message`] must call
/// this (or equivalent); do not duplicate ad-hoc stamp assignments elsewhere.
fn authenticate_outgoing_message(
    shared: &Shared,
    sender: FlowId,
    recipient: FlowId,
    mut message: Message,
    vm: &mut crate::vm::Vm,
) -> Result<Message, CapError> {
    if message.request_id == 0 {
        message.request_id = vm.fresh_request_id();
    }
    let payload = reissue_caps_in_value(shared, sender, recipient, (*message.payload).clone())?;
    let reply = shared
        .caps
        .mint_or_reuse(recipient, sender, CapRights::SEND)
        .map_err(CapError::from)?;
    Ok(message.with_payload(payload).authenticate(sender.as_u64(), reply))
}

/// Host `Runtime::send`: same choke-point as bytecode hops.
///
/// The embedder is not a flow — it has no mailbox — so `reply_cap` stays
/// [`CapId::NONE`]. `sender` is always [`FlowId::HOST`]. Caps in the payload
/// are reissued with the host's trusted path (`reissue_for`), not `delegate`.
pub(crate) fn authenticate_host_outgoing_message(
    shared: &Shared,
    recipient: FlowId,
    mut message: Message,
) -> Result<Message, CapError> {
    if message.request_id == 0 {
        let rid = shared
            .host_next_request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        message.request_id = if rid == 0 { 1 } else { rid };
    }
    let payload = reissue_caps_from_host(shared, recipient, (*message.payload).clone())?;
    Ok(message
        .with_payload(payload)
        .authenticate(FlowId::HOST.as_u64(), CapId::NONE))
}

/// Host is trusted: re-issue live Caps to `recipient` without a holder check.
fn reissue_caps_from_host(
    shared: &Shared,
    recipient: FlowId,
    value: Value,
) -> Result<Value, CapError> {
    match value {
        Value::Cap(id) if id.is_none() => Ok(Value::Cap(id)),
        Value::Cap(id) => {
            let new_id = shared.caps.reissue_for(id, recipient)?;
            Ok(Value::Cap(new_id))
        }
        Value::Message(m) => {
            let inner = (*m.payload).clone();
            let new_inner = reissue_caps_from_host(shared, recipient, inner)?;
            Ok(Value::Message(m.with_payload(new_inner)))
        }
        other => Ok(other),
    }
}

/// Re-issue every `Value::Cap` the sender holds so `recipient` becomes holder.
/// Nested hops are walked. `CapId::NONE` is left alone. Additive — original
/// tokens stay valid (same reason reply_cap is not one-shot).
fn reissue_caps_in_value(
    shared: &Shared,
    sender: FlowId,
    recipient: FlowId,
    value: Value,
) -> Result<Value, CapError> {
    match value {
        Value::Cap(id) if id.is_none() => Ok(Value::Cap(id)),
        Value::Cap(id) => {
            let new_id = shared.caps.delegate(id, sender, recipient)?;
            Ok(Value::Cap(new_id))
        }
        Value::Message(m) => {
            let inner = (*m.payload).clone();
            let new_inner = reissue_caps_in_value(shared, sender, recipient, inner)?;
            Ok(Value::Message(m.with_payload(new_inner)))
        }
        other => Ok(other),
    }
}

fn receive_filter(match_tag: Option<u16>, match_request_id: Option<u64>) -> WaitFilter {
    match (match_tag, match_request_id) {
        (Some(tag), Some(rid)) => WaitFilter::TaggedCorrelation {
            tag,
            expect_request_id: rid,
        },
        (Some(tag), None) => WaitFilter::Tag(tag),
        (None, Some(rid)) => WaitFilter::Correlation {
            expect_request_id: rid,
            expect_sender: None,
        },
        (None, None) => WaitFilter::Any,
    }
}

/// Resolve `CapId` for `holder`. Returns target [`FlowId`].
fn resolve_cap_held(
    shared: &Shared,
    id: CapId,
    holder: FlowId,
    need: CapRights,
) -> Result<FlowId, CapError> {
    Ok(shared
        .caps
        .resolve(id, holder, need)?
        .target()
        .ok_or(CapError::WrongTarget)?)
}

fn resolve_relation_cap(
    shared: &Shared,
    id: CapId,
    holder: FlowId,
    need: CapRights,
) -> Result<FlowId, String> {
    let entry = shared
        .caps
        .resolve(id, holder, need)
        .map_err(|e| e.to_string())?;
    let target = entry.target().ok_or_else(|| CapError::WrongTarget.to_string())?;
    let cell = match shared.caps.flow_cell(target) {
        Ok(Some(c)) => c,
        Ok(None) => return Err(CapError::Unknown.to_string()),
        Err(e) => return Err(e.to_string()),
    };
    let check = if need == CapRights::LINK {
        super::link_admin::check_link(&entry.cap, cell.as_ref(), target.as_u64())
            .map_err(|e| e.to_string())
    } else {
        super::link_admin::check_monitor(&entry.cap, cell.as_ref(), target.as_u64())
            .map_err(|e| e.to_string())
    };
    check?;
    Ok(target)
}

/// Worker main loop. Panics inside a flow are caught here so one
/// flow's bug cannot take the OS thread down (see `Fault` docs).
pub fn run_worker(shared: Arc<Shared>, local: LocalDeque<Box<Flow>>) {
    while !shared.shutdown.load(Ordering::Acquire) {
        match find_work(&shared, &local) {
            Some(flow) => drive_process(&shared, &local, flow),
            None => wait_for_work(&shared),
        }
    }
}

fn find_work(shared: &Shared, local: &LocalDeque<Box<Flow>>) -> Option<Box<Flow>> {
    if let Some(flow) = local.pop() {
        return Some(flow);
    }

    loop {
        match shared.injector.steal() {
            Steal::Success(flow) => return Some(flow),
            Steal::Empty => break,
            Steal::Retry => continue,
        }
    }

    for stealer in &shared.stealers {
        loop {
            match stealer.steal() {
                Steal::Success(flow) => {
                    RuntimeMetrics::inc(&shared.metrics.steals);
                    return Some(flow);
                }
                Steal::Empty => break,
                Steal::Retry => continue,
            }
        }
    }

    None
}

fn wait_for_work(shared: &Shared) {
    let (lock, cvar) = &shared.notify;
    let guard = match sync_lock::lock(lock, "worker::wait_for_work") {
        Ok(g) => g,
        Err(e) => {
            report_fault(e);
            return;
        }
    };
    if shared.shutdown.load(Ordering::Acquire) {
        return;
    }
    if let Err(e) =
        sync_lock::wait_timeout(cvar, guard, Duration::from_millis(50), "worker::wait_timeout")
    {
        report_fault(e);
    }
}

fn drive_process(
    shared: &Arc<Shared>,
    local: &LocalDeque<Box<Flow>>,
    mut flow: Box<Flow>,
) {
    if let Some(msg) = flow.pending_message.take() {
        match flow.last_receive_dest {
            Some(dest) => {
                let Some(f) = resume_or_fail(shared, flow, dest, msg) else {
                    return;
                };
                flow = f;
                flow.metrics
                    .messages_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            None => {
                finish_failed(shared, *flow, "pending hop missing dest register".into());
                return;
            }
        }
    }

    match shared.kill_signals.take(flow.id) {
        Ok(Some(reason)) => {
            finalize_flow(
                shared,
                *flow,
                FlowOutcome::Failed(kill_outcome_message(reason)),
                reason,
            );
            return;
        }
        Ok(None) => {}
        Err(e) => {
            report_fault(e);
            finish_failed(shared, *flow, "kill-signal table unavailable".into());
            return;
        }
    }

    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            local.push(flow);
            return;
        }

        let remaining = flow.quota.remaining_cpu();
        if remaining <= 0 {
            finish_failed(shared, *flow, super::quota::QuotaError::CpuExhausted.to_string());
            return;
        }
        let slice = (shared.quantum as i64).min(remaining) as u32;
        let before = flow.vm.instructions_executed();
        let ran = panic::catch_unwind(AssertUnwindSafe(|| {
            #[cfg(feature = "jit")]
            {
                super::jit::run_flow_quantum(&mut flow, shared, slice)
            }
            #[cfg(not(feature = "jit"))]
            {
                flow.vm.run(slice)
            }
        }));
        let delta = flow
            .vm
            .instructions_executed()
            .saturating_sub(before);
        if let Err(e) = flow.quota.charge_cpu(delta as i64) {
            finish_failed(shared, *flow, e.to_string());
            return;
        }
        flow
            .metrics
            .instructions
            .store(flow.vm.instructions_executed(), Ordering::Relaxed);

        let result = match ran {
            Ok(r) => r,
            Err(_) => {
                finish_failed(shared, *flow, "flow panicked".into());
                return;
            }
        };

        match result {
            VmResult::Complete(value) => {
                finish_ok(shared, *flow, value);
                return;
            }
            VmResult::Trap(fault) => {
                finish_failed(shared, *flow, fault.to_string());
                return;
            }
            VmResult::Yield => {
                RuntimeMetrics::inc(&shared.metrics.reschedules);
                flow.metrics.reschedules.fetch_add(1, Ordering::Relaxed);
                local.push(flow);
                return;
            }
            VmResult::Sleep(delay) => {
                shared.timer.schedule_sleep(delay, flow);
                return;
            }
            VmResult::SelfPid { dest_reg } => {
                match shared.caps.mint(flow.id, flow.id, CapRights::ADDRESSING) {
                    Ok(cap) => {
                        let Some(f) = resume_or_fail(shared, flow, dest_reg, Value::Cap(cap))
                        else {
                            return;
                        };
                        flow = f;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Spawn {
                function,
                args,
                dest_reg,
                requested_rights,
            } => {
                let ctx = BytecodeSpawn {
                    authority: &flow.authority,
                    cell: flow.cell.as_ref(),
                    quota: flow.quota.as_ref(),
                    requested_rights,
                };
                match spawn_on(
                    shared,
                    &flow.vm.chunk_arc(),
                    &flow.vm.natives_arc(),
                    function,
                    &args,
                    flow.restart_policy,
                    None,
                    Some(flow.id),
                    Some(ctx),
                ) {
                    Ok(child) => {
                        let child_id = child.id();
                        log::info(format!(
                            "spawn parent=flow#{} child=flow#{} fn={}",
                            flow.id.as_u64(),
                            child_id.as_u64(),
                            function
                        ));
                        match shared.caps.mint(flow.id, child_id, CapRights::ADDRESSING) {
                            Ok(cap) => {
                                let Some(f) =
                                    resume_or_fail(shared, flow, dest_reg, Value::Cap(cap))
                                else {
                                    return;
                                };
                                flow = f;
                            }
                            Err(e) => {
                                request_kill(shared, child_id, FlowExitReason::Killed);
                                finish_failed(shared, *flow, e.to_string());
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Send {
                target_cap,
                message,
            } => {
                let Some(msg) = message.as_message().cloned() else {
                    finish_failed(
                        shared,
                        *flow,
                        "Send invariant broken: hop is not Value::Message".into(),
                    );
                    return;
                };
                let target = match resolve_cap_held(shared, target_cap, flow.id, CapRights::SEND) {
                    Ok(id) => id,
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                };
                if let Err(e) = flow.quota.check_send() {
                    finish_failed(shared, *flow, e.to_string());
                    return;
                }
                let stamped = match authenticate_outgoing_message(
                    shared,
                    flow.id,
                    target,
                    msg,
                    &mut flow.vm,
                ) {
                    Ok(m) => Value::Message(m),
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                };
                let hop_cost = stamped.memory_size();
                if let Err(e) = flow.quota.alloc(hop_cost) {
                    finish_failed(shared, *flow, e.to_string());
                    return;
                }
                RuntimeMetrics::inc(&shared.metrics.messages_sent);
                flow.metrics
                    .messages_sent
                    .fetch_add(1, Ordering::Relaxed);
                log::info(format!(
                    "send from=flow#{} to=flow#{} via=cap#{} msg={}",
                    flow.id.as_u64(),
                    target.as_u64(),
                    target_cap,
                    stamped
                ));
                match deliver(shared, local, target, stamped.clone()) {
                    DeliverStatus::Ok => {
                        flow.quota.free(hop_cost);
                    }
                    DeliverStatus::Full => {
                        park_waiting_send(
                            shared,
                            flow,
                            target,
                            stamped,
                            PendingSend::FireAndForget,
                        );
                        return;
                    }
                    DeliverStatus::Gone => {
                        flow.quota.free(hop_cost);
                        finish_failed(shared, *flow, "send target gone".into());
                        return;
                    }
                    DeliverStatus::Unavailable => {
                        flow.quota.free(hop_cost);
                        finish_failed(shared, *flow, "mailbox unavailable".into());
                        return;
                    }
                }
            }
            VmResult::Receive {
                dest_reg,
                timeout,
                match_tag,
                match_request_id,
            } => {
                if !flow.authority.rights.contains(CapRights::RECV) {
                    finish_failed(shared, *flow, "receive denied: flow lacks RECV right".into());
                    return;
                }
                flow.last_receive_dest = Some(dest_reg);
                let filter = receive_filter(match_tag, match_request_id);
                match flow.mailbox.try_pop_filter(filter) {
                    Ok(Some(msg)) => {
                        flow.metrics
                            .messages_received
                            .fetch_add(1, Ordering::Relaxed);
                        log::info(format!(
                            "recv flow#{} filter={filter:?} msg={}",
                            flow.id.as_u64(),
                            msg
                        ));
                        let Some(f) = resume_or_fail(shared, flow, dest_reg, msg) else {
                            return;
                        };
                        flow = f;
                        admit_waiting_on(&flow.mailbox, shared, local);
                    }
                    Ok(None) => {
                        log::debug(format!(
                            "park flow#{} waiting mailbox filter={filter:?} timeout={timeout:?}",
                            flow.id.as_u64(),
                        ));
                        park_on_mailbox(shared, flow, dest_reg, timeout, filter);
                        return;
                    }
                    Err(e) => {
                        report_fault(e);
                        finish_failed(shared, *flow, "mailbox unavailable".into());
                        return;
                    }
                }
            }
            VmResult::Ask {
                dest_reg,
                target_cap,
                request,
                timeout,
            } => {
                let Some(req_msg) = request.as_message().cloned() else {
                    finish_failed(
                        shared,
                        *flow,
                        "Ask invariant broken: request is not Value::Message".into(),
                    );
                    return;
                };
                let target = match resolve_cap_held(shared, target_cap, flow.id, CapRights::ASK) {
                    Ok(id) => id,
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                };
                if let Err(e) = flow.quota.check_send() {
                    finish_failed(shared, *flow, e.to_string());
                    return;
                }
                let stamped_msg = match authenticate_outgoing_message(
                    shared,
                    flow.id,
                    target,
                    req_msg,
                    &mut flow.vm,
                ) {
                    Ok(m) => m,
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                };
                let request_id = stamped_msg.request_id;
                let stamped = Value::Message(stamped_msg);
                let hop_cost = stamped.memory_size();
                if let Err(e) = flow.quota.alloc(hop_cost) {
                    finish_failed(shared, *flow, e.to_string());
                    return;
                }
                // S2: expect FlowId of the Cap target (not CapId).
                let filter = WaitFilter::Correlation {
                    expect_request_id: request_id,
                    expect_sender: Some(target.as_u64()),
                };
                flow.last_receive_dest = Some(dest_reg);
                RuntimeMetrics::inc(&shared.metrics.messages_sent);
                flow.metrics
                    .messages_sent
                    .fetch_add(1, Ordering::Relaxed);
                log::info(format!(
                    "ask from=flow#{} to=flow#{} via=cap#{} req={} wait={filter:?}",
                    flow.id.as_u64(),
                    target.as_u64(),
                    target_cap,
                    stamped
                ));
                // Order: deliver request to *target*, then wait on *our*
                // mailbox. park_filter re-checks under the same mutex if the
                // reply raced ahead (anti lost-wakeup on the caller's inbox).
                match deliver(shared, local, target, stamped.clone()) {
                    DeliverStatus::Ok => {
                        flow.quota.free(hop_cost);
                    }
                    DeliverStatus::Full => {
                        park_waiting_send(
                            shared,
                            flow,
                            target,
                            stamped,
                            PendingSend::Ask {
                                dest_reg,
                                expect_request_id: request_id,
                                expect_sender: target.as_u64(),
                                timeout,
                            },
                        );
                        return;
                    }
                    DeliverStatus::Gone => {
                        flow.quota.free(hop_cost);
                        finish_failed(shared, *flow, "send target gone".into());
                        return;
                    }
                    DeliverStatus::Unavailable => {
                        flow.quota.free(hop_cost);
                        finish_failed(shared, *flow, "mailbox unavailable".into());
                        return;
                    }
                }
                match flow.mailbox.try_pop_filter(filter) {
                    Ok(Some(reply)) => {
                        flow.metrics
                            .messages_received
                            .fetch_add(1, Ordering::Relaxed);
                        log::info(format!(
                            "ask-reply ready flow#{} msg={}",
                            flow.id.as_u64(),
                            reply
                        ));
                        let Some(f) = resume_or_fail(shared, flow, dest_reg, reply) else {
                            return;
                        };
                        flow = f;
                    }
                    Ok(None) => {
                        log::debug(format!(
                            "ask park flow#{} filter={filter:?}",
                            flow.id.as_u64()
                        ));
                        park_ask(shared, flow, dest_reg, timeout, filter, target);
                        return;
                    }
                    Err(e) => {
                        report_fault(e);
                        finish_failed(shared, *flow, "mailbox unavailable".into());
                        return;
                    }
                }
            }
            VmResult::Monitor {
                dest_reg,
                target_cap,
            } => {
                let target = match resolve_relation_cap(shared, target_cap, flow.id, CapRights::MONITOR) {
                    Ok(id) => id,
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                };
                if target == flow.id {
                    finish_failed(shared, *flow, "cannot monitor self".into());
                    return;
                }
                match shared.monitors.create(flow.id, target) {
                    Ok(mon) => {
                        let ref_i = match i64::try_from(mon.as_u64()) {
                            Ok(n) => n,
                            Err(_) => i64::MAX,
                        };
                        let Some(f) =
                            resume_or_fail(shared, flow, dest_reg, Value::Int(ref_i))
                        else {
                            return;
                        };
                        flow = f;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Demonitor { monitor_reg } => {
                let raw = match flow.vm.top_registers().and_then(|r| r.get(monitor_reg as usize)) {
                    Some(Value::Int(n)) if *n >= 0 => *n as u64,
                    _ => {
                        finish_failed(shared, *flow, "demonitor: expected Int ref".into());
                        return;
                    }
                };
                match shared.monitors.remove_owned(flow.id, MonitorRef::from_u64(raw)) {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Link {
                dest_reg,
                target_cap,
            } => {
                let target = match resolve_relation_cap(shared, target_cap, flow.id, CapRights::LINK) {
                    Ok(id) => id,
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                };
                if target == flow.id {
                    finish_failed(shared, *flow, "cannot link self".into());
                    return;
                }
                match shared.links.link(flow.id, target) {
                    Ok(Ok(id)) => {
                        let ref_i = match i64::try_from(id.as_u64()) {
                            Ok(n) => n,
                            Err(_) => i64::MAX,
                        };
                        let Some(f) =
                            resume_or_fail(shared, flow, dest_reg, Value::Int(ref_i))
                        else {
                            return;
                        };
                        flow = f;
                    }
                    Ok(Err(e)) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Unlink { link_reg } => {
                let raw = match flow.vm.top_registers().and_then(|r| r.get(link_reg as usize)) {
                    Some(Value::Int(n)) if *n >= 0 => *n as u64,
                    _ => {
                        finish_failed(shared, *flow, "unlink: expected Int id".into());
                        return;
                    }
                };
                match shared.links.unlink_owned(flow.id, LinkId::from_u64(raw)) {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::RegisterName { name } => {
                if !flow.authority.rights.contains(CapRights::SEND) {
                    finish_failed(
                        shared,
                        *flow,
                        "register_name denied: flow lacks SEND right".into(),
                    );
                    return;
                }
                if name.is_empty() {
                    finish_failed(shared, *flow, "register_name: name must be non-empty".into());
                    return;
                }
                let cap = match shared
                    .caps
                    .mint(flow.id, flow.id, CapRights::ADDRESSING)
                {
                    Ok(id) => id,
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                };
                match shared
                    .registry
                    .register(RegistryName::from(name.as_ref()), cap, flow.id)
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Whereis { dest_reg, name } => {
                let target = match shared.registry.target(name.as_ref()) {
                    Ok(Some(id)) => id,
                    Ok(None) => {
                        let Some(f) = resume_or_fail(shared, flow, dest_reg, Value::Unit) else {
                            return;
                        };
                        flow = f;
                        continue;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                };
                match shared.directory.lookup(target) {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        let Some(f) = resume_or_fail(shared, flow, dest_reg, Value::Unit) else {
                            return;
                        };
                        flow = f;
                        continue;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
                match shared
                    .caps
                    .mint_or_reuse(flow.id, target, CapRights::SEND)
                {
                    Ok(cap) => {
                        let Some(f) = resume_or_fail(shared, flow, dest_reg, Value::Cap(cap))
                        else {
                            return;
                        };
                        flow = f;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Delegate {
                dest_reg,
                src_cap,
                want_rights,
                want_native_cap,
            } => {
                let want_native = match want_native_cap {
                    None => None,
                    Some(id) => match shared.caps.resolve(id, flow.id, CapRights::NATIVE) {
                        Ok(entry) => entry.cap.native_mask,
                        Err(e) => {
                            finish_failed(shared, *flow, e.to_string());
                            return;
                        }
                    },
                };
                match shared.caps.attenuate(
                    src_cap,
                    flow.id,
                    flow.id,
                    want_rights,
                    want_native.as_ref(),
                ) {
                    Ok(new_id) => {
                        let Some(f) =
                            resume_or_fail(shared, flow, dest_reg, Value::Cap(new_id))
                        else {
                            return;
                        };
                        flow = f;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
        }
    }
}

enum DeliverStatus {
    Ok,
    Full,
    Gone,
    Unavailable,
}

fn kill_outcome_message(reason: FlowExitReason) -> String {
    match reason {
        FlowExitReason::Link => format!("linked exit ({reason})"),
        FlowExitReason::Killed => format!("killed ({reason})"),
        other => format!("exit ({other})"),
    }
}

fn admit_waiting_on(
    mailbox: &std::sync::Arc<super::mailbox::Mailbox>,
    shared: &Arc<Shared>,
    local: &LocalDeque<Box<Flow>>,
) {
    match mailbox.admit_waiting_sender() {
        Ok(AdmitSender::Woken(mut sender)) => {
            if let Err(e) = shared.waiting_send_at.remove(sender.id) {
                report_fault(e);
            }
            match sender.pending_send.take() {
                Some(PendingSend::Ask {
                    dest_reg,
                    expect_request_id,
                    expect_sender,
                    timeout,
                }) => {
                    let filter = WaitFilter::Correlation {
                        expect_request_id,
                        expect_sender: Some(expect_sender),
                    };
                    park_ask(
                        shared,
                        sender,
                        dest_reg,
                        timeout,
                        filter,
                        flow_id_from_u64(expect_sender),
                    );
                }
                _ => {
                    local.push(sender);
                    wake_workers(shared);
                }
            }
        }
        Ok(AdmitSender::Idle) => {}
        Ok(AdmitSender::Undeliverable(sender)) => {
            if let Err(e) = shared.waiting_send_at.remove(sender.id) {
                report_fault(e);
            }
            finish_failed(
                shared,
                *sender,
                "hop larger than target mailbox budget".into(),
            );
        }
        Err(e) => report_fault(e),
    }
}

fn deliver(
    shared: &Arc<Shared>,
    local: &LocalDeque<Box<Flow>>,
    target: FlowId,
    message: Value,
) -> DeliverStatus {
    let mailbox = match shared.directory.lookup(target) {
        Ok(Some(m)) => m,
        Ok(None) => return DeliverStatus::Gone,
        Err(e) => {
            report_fault(e);
            return DeliverStatus::Unavailable;
        }
    };
    match mailbox.push(message.clone()) {
        Ok(Ok(Delivery::Queued | Delivery::QueuedDropOldest)) => {
            log::debug(format!("deliver queued → flow#{target} msg={message}"));
            DeliverStatus::Ok
        }
        Ok(Ok(Delivery::DroppedNewest)) => {
            log::debug(format!(
                "deliver drop-newest → flow#{target} msg={message}"
            ));
            DeliverStatus::Ok
        }
        Ok(Ok(Delivery::Handoff(flow))) => {
            log::debug(format!(
                "deliver handoff → flow#{} msg={}",
                flow.id.as_u64(),
                message
            ));
            if let Err(e) = shared.ask_waits.remove_asker(flow.id) {
                report_fault(e);
            }
            match flow.last_receive_dest {
                Some(dest) => {
                    let Some(flow) = resume_or_fail(shared, flow, dest, message) else {
                        return DeliverStatus::Ok;
                    };
                    flow.metrics
                        .messages_received
                        .fetch_add(1, Ordering::Relaxed);
                    local.push(flow);
                    wake_workers(shared);
                    DeliverStatus::Ok
                }
                None => {
                    finish_failed(shared, *flow, "handoff missing dest register".into());
                    DeliverStatus::Ok
                }
            }
        }
        Ok(Err(full)) => {
            let reason = full.reason();
            log::info(format!(
                "deliver rejected (mailbox full: {reason}) → flow#{target} msg={message}"
            ));
            DeliverStatus::Full
        }
        Err(e) => {
            report_fault(e);
            DeliverStatus::Unavailable
        }
    }
}

fn park_ask(
    shared: &Arc<Shared>,
    flow: Box<Flow>,
    dest_reg: u8,
    timeout: Option<Duration>,
    filter: WaitFilter,
    target: FlowId,
) {
    let asker = flow.id;
    let rid = match filter {
        WaitFilter::Correlation {
            expect_request_id, ..
        } => expect_request_id,
        WaitFilter::TaggedCorrelation {
            expect_request_id, ..
        } => expect_request_id,
        _ => 0,
    };
    match shared.ask_waits.insert(asker, target, rid) {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            finish_failed(shared, *flow, DuplicateAsk.to_string());
            return;
        }
        Err(e) => {
            report_fault(e);
            finish_failed(shared, *flow, "ask-wait index".into());
            return;
        }
    }
    let mailbox = flow.mailbox.clone();
    match mailbox.park_filter(flow, filter) {
        Ok(Ok(epoch)) => {
            // Close insert→park race with `wake_orphaned_asks`: finalize may
            // have already `take_waiters_of`'d us while `take_parked` was
            // still `None`. Membership gone ⇒ self-wake with TAG_SYS_EXIT.
            // Still indexed but target unregistered ⇒ same (insert after
            // finalize's take_waiters missed us). Stale timeouts are safe
            // via WaitEpoch if finalize wins the race after this check.
            let still_waiting = matches!(
                shared.ask_waits.target_of(asker),
                Ok(Some(t)) if t == target
            );
            let target_alive = matches!(shared.directory.lookup(target), Ok(Some(_)));
            if !still_waiting || !target_alive {
                resume_ask_target_gone(shared, &mailbox, asker, dest_reg, target);
                return;
            }
            if let Some(delay) = timeout {
                shared
                    .timer
                    .schedule_receive_timeout(delay, asker, mailbox, dest_reg, epoch);
            }
        }
        Ok(Err(mut flow)) => {
            let _ = shared.ask_waits.remove_asker(asker);
            if let Some(msg) = flow.pending_message.take() {
                let Some(f) = resume_or_fail(shared, flow, dest_reg, msg) else {
                    return;
                };
                flow = f;
                flow.metrics
                    .messages_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            shared.injector.push(flow);
            wake_workers(shared);
        }
        Err((e, flow)) => {
            let _ = shared.ask_waits.remove_asker(asker);
            report_fault(e);
            finish_failed(shared, *flow, "mailbox unavailable".into());
        }
    }
}

/// Unpark an Ask whose target exited (or whose ask-wait membership was
/// cleared) before / while parking, and resume with `TAG_SYS_EXIT`.
fn resume_ask_target_gone(
    shared: &Arc<Shared>,
    mailbox: &Mailbox,
    asker: FlowId,
    dest_reg: u8,
    target: FlowId,
) {
    if let Ok(Some(parked)) = mailbox.take_parked() {
        let _ = shared.ask_waits.remove_asker(asker);
        let hop = Value::Message(Message::linked_exit(
            target.as_u64(),
            FlowExitReason::Fault.as_u64(),
        ));
        let Some(parked) = resume_or_fail(shared, parked, dest_reg, hop) else {
            return;
        };
        parked
            .metrics
            .messages_received
            .fetch_add(1, Ordering::Relaxed);
        shared.injector.push(parked);
        wake_workers(shared);
    }
}

fn park_on_mailbox(
    shared: &Arc<Shared>,
    flow: Box<Flow>,
    dest_reg: u8,
    timeout: Option<Duration>,
    filter: WaitFilter,
) {
    let mailbox = flow.mailbox.clone();
    let pid = flow.id;

    match mailbox.park_filter(flow, filter) {
        Ok(Ok(epoch)) => {
            if let Some(delay) = timeout {
                shared
                    .timer
                    .schedule_receive_timeout(delay, pid, mailbox, dest_reg, epoch);
            }
        }
        Ok(Err(mut flow)) => {
            if let Some(msg) = flow.pending_message.take() {
                let Some(f) = resume_or_fail(shared, flow, dest_reg, msg) else {
                    return;
                };
                flow = f;
                flow.metrics
                    .messages_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            shared.injector.push(flow);
            wake_workers(shared);
        }
        Err((e, flow)) => {
            report_fault(e);
            finish_failed(shared, *flow, "mailbox unavailable".into());
        }
    }
}

fn finish_ok(shared: &Shared, flow: Flow, value: Value) {
    finalize_flow(
        shared,
        flow,
        FlowOutcome::Completed(value),
        FlowExitReason::Normal,
    );
}

fn finish_failed(shared: &Shared, flow: Flow, msg: String) {
    finalize_flow(shared, flow, FlowOutcome::Failed(msg), FlowExitReason::Fault);
}

fn park_waiting_send(
    shared: &Shared,
    mut flow: Box<Flow>,
    target: FlowId,
    stamped: Value,
    pending: PendingSend,
) {
    flow.pending_send = Some(pending);
    if let Err(e) = shared.waiting_send_at.insert(flow.id, target) {
        report_fault(e);
        finalize_flow(
            shared,
            *flow,
            FlowOutcome::Failed("waiting-send index".into()),
            FlowExitReason::Fault,
        );
        return;
    }
    let Some(mailbox) = (match shared.directory.lookup(target) {
        Ok(m) => m,
        Err(e) => {
            report_fault(e);
            let _ = shared.waiting_send_at.remove(flow.id);
            finalize_flow(
                shared,
                *flow,
                FlowOutcome::Failed("send target gone".into()),
                FlowExitReason::Fault,
            );
            return;
        }
    }) else {
        let _ = shared.waiting_send_at.remove(flow.id);
        finalize_flow(
            shared,
            *flow,
            FlowOutcome::Failed("send target gone".into()),
            FlowExitReason::Fault,
        );
        return;
    };
    match mailbox.park_sender(flow, stamped) {
        ParkSender::Parked => {}
        ParkSender::Closed(flow) => {
            let _ = shared.waiting_send_at.remove(flow.id);
            finalize_flow(
                shared,
                *flow,
                FlowOutcome::Failed("send target gone".into()),
                FlowExitReason::Fault,
            );
        }
        ParkSender::Undeliverable(flow) => {
            let _ = shared.waiting_send_at.remove(flow.id);
            finalize_flow(
                shared,
                *flow,
                FlowOutcome::Failed("hop larger than target mailbox budget".into()),
                FlowExitReason::Fault,
            );
        }
    }
}
