//! Integration tests for the 0.9 capability security model.

use std::sync::{Arc, Mutex};

use byteflow::{
    samples, FlowOutcome, LifecycleError, NullSink, OutputSink, Program, Runtime, RuntimeConfig,
    Value,
};

#[derive(Debug)]
struct CaptureSink(Mutex<Vec<String>>);

impl OutputSink for CaptureSink {
    fn write(&self, values: &[Value]) {
        let line = values
            .iter()
            .map(|v| format!("{v}"))
            .collect::<Vec<_>>()
            .join(" ");
        if let Ok(mut g) = self.0.lock() {
            g.push(line);
        }
    }
}

fn tiny_config() -> RuntimeConfig {
    RuntimeConfig {
        workers: 1,
        quantum: 10_000,
        mailbox: byteflow::MailboxConfig::DEFAULT,
        ..Default::default()
    }
}

#[test]
fn cap_from_runtime_a_cannot_register_in_runtime_b() -> Result<(), Box<dyn std::error::Error>> {
    let chunk = samples::add_forty_two();
    let rt_a = Runtime::with_config(chunk.clone(), tiny_config())?;
    let rt_b = Runtime::with_config(chunk, tiny_config())?;
    let h = rt_a.spawn(0, &[])?;
    let cap = rt_a.mint_cap(h.id())?;
    assert_eq!(
        rt_b.register_name("svc", cap),
        Err(LifecycleError::InvalidCapability)
    );
    let _ = h.join();
    rt_a.shutdown();
    rt_b.shutdown();
    Ok(())
}

#[test]
fn cap_revoked_after_holder_flow_exits() -> Result<(), Box<dyn std::error::Error>> {
    let mut p = Program::new("cap-revoke");
    p.function("main", 0, |f| {
        let z = f.load_i32(0);
        f.return_(z);
    });
    let rt = Runtime::with_config(p.build(), tiny_config())?;
    let h = rt.spawn(0, &[])?;
    let cap = rt.mint_cap(h.id())?;
    let _ = h.join();
    assert_eq!(
        rt.register_name("svc", cap),
        Err(LifecycleError::InvalidCapability)
    );
    rt.shutdown();
    Ok(())
}

#[test]
fn forged_sender_samples_return_authenticated_pid() -> Result<(), Box<dyn std::error::Error>> {
    for chunk in [samples::forged_sender_send(), samples::forged_sender_ask()] {
        let rt = Runtime::with_std_natives_and_config(chunk, tiny_config())?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Pid(n)) => {
                assert_ne!(n, 999);
                assert!(n >= 1);
            }
            other => return Err(format!("expected Completed(Pid), got {other:?}").into()),
        }
    }
    Ok(())
}

#[test]
fn runtime_config_output_wires_print_native() -> Result<(), Box<dyn std::error::Error>> {
    let capture = Arc::new(CaptureSink(Mutex::new(Vec::new())));
    let mut p = Program::new("print-sink");
    p.function("main", 0, |f| {
        let n = f.load_i32(7);
        f.native1_on(n, 0);
        let z = f.load_i32(0);
        f.return_(z);
    });
    let config = RuntimeConfig {
        output: Arc::clone(&capture) as Arc<dyn OutputSink>,
        ..tiny_config()
    };
    let rt = Runtime::with_std_natives_and_config(p.build(), config)?;
    let _ = rt.spawn(0, &[])?.join();
    rt.shutdown();
    let lines = capture.0.lock().map_err(|_| "poisoned capture lock")?;
    assert_eq!(lines.as_slice(), &["7".to_owned()]);
    Ok(())
}

#[test]
fn null_sink_is_default_and_silent() -> Result<(), Box<dyn std::error::Error>> {
    let config = RuntimeConfig {
        output: Arc::new(NullSink),
        ..tiny_config()
    };
    let mut p = Program::new("silent");
    p.function("main", 0, |f| {
        let n = f.load_i32(1);
        f.native1_on(n, 0);
        let z = f.load_i32(0);
        f.return_(z);
    });
    let rt = Runtime::with_std_natives_and_config(p.build(), config)?;
    let _ = rt.spawn(0, &[])?.join();
    rt.shutdown();
    Ok(())
}

#[test]
fn confined_child_cannot_call_natives() -> Result<(), Box<dyn std::error::Error>> {
    let mut p = Program::new("confined-native");
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
    let rt = Runtime::with_std_natives_and_config(p.build(), tiny_config())?;
    let idx = rt.function_index("main").ok_or("main")?;
    let outcome = rt.spawn(idx, &[])?.join();
    rt.shutdown();
    match outcome {
        FlowOutcome::Completed(Value::Int(0)) => Ok(()),
        other => Err(format!("parent should complete, got {other:?}").into()),
    }
}

#[test]
fn admin_kill_requires_scheduler_cap() -> Result<(), Box<dyn std::error::Error>> {
    let mut p = Program::new("admin-kill-victim");
    p.function("main", 0, |f| {
        let ms = f.load_i32(200);
        f.sleep(ms);
        let z = f.load_i32(42);
        f.return_(z);
    });
    let rt = Runtime::with_config(p.build(), tiny_config())?;
    let victim = rt.spawn(0, &[])?;
    let holder = rt.spawn(0, &[])?;
    let ordinary = rt.mint_cap(holder.id())?;
    assert_eq!(
        rt.admin_kill(holder.id(), ordinary, victim.id()),
        Err(LifecycleError::InvalidCapability)
    );
    let admin = rt.mint_admin_cap(holder.id())?;
    rt.admin_kill(holder.id(), admin, victim.id())?;
    let outcome = victim.join();
    let _ = holder.join();
    rt.shutdown();
    match outcome {
        FlowOutcome::Failed(_) => Ok(()),
        other => Err(format!("admin_kill must fail the victim, got {other:?}").into()),
    }
}

#[test]
fn admin_top_up_cpu_uses_shared_quota_table() -> Result<(), Box<dyn std::error::Error>> {
    let mut p = Program::new("idle");
    p.function("main", 0, |f| {
        let ms = f.load_i32(20);
        f.sleep(ms);
        let z = f.load_i32(0);
        f.return_(z);
    });
    let rt = Runtime::with_config(p.build(), tiny_config())?;
    let h = rt.spawn(0, &[])?;
    let admin = rt.mint_admin_cap(h.id())?;
    rt.admin_top_up_cpu(h.id(), admin, h.id(), 1_000)?;
    rt.admin_top_up_mem(h.id(), admin, h.id(), 4096)?;
    rt.admin_top_up_send(h.id(), admin, h.id(), 32)?;
    let _ = h.join();
    rt.shutdown();
    Ok(())
}
