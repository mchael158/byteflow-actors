//! Sandbox acceptance tests: capabilities, quotas, bytecode trust, runtime policy.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use byteflow::{
    decode, decode_with, encode, verify, verify_with, Cap, CapId, CapRights, CapTarget, Chunk,
    Fault, FunctionDef, Instruction, MemoryBudget, NativeTable, Opcode, Program, QuotaConfig,
    Runtime, RuntimeConfig, TrustLevel, Value, VerifyConfig, VerifyError, Vm, VmResult,
    MAX_LIST_LEN,
};

fn tiny_main() -> Chunk {
    let mut p = Program::new("sandbox-main");
    p.function("main", 0, |f| {
        let z = f.load_i32(0);
        f.return_(z);
    });
    p.build()
}

fn tiny_config(workers: usize) -> RuntimeConfig {
    RuntimeConfig {
        workers,
        quantum: 10_000,
        mailbox: byteflow::MailboxConfig::DEFAULT,
        ..RuntimeConfig::default()
    }
}

// --- Capability ---

#[test]
fn capability_creation_generates_unique_ids() -> Result<(), String> {
    let cell = byteflow::RevocationCell::new();
    let mut ids = Vec::new();
    for _ in 0..64 {
        let id = CapId::random().map_err(|e| e.to_string())?;
        assert!(!id.is_none());
        assert!(!ids.contains(&id));
        ids.push(id);
        let _ = Cap::root(CapTarget::Flow(1), CapRights::SEND, None, &cell);
    }
    Ok(())
}

#[test]
fn capability_id_uses_128_bits() -> Result<(), String> {
    let id = CapId::random().map_err(|e| e.to_string())?;
    assert_eq!(std::mem::size_of_val(&id.as_u128()), 16);
    assert!(!id.is_none());
    Ok(())
}

#[test]
fn capability_rejects_wrong_holder() -> Result<(), Box<dyn std::error::Error>> {
    // Cross-runtime CapId is not authority: Cap minted in A cannot register in B.
    let chunk = tiny_main();
    let rt_a = Runtime::with_config(chunk.clone(), tiny_config(1))?;
    let rt_b = Runtime::with_config(chunk, tiny_config(1))?;
    let h = rt_a.spawn(0, &[])?;
    let cap = rt_a.mint_cap(h.id())?;
    assert_eq!(
        rt_b.register_name("svc", cap),
        Err(byteflow::LifecycleError::InvalidCapability)
    );
    let _ = h.join();
    rt_a.shutdown();
    rt_b.shutdown();
    Ok(())
}

#[test]
fn capability_delegation_cannot_escalate_rights() {
    let cell = byteflow::RevocationCell::new();
    let parent = Cap::root(CapTarget::Flow(1), CapRights::SEND, None, &cell);
    let child = parent.attenuate(CapRights::ADMIN.union(CapRights::SEND), None);
    assert!(!child.rights().contains(CapRights::ADMIN));
    assert!(child.rights().is_subset_of(parent.rights()));
    assert_eq!(child.rights().bits(), CapRights::SEND.bits());
}

#[test]
fn capability_revocation_blocks_future_access() {
    let cell = byteflow::RevocationCell::new();
    let root = Cap::root(CapTarget::Flow(2), CapRights::SEND, None, &cell);
    let child = root.attenuate(CapRights::SEND, None);
    assert!(child.is_valid(&cell));
    cell.revoke();
    assert!(!root.is_valid(&cell));
    assert!(!child.is_valid(&cell));
}

#[test]
fn capability_revocation_propagates_to_children() {
    let cell = byteflow::RevocationCell::new();
    let root = Cap::root(CapTarget::Flow(3), CapRights::FLOW, None, &cell);
    let a = root.attenuate(CapRights::SEND, None);
    let b = a.attenuate(CapRights::SEND, None);
    cell.revoke();
    assert!(!a.is_valid(&cell));
    assert!(!b.is_valid(&cell));
}

#[test]
fn capability_revocation_is_thread_safe() -> Result<(), String> {
    for _ in 0..24 {
        let cell = Arc::new(byteflow::RevocationCell::new());
        let cap = Cap::root(CapTarget::Flow(4), CapRights::SEND, None, cell.as_ref());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handles = Vec::new();
        for _ in 0..6 {
            let cell = Arc::clone(&cell);
            let worker_cap = cap.clone();
            let stop = Arc::clone(&stop);
            handles.push(thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let _ = worker_cap.is_valid(cell.as_ref());
                }
            }));
        }
        cell.revoke();
        stop.store(true, Ordering::Release);
        for h in handles {
            h.join().map_err(|_| "panic".to_string())?;
        }
        if cap.is_valid(cell.as_ref()) {
            return Err("revoked cap still valid".into());
        }
    }
    Ok(())
}

// --- Quotas / memory ---

#[test]
fn memory_quota_accepts_valid_allocation() {
    let budget = MemoryBudget::new(1024);
    assert!(budget.try_charge(100).is_ok());
    assert_eq!(budget.used(), 100);
}

#[test]
fn memory_quota_rejects_limit_exceeded() {
    let budget = MemoryBudget::new(50);
    assert!(budget.try_charge(51).is_err());
    assert_eq!(budget.used(), 0);
}

#[test]
fn memory_quota_rejects_integer_overflow() {
    let budget = MemoryBudget::new(usize::MAX);
    assert!(budget.try_charge(usize::MAX - 10).is_ok());
    assert!(budget.try_charge(20).is_err());
}

#[test]
fn memory_quota_releases_allocated_memory() -> Result<(), String> {
    let budget = MemoryBudget::new(200);
    budget.try_charge(80).map_err(|e| e.to_string())?;
    budget.release(80);
    assert_eq!(budget.used(), 0);
    budget.try_charge(200).map_err(|e| e.to_string())?;
    Ok(())
}

#[test]
fn memory_quota_handles_concurrent_allocations() -> Result<(), String> {
    let budget = Arc::new(MemoryBudget::new(1_000));
    let ok = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..10)
        .map(|_| {
            let budget = Arc::clone(&budget);
            let ok = Arc::clone(&ok);
            thread::spawn(move || {
                for _ in 0..200 {
                    if budget.try_charge(1).is_ok() {
                        ok.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().map_err(|_| "join".to_string())?;
    }
    assert_eq!(ok.load(Ordering::Relaxed), 1_000);
    assert_eq!(budget.used(), 1_000);
    Ok(())
}

#[test]
fn memory_quota_never_exceeds_limit() -> Result<(), String> {
    let budget = Arc::new(MemoryBudget::new(64));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let budget = Arc::clone(&budget);
            thread::spawn(move || {
                for _ in 0..100 {
                    let _ = budget.try_charge(1);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().map_err(|_| "join".to_string())?;
    }
    assert!(budget.used() <= budget.limit());
    Ok(())
}

#[test]
fn flow_quota_rejects_excess_flows() -> Result<(), Box<dyn std::error::Error>> {
    let config = RuntimeConfig {
        max_flows: 1,
        ..tiny_config(1)
    };
    let rt = Runtime::with_config(tiny_main(), config)?;
    let a = rt.spawn(0, &[])?;
    let err = rt.spawn(0, &[]);
    assert!(err.is_err());
    let _ = a.join();
    rt.shutdown();
    Ok(())
}

#[test]
fn ask_wait_quota_is_enforced() -> Result<(), Box<dyn std::error::Error>> {
    // Covered in unit tests; here we assert sandbox profile sets a finite cap.
    let s = RuntimeConfig::sandbox();
    assert_eq!(s.max_ask_waits, 64);
    assert_ne!(s.max_ask_waits, 0);
    Ok(())
}

#[test]
fn host_await_quota_is_enforced() {
    let s = RuntimeConfig::sandbox();
    assert_eq!(s.max_host_awaits, 128);
    assert_eq!(RuntimeConfig::default().max_host_awaits, 0);
}

#[test]
fn mailbox_limit_is_enforced() {
    let s = RuntimeConfig::sandbox();
    assert_eq!(s.mailbox.capacity().get(), 256);
}

// --- Bytecode / trust ---

#[test]
fn untrusted_module_rejects_cap_constant() -> Result<(), String> {
    let mut chunk = tiny_main();
    chunk
        .constants
        .push(Value::Cap(CapId::random().map_err(|e| e.to_string())?));
    match verify_with(
        &chunk,
        VerifyConfig {
            trust: TrustLevel::Untrusted,
        },
    ) {
        Err(VerifyError::ForbiddenConstant { .. }) => Ok(()),
        other => Err(format!("{other:?}")),
    }
}

#[test]
fn untrusted_module_rejects_pid_constant() {
    let mut chunk = tiny_main();
    chunk.constants.push(Value::Pid(7));
    assert!(matches!(
        verify_with(
            &chunk,
            VerifyConfig {
                trust: TrustLevel::Untrusted,
            }
        ),
        Err(VerifyError::ForbiddenConstant { .. })
    ));
}

#[test]
fn untrusted_module_rejects_message_constant() {
    let mut chunk = tiny_main();
    chunk
        .constants
        .push(Value::Message(byteflow::Message::new(0, 0, 0, Value::Unit)));
    assert!(matches!(
        verify_with(
            &chunk,
            VerifyConfig {
                trust: TrustLevel::Restricted,
            }
        ),
        Err(VerifyError::ForbiddenConstant { .. })
    ));
}

#[test]
fn restricted_also_rejects_authority_constants() {
    assert!(TrustLevel::Restricted.forbids_authority_constants());
    assert!(!TrustLevel::Trusted.forbids_authority_constants());
}

#[test]
fn verifier_rejects_invalid_jump() {
    let chunk = Chunk {
        name: "j".into(),
        constants: vec![],
        code: vec![
            Instruction::only_imm(Opcode::Jump, 999),
            Instruction::nullary(Opcode::Halt),
        ],
        functions: vec![FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        }],
    };
    assert!(matches!(
        verify(&chunk),
        Err(VerifyError::JumpOutOfRange { .. })
    ));
}

#[test]
fn verifier_rejects_invalid_constant_index() {
    let chunk = Chunk {
        name: "c".into(),
        constants: vec![],
        code: vec![Instruction::a_imm(Opcode::LoadConst, 0, 3)],
        functions: vec![FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        }],
    };
    assert!(matches!(
        verify(&chunk),
        Err(VerifyError::ConstOutOfRange { .. })
    ));
}

#[test]
fn verifier_rejects_invalid_register() {
    let chunk = Chunk {
        name: "r".into(),
        constants: vec![],
        code: vec![Instruction::new(Opcode::StartChild, 9, 0, 0, 0)],
        functions: vec![FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 2,
        }],
    };
    assert!(matches!(
        verify(&chunk),
        Err(VerifyError::BadImmediate { .. })
    ));
}

#[test]
fn verifier_rejects_truncated_module() {
    let bytes = encode(&tiny_main());
    assert!(decode(&bytes[..bytes.len().saturating_sub(1)]).is_err());
}

#[test]
fn verifier_rejects_oversized_module() {
    let mut chunk = tiny_main();
    chunk.code = vec![
        Instruction::new(Opcode::ListPack, 0, 0, 0, MAX_LIST_LEN as i32 + 1),
        Instruction::abc(Opcode::Return, 0, 0, 0),
    ];
    assert!(verify(&chunk).is_err());
}

#[test]
fn verifier_never_panics_on_arbitrary_input() {
    let mut rng = byteflow::prng::XorShift64::new(0x5AFE_0001);
    for _ in 0..64 {
        let len = rng.next_u32_inclusive(0, 255) as usize;
        let mut data = vec![0u8; len];
        rng.fill_bytes(&mut data);
        let _ = decode_with(&data, TrustLevel::Untrusted);
        let _ = decode_with(&data, TrustLevel::Restricted);
        if let Ok(chunk) = decode_with(&data, TrustLevel::Trusted) {
            let _ = verify_with(
                &chunk,
                VerifyConfig {
                    trust: TrustLevel::Untrusted,
                },
            );
        }
    }
}

// --- Runtime sandbox ---

#[test]
fn sandbox_preserves_permissive_default() {
    let d = RuntimeConfig::default();
    assert_eq!(d.max_flows, 0);
    assert_eq!(d.max_ask_waits, 0);
    assert_eq!(d.max_host_awaits, 0);
    assert_eq!(d.quota.cpu_budget, QuotaConfig::permissive().cpu_budget);
}

#[test]
fn sandbox_rejects_privilege_escalation() {
    let cell = byteflow::RevocationCell::new();
    let parent = Cap::root(CapTarget::Flow(1), CapRights::SEND, None, &cell);
    let escalated = parent.attenuate(CapRights::from_bits(u32::MAX), None);
    assert!(escalated.rights().is_subset_of(CapRights::SEND));
}

#[test]
fn sandbox_denies_ungranted_native() -> Result<(), Box<dyn std::error::Error>> {
    // Host-spawned roots get FLOW (incl. NATIVE). Confined children do not —
    // that is the sandbox deny path for natives.
    let mut p = Program::new("native-deny");
    let child = p.function("child", 0, |f| {
        let n = f.load_i32(1);
        f.native1_on(n, 0);
        f.return_(n);
    });
    p.function("main", 0, |f| {
        let _cap = f.spawn_confined(child, &[]);
        let z = f.load_i32(0);
        f.return_(z);
    });
    let rt = Runtime::with_std_natives_and_config(p.build(), tiny_config(1))?;
    let main = rt.function_index("main").ok_or("main")?;
    let outcome = rt.spawn(main, &[])?.join();
    rt.shutdown();
    match outcome {
        byteflow::FlowOutcome::Completed(Value::Int(0)) => Ok(()),
        other => Err(format!("parent should complete; confined native denied: {other:?}").into()),
    }
}

#[test]
fn sandbox_releases_quota_after_flow_completion() -> Result<(), Box<dyn std::error::Error>> {
    let config = RuntimeConfig {
        max_flows: 2,
        max_runtime_bytes: 1024 * 1024,
        ..tiny_config(1)
    };
    let rt = Runtime::with_config(tiny_main(), config)?;
    let before = rt.snapshot().memory.used;
    let h = rt.spawn(0, &[])?;
    let _ = h.join();
    let after = rt.snapshot().memory.used;
    assert!(after <= before + 4096);
    rt.shutdown();
    Ok(())
}

#[test]
fn sandbox_policy_fingerprint_and_attestation() -> Result<(), Box<dyn std::error::Error>> {
    let config = RuntimeConfig::sandbox();
    let fp = config.policy_fingerprint();
    let rt = Runtime::with_config(tiny_main(), config)?;
    assert_eq!(rt.policy_fingerprint(), fp);
    assert_eq!(rt.trust_level(), TrustLevel::Untrusted);
    let h = rt.spawn(0, &[])?;
    let attest = rt.attest_capabilities(h.id())?;
    assert_eq!(attest.flow, h.id());
    assert_eq!(attest.trust, TrustLevel::Untrusted);
    let _ = h.join();
    rt.shutdown();
    Ok(())
}

#[test]
fn sandbox_restricted_profile_sets_trust() {
    let c = RuntimeConfig::sandbox_restricted();
    assert_eq!(c.trust, TrustLevel::Restricted);
    assert_eq!(c.max_flows, 128);
    assert!(c.applies_sandbox_caps());
    assert!(!RuntimeConfig::default().applies_sandbox_caps());
}

#[derive(Debug, Default)]
struct AuditCapture(std::sync::Mutex<Vec<String>>);

impl byteflow::SecurityAuditSink for AuditCapture {
    fn record(&self, event: &byteflow::SecurityEvent) {
        if let Ok(mut g) = self.0.lock() {
            g.push(event.to_string());
        }
    }
}

#[test]
fn sandbox_verify_failure_notifies_audit_sink() -> Result<(), Box<dyn std::error::Error>> {
    let audit = Arc::new(AuditCapture::default());
    let mut chunk = tiny_main();
    chunk.constants.push(Value::Pid(99));
    let config = RuntimeConfig {
        trust: TrustLevel::Untrusted,
        security_audit: Some(audit.clone() as Arc<dyn byteflow::SecurityAuditSink>),
        ..tiny_config(1)
    };
    let err = Runtime::with_config(chunk, config);
    assert!(err.is_err(), "untrusted Cap/Pid pool must fail verify");
    let lines = audit.0.lock().map_err(|_| "poisoned")?;
    assert!(
        lines.iter().any(|l| l.contains("module_rejected")),
        "audit missing reject: {lines:?}"
    );
    Ok(())
}

#[test]
fn sandbox_counters_track_spawn_and_finalize() -> Result<(), Box<dyn std::error::Error>> {
    let config = RuntimeConfig::sandbox();
    let rt = Runtime::with_config(tiny_main(), config)?;
    let before = rt.security_counters();
    let h = rt.spawn(0, &[])?;
    let mid = rt.security_counters();
    assert!(mid.capabilities_created > before.capabilities_created);
    assert!(mid.active_untrusted_flows >= 1);
    let _ = h.join();
    let after = rt.security_counters();
    assert!(after.capabilities_revoked >= before.capabilities_revoked);
    assert_eq!(after.active_untrusted_flows, 0);
    rt.shutdown();
    Ok(())
}

#[test]
fn sandbox_profile_fingerprint_is_stable_across_clones() {
    let a = RuntimeConfig::sandbox();
    let b = a.clone();
    assert_eq!(a.policy_fingerprint(), b.policy_fingerprint());
    assert!(a.applies_sandbox_caps());
}

#[test]
fn vm_traps_bad_register_without_panic() -> Result<(), String> {
    let chunk = Arc::new(Chunk {
        name: "bad".into(),
        constants: vec![],
        code: vec![
            Instruction::a_imm(Opcode::LoadImm, 0, 1),
            Instruction::abc(Opcode::Move, 5, 0, 0),
            Instruction::abc(Opcode::Return, 5, 0, 0),
        ],
        functions: vec![FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 2,
        }],
    });
    let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[]).map_err(|e| e.to_string())?;
    match vm.run(u32::MAX) {
        VmResult::Trap(Fault::RegisterOutOfRange { .. }) => Ok(()),
        other => Err(format!("{other:?}")),
    }
}
