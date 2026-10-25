//! Two runtimes in one process, one remote hop.
//!
//! Runtime A builds the frame with [`byteflow::encode_frame`]. Runtime B
//! delivers those bytes with [`byteflow::Runtime::deliver_frame`]. Neither
//! runtime opens a socket.

use byteflow::{
    flow_id_from_u64, CapId, FlowId, FlowOutcome, Message, OutboundHop, Program, RemoteFrameError,
    Runtime, RuntimeConfig, Value,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sender_zero = OutboundHop {
        sender: FlowId::HOST,
        node: 1,
        name: std::sync::Arc::from("inbox"),
        message: Message::new(0, 1, 7, Value::Int(8)),
    };
    match encode_named(&sender_zero) {
        Err(RemoteFrameError::SenderZero) => {
            println!("sender 0 rejected");
        }
        other => return Err(format!("sender 0 should be rejected, got {other:?}").into()),
    }

    let cap = CapId::random()?;
    let with_cap = OutboundHop {
        sender: flow_id_from_u64(44),
        node: 1,
        name: std::sync::Arc::from("inbox"),
        message: Message::new(flow_id_from_u64(44).as_u64(), 1, 7, Value::Cap(cap)),
    };
    match encode_named(&with_cap) {
        Err(RemoteFrameError::LiveCapability) => {
            println!("capability rejected");
        }
        other => return Err(format!("capability should be rejected, got {other:?}").into()),
    }

    let with_pid = OutboundHop {
        sender: flow_id_from_u64(44),
        node: 1,
        name: std::sync::Arc::from("inbox"),
        message: Message::new(flow_id_from_u64(44).as_u64(), 1, 7, Value::Pid(44)),
    };
    match encode_named(&with_pid) {
        Err(RemoteFrameError::LiveCapability) => {
            println!("pid rejected");
        }
        other => return Err(format!("pid should be rejected, got {other:?}").into()),
    }

    let mut program = Program::new("inbox");
    program.function("main", 0, |f| {
        let msg = f.receive();
        let n = f.hop_payload(msg);
        f.return_(n);
    });
    let mut origin_program = Program::new("origin");
    origin_program.function("main", 0, |f| {
        let zero = f.load_i32(0);
        f.return_(zero);
    });
    let origin = Runtime::with_config(
        origin_program.build(),
        RuntimeConfig {
            workers: 1,
            ..RuntimeConfig::default()
        },
    )?;
    let destination = Runtime::with_natives_and_config(
        program.build(),
        byteflow::std_native_table(),
        RuntimeConfig {
            workers: 1,
            ..RuntimeConfig::default()
        },
    )?;
    let handle = destination.spawn(0, &[])?;
    let cap = destination.mint_cap(handle.id())?;
    destination.register_name("inbox", cap)?;

    let frame = encode_named(&OutboundHop {
        sender: flow_id_from_u64(44),
        node: 2,
        name: std::sync::Arc::from("inbox"),
        message: Message::new(flow_id_from_u64(44).as_u64(), 1, 7, Value::Int(8)),
    })?;
    destination.deliver_frame(&frame)?;
    let outcome = handle
        .join_timeout(std::time::Duration::from_secs(2))
        .ok_or("delivery timed out")?;
    origin.shutdown();
    destination.shutdown();
    match outcome {
        FlowOutcome::Completed(Value::Int(8)) => {
            println!("delivered 8 to inbox from flow 44");
            Ok(())
        }
        other => Err(format!("mailbox returned {other:?}").into()),
    }
}

fn encode_named(hop: &OutboundHop) -> Result<Vec<u8>, RemoteFrameError> {
    byteflow::encode_frame(hop)
}
