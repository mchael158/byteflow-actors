//! Property-style mailbox stress tests (in-house PRNG — no external crates).

use byteflow::prng::XorShift64;
use byteflow::{
    Delivery, Mailbox, MailboxBytes, MailboxCapacity, MailboxConfig, MailboxFullReason,
    OverflowPolicy, Value,
};

fn mailbox(capacity: u32, policy: OverflowPolicy) -> Mailbox {
    let capacity = match MailboxCapacity::new(capacity) {
        Some(c) => c,
        None => MailboxCapacity::DEFAULT,
    };
    let bytes = match MailboxBytes::new(16 * 1024 * 1024) {
        Some(b) => b,
        None => MailboxConfig::DEFAULT.bytes(),
    };
    Mailbox::with_config(MailboxConfig::new(capacity, policy).with_bytes(bytes))
}

#[test]
fn drop_newest_never_exceeds_capacity() -> Result<(), String> {
    let mut rng = XorShift64::new(0xB0A1_0001);
    for _case in 0..64 {
        let capacity = rng.next_u32_inclusive(1, 63);
        let mb = mailbox(capacity, OverflowPolicy::DropNewest);
        let limit = capacity as usize;
        let n_payloads = rng.next_u32_inclusive(0, 200) as usize;
        for _ in 0..n_payloads {
            let _ = mb
                .push(Value::Int(rng.next_i64()))
                .map_err(|e| e.to_string())?;
            let stats = mb.stats().map_err(|e| e.to_string())?;
            if stats.queued_messages > limit {
                return Err(format!(
                    "queued={} > capacity={}",
                    stats.queued_messages, limit
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn reject_never_exceeds_capacity() -> Result<(), String> {
    let mut rng = XorShift64::new(0xB0A1_0002);
    for _case in 0..64 {
        let capacity = rng.next_u32_inclusive(1, 31);
        let mb = mailbox(capacity, OverflowPolicy::Reject);
        let limit = capacity as usize;
        let n_payloads = rng.next_u32_inclusive(0, 100) as usize;
        for _ in 0..n_payloads {
            let _ = mb
                .push(Value::Int(rng.next_i64()))
                .map_err(|e| e.to_string())?;
            let stats = mb.stats().map_err(|e| e.to_string())?;
            if stats.queued_messages > limit {
                return Err(format!(
                    "queued={} > capacity={}",
                    stats.queued_messages, limit
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn drop_newest_preserves_existing_head() -> Result<(), String> {
    let mut rng = XorShift64::new(0xB0A1_0003);
    for _ in 0..64 {
        let a = rng.next_i64();
        let mut b = rng.next_i64();
        if a == b {
            b = b.wrapping_add(1);
        }
        let mb = mailbox(1, OverflowPolicy::DropNewest);
        match mb.push(Value::Int(a)).map_err(|e| e.to_string())? {
            Ok(Delivery::Queued) => {}
            _ => return Err("first push: expected Queued".into()),
        }
        match mb.push(Value::Int(b)).map_err(|e| e.to_string())? {
            Ok(Delivery::DroppedNewest) => {}
            _ => return Err("second push: expected DroppedNewest".into()),
        }
        match mb.try_pop().map_err(|e| e.to_string())? {
            Some(Value::Int(got)) if got == a => {}
            Some(Value::Int(got)) => {
                return Err(format!("expected head Int({a}), got Int({got})"));
            }
            Some(_) => return Err("expected head Int".into()),
            None => return Err("head missing after DropNewest".into()),
        }
        let stats = mb.stats().map_err(|e| e.to_string())?;
        if stats.dropped_newest < 1 {
            return Err("dropped_newest counter not updated".into());
        }
        if stats.queued_messages != 0 {
            return Err("queue should be empty after pop".into());
        }
    }
    Ok(())
}

#[test]
fn reject_at_capacity_is_message_limit() -> Result<(), String> {
    let mut rng = XorShift64::new(0xB0A1_0004);
    for _ in 0..64 {
        let seed = rng.next_i64();
        let mb = mailbox(2, OverflowPolicy::Reject);
        for n in [seed, seed.wrapping_add(1)] {
            match mb.push(Value::Int(n)).map_err(|e| e.to_string())? {
                Ok(_) => {}
                Err(_) => return Err("seed push hit MailboxFull early".into()),
            }
        }
        let before = mb.stats().map_err(|e| e.to_string())?.queued_messages;
        if before != 2 {
            return Err(format!("expected occupancy 2, got {before}"));
        }
        match mb
            .push(Value::Int(seed.wrapping_add(2)))
            .map_err(|e| e.to_string())?
        {
            Err(full) if full.reason() == MailboxFullReason::MessageLimit => {}
            Err(full) => {
                return Err(format!("expected MessageLimit, got {:?}", full.reason()));
            }
            Ok(_) => return Err("expected MessageLimit refusal".into()),
        }
        let after = mb.stats().map_err(|e| e.to_string())?.queued_messages;
        if after != 2 {
            return Err(format!("occupancy changed after reject: {after}"));
        }
    }
    Ok(())
}
