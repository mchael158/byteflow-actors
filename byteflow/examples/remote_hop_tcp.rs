//! TCP reference transport: one `SendRemote` hop crosses a real socket.
//!
//! Runs origin and destination runtimes in one process (CI-friendly). The
//! pattern is the same on two hosts: bind a listener, [`spawn_tcp_inbound`],
//! register the service name, attach [`DistributionBridge`] + [`spawn_tcp_outbound`]
//! on the sender node.
//!
//! ```text
//! cargo run -p byteflow-actors --example remote_hop_tcp
//! ```

use std::net::TcpListener;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use byteflow::{
    FlowOutcome, Program, RemoteFrameError, Runtime, RuntimeConfig, TcpInboundConfig,
    TcpOutboundConfig, Value,
};

const REMOTE_NODE: i32 = 2;
const REMOTE_MSG_TAG: u16 = 7;
const SERVICE: &str = "inbox";

fn main() -> Result<(), String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    listener.set_nonblocking(false).map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let (ready_tx, ready_rx) = sync_channel(1);

    let dest = thread::spawn(move || run_destination(listener, ready_tx));

    ready_rx
        .recv()
        .map_err(|_| "destination not ready".to_string())?;
    thread::sleep(Duration::from_millis(50));
    run_origin(addr)?;

    match dest.join() {
        Ok(result) => result,
        Err(_) => Err("destination thread panicked".into()),
    }
}

fn run_destination(listener: TcpListener, ready: SyncSender<()>) -> Result<(), String> {
    let mut program = Program::new("inbox");
    program.function("main", 0, |f| {
        let msg = f.receive_match_imm(REMOTE_MSG_TAG);
        let n = f.hop_payload(msg);
        f.return_(n);
    });

    let rt = Runtime::with_natives_and_config(
        program.build(),
        byteflow::std_native_table(),
        RuntimeConfig {
            workers: 1,
            ..RuntimeConfig::default()
        },
    )
    .map_err(|e| e.to_string())?;

    let handle = rt.spawn(0, &[]).map_err(|e| e.to_string())?;
    let cap = rt.mint_cap(handle.id()).map_err(|e| e.to_string())?;
    rt.register_name(SERVICE, cap).map_err(|e| e.to_string())?;

    let (frame_tx, frame_rx) = sync_channel::<Vec<u8>>(32);
    let deliver = Arc::new(move |bytes: &[u8]| {
        frame_tx
            .send(bytes.to_vec())
            .map_err(|_| RemoteFrameError::Unavailable)?;
        Ok(())
    });

    let inbound =
        byteflow::spawn_tcp_inbound(listener, Arc::clone(&deliver), TcpInboundConfig::default())
            .map_err(|e| e.to_string())?;

    let _ = ready.send(());

    let outcome = wait_inbox_with_frames(&rt, &handle, &frame_rx, Duration::from_secs(5))?;
    inbound.stop().map_err(|e| e.to_string())?;
    drop(deliver);
    rt.shutdown();

    match outcome {
        FlowOutcome::Completed(Value::Int(8)) => {
            println!("tcp delivered 8 to {SERVICE}");
            Ok(())
        }
        other => Err(format!("unexpected inbox outcome: {other:?}")),
    }
}

fn wait_inbox_with_frames(
    rt: &Runtime,
    handle: &byteflow::FlowHandle,
    frame_rx: &Receiver<Vec<u8>>,
    timeout: Duration,
) -> Result<FlowOutcome, String> {
    let deadline = Instant::now() + timeout;
    loop {
        while let Ok(bytes) = frame_rx.try_recv() {
            rt.deliver_frame(&bytes).map_err(|e| e.to_string())?;
        }
        if let Some(outcome) = handle.join_timeout(Duration::from_millis(25)) {
            return Ok(outcome);
        }
        if Instant::now() >= deadline {
            return Err("inbox timed out waiting for remote hop".into());
        }
    }
}

fn run_origin(peer: std::net::SocketAddr) -> Result<(), String> {
    let bridge = byteflow::DistributionBridge::new();
    let outbound = byteflow::spawn_tcp_outbound(
        Arc::clone(&bridge),
        peer,
        TcpOutboundConfig {
            queue_capacity: 16,
            connect_timeout: Duration::from_secs(2),
            ..TcpOutboundConfig::default()
        },
    )
    .map_err(|e| e.to_string())?;

    let mut program = Program::new("origin");
    program.function("main", 0, |f| {
        let node = f.load_i32(REMOTE_NODE);
        let name = f.load_str(SERVICE);
        let payload = f.load_i32(8);
        let msg = f.hop_fresh(i32::from(REMOTE_MSG_TAG), payload);
        f.send_remote(node, name, msg);
        f.return_(payload);
    });

    let rt = Runtime::with_natives_and_config(
        program.build(),
        byteflow::std_native_table(),
        RuntimeConfig {
            distribution: Some(Arc::clone(&bridge)),
            workers: 1,
            ..RuntimeConfig::default()
        },
    )
    .map_err(|e| e.to_string())?;

    let outcome = rt.spawn(0, &[]).map_err(|e| e.to_string())?.join();
    rt.shutdown();
    byteflow::wait_bridge_drained(&bridge, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    outbound
        .wait_wire_drained(Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    outbound.stop().map_err(|e| e.to_string())?;

    match outcome {
        FlowOutcome::Completed(Value::Int(8)) => Ok(()),
        other => Err(format!("origin flow: {other:?}")),
    }
}
