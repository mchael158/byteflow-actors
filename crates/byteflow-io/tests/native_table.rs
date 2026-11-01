use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use byteflow::{NullSink, Value};
use byteflow_io::{
    build_native_table, native_index, IoCommand, IoDriver, IoDriverError, IoErrorKind,
};

#[derive(Debug, Default)]
struct TestDriver {
    next_id: AtomicU64,
    commands: Mutex<Vec<IoCommand>>,
}

impl IoDriver for TestDriver {
    fn try_submit(&self, command: IoCommand) -> Result<u64, IoDriverError> {
        match self.commands.lock() {
            Ok(mut commands) => commands.push(command),
            Err(_) => return Err(IoDriverError::poisoned("test command queue poisoned")),
        }
        Ok(self.next_id.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

#[test]
fn standard_native_slots_are_preserved_and_io_slots_are_appended() {
    let driver = Arc::new(TestDriver::default());
    let output = Arc::new(NullSink);
    let table = build_native_table(driver, output).expect("native table should build");

    assert_eq!(table.index_of("print"), Some(native_index::PRINT));
    assert_eq!(table.index_of("now_ms"), Some(native_index::NOW_MS));
    assert_eq!(table.index_of("make_msg"), Some(native_index::MAKE_MSG));
    assert_eq!(table.index_of("msg_sender"), Some(native_index::MSG_SENDER));
    assert_eq!(
        table.index_of("msg_request_id"),
        Some(native_index::MSG_REQUEST_ID)
    );
    assert_eq!(table.index_of("msg_tag"), Some(native_index::MSG_TAG));
    assert_eq!(
        table.index_of("msg_payload"),
        Some(native_index::MSG_PAYLOAD)
    );
    assert_eq!(
        table.index_of("msg_reply_cap"),
        Some(native_index::MSG_REPLY_CAP)
    );

    assert_eq!(table.index_of("io_gpio_write"), Some(8));
    assert_eq!(table.index_of("io_mqtt_publish_fast"), Some(9));
    assert_eq!(table.index_of("io_uart_write_fast"), Some(10));
    assert_eq!(table.len(), 11);
}

#[test]
fn gpio_native_only_enqueues_and_returns_a_command_id() {
    let driver = Arc::new(TestDriver::default());
    let output = Arc::new(NullSink);
    let table = build_native_table(driver.clone(), output).expect("native table should build");
    let gpio = table
        .get(native_index::GPIO_WRITE)
        .expect("GPIO native must be registered");

    let result = gpio(&[Value::Int(17), Value::Bool(true)]);
    assert!(matches!(result, Ok(Value::Int(1))));

    let commands = driver.commands.lock().expect("test mutex");
    assert_eq!(
        commands.as_slice(),
        &[IoCommand::GpioWrite {
            pin: 17,
            high: true
        }]
    );
}

#[test]
fn gpio_native_accepts_int_truthy_for_high() {
    let driver = Arc::new(TestDriver::default());
    let output = Arc::new(NullSink);
    let table = build_native_table(driver.clone(), output).expect("native table should build");
    let gpio = table
        .get(native_index::GPIO_WRITE)
        .expect("GPIO native must be registered");

    let result = gpio(&[Value::Int(3), Value::Int(1)]);
    assert!(matches!(result, Ok(Value::Int(1))));

    let commands = driver.commands.lock().expect("test mutex");
    assert_eq!(
        commands.as_slice(),
        &[IoCommand::GpioWrite { pin: 3, high: true }]
    );
}

#[test]
fn mqtt_native_rejects_invalid_qos_before_touching_driver() {
    let driver = Arc::new(TestDriver::default());
    let output = Arc::new(NullSink);
    let table = build_native_table(driver.clone(), output).expect("native table should build");
    let mqtt = table
        .get(native_index::MQTT_PUBLISH_FAST)
        .expect("MQTT native must be registered");

    let result = mqtt(&[
        Value::str("devices/washer-01/cmd"),
        Value::Int(1),
        Value::Int(3),
    ]);
    assert!(result.is_err());

    let commands = driver.commands.lock().expect("test mutex");
    assert!(commands.is_empty());
}

#[derive(Debug)]
struct NoopAwaitHandler;

impl byteflow_io::HostAwaitHandler for NoopAwaitHandler {
    fn execute(
        &self,
        _op: u32,
        _args: Value,
        _cancel: &byteflow_io::CancellationToken,
    ) -> Result<Value, IoDriverError> {
        Ok(Value::Unit)
    }
}

#[test]
fn queued_driver_is_bounded_and_returns_before_device_execution() {
    use byteflow_io::{IoCommandReceiver, QueuedIoDriver};

    let handler = Arc::new(NoopAwaitHandler);
    let (driver, receiver): (Arc<QueuedIoDriver>, IoCommandReceiver) =
        QueuedIoDriver::new(1, handler).expect("bounded driver should build");

    let first = driver
        .try_submit(IoCommand::GpioWrite { pin: 4, high: true })
        .expect("first command should fit");
    assert_eq!(first, 1);

    let second = driver.try_submit(IoCommand::GpioWrite {
        pin: 5,
        high: false,
    });
    assert_eq!(
        second
            .expect_err("second command must not wait for queue space")
            .kind(),
        IoErrorKind::QueueFull
    );
    assert_eq!(driver.metrics().snapshot().rejected_full, 1);

    let third = driver
        .try_submit(IoCommand::GpioWrite { pin: 6, high: true })
        .expect_err("queue still full");
    assert_eq!(third.kind(), IoErrorKind::QueueFull);

    let queued = receiver.try_recv().expect("first command should be queued");
    assert_eq!(queued.id, first);
    assert_eq!(queued.command, IoCommand::GpioWrite { pin: 4, high: true });

    let second_ok = driver
        .try_submit(IoCommand::GpioWrite { pin: 6, high: true })
        .expect("queue has capacity after first item is still in channel");
    assert_eq!(second_ok, 2);
}

#[test]
fn queued_driver_shutdown_rejects_further_submits() {
    use byteflow_io::QueuedIoDriver;

    let (driver, _receiver) =
        QueuedIoDriver::new(4, Arc::new(NoopAwaitHandler)).expect("driver builds");
    driver.shutdown().expect("shutdown");
    let err = driver.try_submit(IoCommand::Custom {
        op: 1,
        args: Value::Unit,
    });
    assert_eq!(err.expect_err("closed").kind(), IoErrorKind::QueueClosed);
    assert_eq!(driver.metrics().snapshot().rejected_closed, 1);
}

#[test]
fn queued_driver_accepts_custom_commands() {
    use byteflow_io::QueuedIoDriver;

    let (driver, receiver) =
        QueuedIoDriver::new(2, Arc::new(NoopAwaitHandler)).expect("driver builds");
    let id = driver
        .try_submit(IoCommand::Custom {
            op: 99,
            args: Value::Int(7),
        })
        .expect("custom enqueue");
    let queued = receiver.try_recv().expect("custom queued");
    assert_eq!(queued.id, id);
    assert_eq!(
        queued.command,
        IoCommand::Custom {
            op: 99,
            args: Value::Int(7)
        }
    );
}

#[derive(Debug)]
struct EchoAwaitDriver;

impl IoDriver for EchoAwaitDriver {
    fn try_submit(&self, _command: IoCommand) -> Result<u64, IoDriverError> {
        Err(IoDriverError::unsupported(
            "fast commands are not used by this test",
        ))
    }

    fn execute_host_await(
        &self,
        op: u32,
        args: Value,
        cancel: &byteflow_io::CancellationToken,
    ) -> Result<Value, IoDriverError> {
        if cancel.is_cancelled() {
            return Err(IoDriverError::cancelled("cancelled"));
        }
        if op != 77 {
            return Err(IoDriverError::unsupported("unexpected test op"));
        }
        Ok(args)
    }
}

#[test]
fn host_await_bridge_completes_a_waiting_flow() -> Result<(), Box<dyn std::error::Error>> {
    use byteflow::{FlowOutcome, Program, Runtime, RuntimeConfig, TAG_SYS_AWAIT};
    use byteflow_io::IoConfig;

    let driver = Arc::new(EchoAwaitDriver);
    let io = byteflow_io::ByteflowIo::new(driver, IoConfig::default())?;
    let bridge = io.host_await_bridge();

    let mut program = Program::new("host-await-integration-test");
    program.function("main", 0, |f| {
        let input = f.load_i32(42);
        let _ticket = f.host_await(77, input);
        let message = f.receive();
        f.return_(message);
    });

    let config = RuntimeConfig {
        host_await: Some(bridge),
        ..RuntimeConfig::default()
    };
    let runtime = Runtime::with_config(program.build(), config)?;
    let outcome = runtime.spawn(0, &[])?.join();
    runtime.shutdown();
    io.shutdown()?;

    assert!(matches!(
        outcome,
        FlowOutcome::Completed(Value::Message(ref message))
            if message.tag == TAG_SYS_AWAIT
                && matches!(message.payload.as_ref(), Value::Int(42))
    ));
    assert_eq!(io.metrics().snapshot().submitted, 1);
    assert_eq!(io.metrics().snapshot().completed_ok, 1);
    Ok(())
}

#[derive(Debug)]
struct HoldGate {
    started: Mutex<usize>,
    release: Mutex<bool>,
    cv: Condvar,
}

impl HoldGate {
    fn new() -> Self {
        Self {
            started: Mutex::new(0),
            release: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    fn wait_started(&self, n: usize) {
        let mut guard = self.started.lock().expect("started");
        while *guard < n {
            guard = self.cv.wait(guard).expect("wait started");
        }
    }

    fn mark_started(&self) {
        let mut guard = self.started.lock().expect("started");
        *guard += 1;
        self.cv.notify_all();
    }

    fn wait_release(&self, cancel: &byteflow_io::CancellationToken) -> Result<(), IoDriverError> {
        let mut guard = self
            .release
            .lock()
            .map_err(|_| IoDriverError::poisoned("poison"))?;
        while !*guard {
            if cancel.is_cancelled() {
                return Err(IoDriverError::cancelled("cancelled while held"));
            }
            let (next, _) = self
                .cv
                .wait_timeout(guard, Duration::from_millis(20))
                .map_err(|_| IoDriverError::other("wait"))?;
            guard = next;
        }
        Ok(())
    }

    fn release(&self) {
        let mut guard = self.release.lock().expect("release");
        *guard = true;
        self.cv.notify_all();
    }
}

#[derive(Debug)]
struct HoldingDriver {
    gate: Arc<HoldGate>,
}

impl IoDriver for HoldingDriver {
    fn try_submit(&self, _command: IoCommand) -> Result<u64, IoDriverError> {
        Err(IoDriverError::unsupported("unused"))
    }

    fn execute_host_await(
        &self,
        _op: u32,
        args: Value,
        cancel: &byteflow_io::CancellationToken,
    ) -> Result<Value, IoDriverError> {
        self.gate.mark_started();
        self.gate.wait_release(cancel)?;
        Ok(args)
    }
}

#[test]
fn host_await_queue_full_fails_without_blocking() -> Result<(), Box<dyn std::error::Error>> {
    use byteflow::{FlowOutcome, Program, Runtime};
    use byteflow_io::{ByteflowIo, IoConfig};

    let gate = Arc::new(HoldGate::new());
    let driver = Arc::new(HoldingDriver {
        gate: Arc::clone(&gate),
    });
    let io = ByteflowIo::new(
        driver,
        IoConfig {
            worker_threads: 1,
            queue_capacity: 1,
            ..IoConfig::default()
        },
    )?;

    let mut program = Program::new("ha-full");
    let waiter = program.function("waiter", 0, |f| {
        let args = f.load_i32(1);
        let _ticket = f.host_await(1, args);
        let msg = f.receive();
        f.return_(msg);
    });

    let runtime = Runtime::with_natives_and_config(
        program.build(),
        byteflow::std_native_table(),
        io.runtime_config(Arc::new(NullSink)),
    )?;

    let first = runtime.spawn(waiter, &[])?;
    gate.wait_started(1);

    // Fill the single queue slot while the worker is busy.
    let second = runtime.spawn(waiter, &[])?;
    // Give the second submit a moment to land in the queue.
    std::thread::sleep(Duration::from_millis(50));

    let third = runtime.spawn(waiter, &[])?;
    let third_out = third
        .join_timeout(Duration::from_secs(2))
        .ok_or("third should fail on full queue")?;

    gate.release();
    let _ = first.join_timeout(Duration::from_secs(2));
    let _ = second.join_timeout(Duration::from_secs(2));
    runtime.shutdown();
    io.shutdown()?;

    match third_out {
        FlowOutcome::Failed(msg) if msg.contains("queue is full") => {}
        other => return Err(format!("expected queue-full failure, got {other:?}").into()),
    }
    assert!(io.metrics().snapshot().rejected_full >= 1);
    Ok(())
}

#[derive(Debug)]
struct PanicDriver;

impl IoDriver for PanicDriver {
    fn try_submit(&self, _command: IoCommand) -> Result<u64, IoDriverError> {
        Err(IoDriverError::unsupported("unused"))
    }

    fn execute_host_await(
        &self,
        _op: u32,
        _args: Value,
        _cancel: &byteflow_io::CancellationToken,
    ) -> Result<Value, IoDriverError> {
        panic!("intentional driver panic");
    }
}

#[test]
fn host_await_driver_panic_fails_the_flow() -> Result<(), Box<dyn std::error::Error>> {
    use byteflow::{FlowOutcome, Program, Runtime};
    use byteflow_io::{ByteflowIo, IoConfig};

    let io = ByteflowIo::new(Arc::new(PanicDriver), IoConfig::default())?;
    let mut program = Program::new("ha-panic");
    program.function("main", 0, |f| {
        let args = f.load_i32(1);
        let _ticket = f.host_await(1, args);
        let msg = f.receive();
        f.return_(msg);
    });

    let runtime = Runtime::with_natives_and_config(
        program.build(),
        byteflow::std_native_table(),
        io.runtime_config(Arc::new(NullSink)),
    )?;
    let outcome = runtime
        .spawn(0, &[])?
        .join_timeout(Duration::from_secs(2))
        .ok_or("panic settle timed out")?;
    runtime.shutdown();
    io.shutdown()?;

    match outcome {
        FlowOutcome::Failed(msg) if msg.contains("panicked") => Ok(()),
        other => Err(format!("expected panic failure, got {other:?}").into()),
    }
}

#[test]
fn host_await_cancel_via_kill_does_not_deliver_late_result(
) -> Result<(), Box<dyn std::error::Error>> {
    use byteflow::{FlowOutcome, Program, Runtime};
    use byteflow_io::{ByteflowIo, IoConfig};

    let gate = Arc::new(HoldGate::new());
    let driver = Arc::new(HoldingDriver {
        gate: Arc::clone(&gate),
    });
    let io = ByteflowIo::new(driver, IoConfig::default())?;

    let mut program = Program::new("ha-cancel");
    program.function("main", 0, |f| {
        let args = f.load_i32(99);
        let _ticket = f.host_await(1, args);
        let msg = f.receive();
        f.return_(msg);
    });

    let runtime = Runtime::with_natives_and_config(
        program.build(),
        byteflow::std_native_table(),
        io.runtime_config(Arc::new(NullSink)),
    )?;
    let handle = runtime.spawn(0, &[])?;
    gate.wait_started(1);
    runtime.kill(handle.id())?;
    let outcome = handle
        .join_timeout(Duration::from_secs(2))
        .ok_or("kill join timed out")?;
    gate.release();
    std::thread::sleep(Duration::from_millis(50));
    runtime.shutdown();
    io.shutdown()?;

    match outcome {
        FlowOutcome::Failed(_) => Ok(()),
        FlowOutcome::Completed(Value::Message(msg))
            if matches!(msg.payload.as_ref(), Value::Int(99)) =>
        {
            Err("late host result resurrected the flow".into())
        }
        other => Err(format!("expected kill failure, got {other:?}").into()),
    }
}

#[test]
fn event_router_delivers_a_message_hop() -> Result<(), Box<dyn std::error::Error>> {
    use byteflow::{FlowOutcome, Program, Runtime, Value};
    use byteflow_io::samples::event_tag;
    use byteflow_io::EventRouter;

    let mut program = Program::new("event-router");
    program.function("main", 0, |f| {
        let msg = f.receive();
        f.return_(msg);
    });

    let runtime = Arc::new(Runtime::new(program.build())?);
    let handle = runtime.spawn(0, &[])?;
    let events = EventRouter::new(Arc::clone(&runtime));
    events.send(handle.id(), 1001, event_tag::COIN_INSERTED, Value::Int(500))?;

    let outcome = handle
        .join_timeout(Duration::from_secs(2))
        .ok_or("event delivery timed out")?;
    // `Runtime::shutdown` takes ownership; drop the Arc so `Drop` finalizes.
    drop(events);
    drop(runtime);

    match outcome {
        FlowOutcome::Completed(Value::Message(msg))
            if msg.tag == event_tag::COIN_INSERTED
                && msg.request_id == 1001
                && matches!(msg.payload.as_ref(), Value::Int(500)) =>
        {
            Ok(())
        }
        other => Err(format!("expected coin event hop, got {other:?}").into()),
    }
}

#[test]
fn parallel_workers_can_run_two_awaits() -> Result<(), Box<dyn std::error::Error>> {
    use byteflow::{FlowOutcome, Program, Runtime, TAG_SYS_AWAIT};
    use byteflow_io::{ByteflowIo, IoConfig};

    #[derive(Debug)]
    struct ParallelGate {
        active: Mutex<usize>,
        peak: Mutex<usize>,
        cv: Condvar,
        release: AtomicBool,
    }

    impl ParallelGate {
        fn enter(&self) {
            let mut active = self.active.lock().expect("active");
            *active += 1;
            let mut peak = self.peak.lock().expect("peak");
            if *active > *peak {
                *peak = *active;
            }
            self.cv.notify_all();
        }

        fn wait_peak_at_least(&self, n: usize) {
            let mut peak = self.peak.lock().expect("peak");
            while *peak < n {
                peak = self.cv.wait(peak).expect("wait peak");
            }
        }

        fn leave(&self) {
            let mut active = self.active.lock().expect("active");
            *active -= 1;
        }
    }

    #[derive(Debug)]
    struct ParallelDriver {
        gate: Arc<ParallelGate>,
    }

    impl IoDriver for ParallelDriver {
        fn try_submit(&self, _command: IoCommand) -> Result<u64, IoDriverError> {
            Err(IoDriverError::unsupported("unused"))
        }

        fn execute_host_await(
            &self,
            _op: u32,
            args: Value,
            _cancel: &byteflow_io::CancellationToken,
        ) -> Result<Value, IoDriverError> {
            self.gate.enter();
            while !self.gate.release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.gate.leave();
            Ok(args)
        }
    }

    let gate = Arc::new(ParallelGate {
        active: Mutex::new(0),
        peak: Mutex::new(0),
        cv: Condvar::new(),
        release: AtomicBool::new(false),
    });
    let io = ByteflowIo::new(
        Arc::new(ParallelDriver {
            gate: Arc::clone(&gate),
        }),
        IoConfig {
            worker_threads: 2,
            queue_capacity: 8,
            ..IoConfig::default()
        },
    )?;

    let mut program = Program::new("ha-parallel");
    let waiter = program.function("waiter", 0, |f| {
        let args = f.load_i32(3);
        let _ticket = f.host_await(1, args);
        let msg = f.receive();
        f.return_(msg);
    });

    let runtime = Runtime::with_natives_and_config(
        program.build(),
        byteflow::std_native_table(),
        io.runtime_config(Arc::new(NullSink)),
    )?;
    let a = runtime.spawn(waiter, &[])?;
    let b = runtime.spawn(waiter, &[])?;
    gate.wait_peak_at_least(2);
    gate.release.store(true, Ordering::Release);

    let a_out = a
        .join_timeout(Duration::from_secs(2))
        .ok_or("a timed out")?;
    let b_out = b
        .join_timeout(Duration::from_secs(2))
        .ok_or("b timed out")?;
    runtime.shutdown();
    io.shutdown()?;

    for outcome in [&a_out, &b_out] {
        match outcome {
            FlowOutcome::Completed(Value::Message(msg))
                if msg.tag == TAG_SYS_AWAIT && matches!(msg.payload.as_ref(), Value::Int(3)) => {}
            other => return Err(format!("expected hop 3, got {other:?}").into()),
        }
    }
    Ok(())
}
