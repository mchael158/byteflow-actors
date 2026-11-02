//! Built-in demo chunks assembled with [`crate::Program`].
//!
//! Use these as runnable specs of the messaging contract (and as regression
//! tests). Prefer copying a sample over inventing hop register layouts from
//! scratch.
//!
//! | Sample | Shows |
//! |--------|--------|
//! | [`add_forty_two`] | Scalar VM path (no natives) |
//! | [`ping_pong`] | Cap spawn + Atomic Hop round-trip |
//! | [`atomic_request_reply`] | Tagged REQ/REP + correlated receive |
//! | [`atomic_actors`] | Server loop + two `Ask` clients + minted `request_id` |
//! | [`cap_in_payload`] | Cap in hop payload is reissued to the recipient |
//! | [`cap_in_list`] | Cap nested in a `List` is reissued the same way |
//! | [`selective_receive`] | `ReceiveMatch` FIFO skip |
//! | [`selective_receive_timeout`] | BEAM `after`: `ReceiveMatchImmTimeout` → `Unit` |
//! | [`selective_receive_after`] | `after` still skips junk and hits the tag |
//! | [`process_alive`] | `IsAlive` (BEAM `is_process_alive/1`) |
//! | [`process_dict`] | process dictionary `put` / `get` / `erase` |
//! | [`receive_match_eq`] | term equality: tag + `Int` payload |
//! | [`exit_signal`] | BEAM `exit/2` → `DOWN` with `Fault` |
//! | [`supervised_tree`] | bytecode `StartChild` under a host supervisor |
//! | [`receive_match_kind`] | `ReceiveMatchKind` by payload wire-tag |
//! | [`ask_reply`] | `Ask` RPC hop |
//! | [`ask_timeout_expires`] | `AskTimeout` writes `Unit` when the server stays silent |
//! | [`ask_target_exits`] | `Ask` dest is `TAG_SYS_EXIT` when the server dies first |
//! | [`server_loop`] | BEAM-style receive → handle → reply loop |
//! | [`named_service`] | Bytecode `register_name` / `whereis` (Cap, not FlowId) |
//! | [`forged_sender_send`] / [`forged_sender_ask`] | S1: forged `make_msg` sender dies |
//! | [`boom`] | Immediate trap (supervisor demos) |
//! | [`monitor_down`] | Monitor → [`crate::TAG_SYS_DOWN`] on child exit |
//! | [`waiting_send`] | `WAITING_SEND`: second hop parks until the first is received |
//!
//! Lifecycle regression tests in this module (not demo samples) also cover
//! `trap_exit` / `SetTrapExit` and linked `TAG_SYS_EXIT` hops.
//!
//! Hop samples require [`crate::std_native_table`].
//!
//! ## Flow / “actor” patterns (all correct when used in the right role)
//!
//! | Role | Pattern | Examples |
//! |------|---------|----------|
//! | Long-lived service | [`Fn::actor_loop`](crate::Fn::actor_loop) + `send_reply` | [`server_loop`], [`named_service`], [`atomic_actors`] server, [`ask_reply`] |
//! | One-shot worker | `receive_match_imm(tag)` → handle → `send_reply` → `exit` | [`ping_pong`] `pong`, [`atomic_request_reply`] server, [`cap_in_*`] |
//! | RPC client | [`Fn::ask`] / `ask_timeout` + `hop_fresh` | [`atomic_actors`] client, [`ask_reply`] main |
//! | Cast client | `send` / `cast` + `receive_match_corr_imm` or `receive_match_imm` | [`ping_pong`] main, [`selective_receive`] |
//!
//! Prefer shared tags in [`TAG_REQ`] / [`TAG_REP`] / …; mint ids with [`Fn::hop_fresh`]
//! when correlating multiple in-flight hops on one flow.

use crate::natives::std_native;
use crate::{Chunk, Program};

/// Native indices (must match [`crate::std_native_map`]).
const N_PRINT: u32 = std_native::PRINT;
const N_MAKE_MSG: u32 = std_native::MAKE_MSG;

/// Protocol tags for Atomic Hop samples.
pub const TAG_REQ: i32 = 1;
pub const TAG_REP: i32 = 2;
/// Client → parent completion hop in [`atomic_actors`] (not an RPC reply).
pub const TAG_DONE: i32 = 3;
/// Readiness handshake (e.g. `SetTrapExit` before link).
pub const TAG_READY: i32 = 4;
pub const TAG_PING: i32 = 10;
pub const TAG_PONG: i32 = 11;
/// Decoy hop for [`selective_receive`] — must be skipped by `ReceiveMatch`.
pub const TAG_JUNK: i32 = 99;

/// `41 + 1; return` — the 60-second sanity chunk.
pub fn add_forty_two() -> Chunk {
    let mut p = Program::new("add-forty-two");
    p.function("main", 0, |f| {
        let a = f.load_i32(41);
        let b = f.load_i32(1);
        let sum = f.add(a, b);
        f.return_(sum);
    });
    p.build()
}

/// Two flows, one **Atomic Hop** round-trip: `main` sends a `Message` to
/// `pong`, `pong` replies with payload+1 via `msg_reply_cap`, `main` returns
/// that payload (`2`).
pub fn ping_pong() -> Chunk {
    let mut p = Program::new("ping-pong");
    let pong = p.function("pong", 0, |f| {
        let msg = f.receive_match_imm(TAG_PING as u16);
        let payload = f.hop_payload(msg);
        f.add_imm(payload, 1);
        f.send_reply(msg, TAG_PONG, payload);
        f.exit(payload);
    });
    p.function("main", 0, |f| {
        let child = f.spawn(pong, &[]);
        let payload = f.load_i32(1);
        let req = f.hop_fresh(TAG_PING, payload);
        let rid = f.hop_request_id(req);
        f.send(child, req);
        let reply = f.receive_match_corr_imm(TAG_PONG as u16, rid);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// Atomic request-reply with [`crate::Value::Message`] (one envelope per hop).
pub fn atomic_request_reply() -> Chunk {
    let mut p = Program::new("atomic-request-reply");
    let server = p.function("server", 0, |f| {
        let msg = f.receive_match_imm(TAG_REQ as u16);
        f.native1_on(msg, N_PRINT);
        let payload = f.hop_payload(msg);
        f.add_imm(payload, 1);
        f.send_reply(msg, TAG_REP, payload);
        f.exit(payload);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let payload = f.load_i32(41);
        let req = f.hop_fresh(TAG_REQ, payload);
        let rid = f.hop_request_id(req);
        f.native1_on(req, N_PRINT);
        f.send(server_cap, req);
        let reply = f.receive_match_corr_imm(TAG_REP as u16, rid);
        f.native1_on(reply, N_PRINT);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// Selective Atomic Hop: server waits for `TAG_REQ` while a `TAG_JUNK` hop
/// sits ahead in the mailbox (FIFO skip, not drop).
pub fn selective_receive() -> Chunk {
    let mut p = Program::new("selective-receive");
    let server = p.function("server", 0, |f| {
        let msg = f.receive_match_imm(TAG_REQ as u16);
        let payload = f.hop_payload(msg);
        f.add_imm(payload, 1);
        f.send_reply(msg, TAG_REP, payload);
        let junk = f.receive_match_imm(TAG_JUNK as u16);
        let tag = f.hop_tag(junk);
        let is_junk = f.eq_imm(tag, TAG_JUNK);
        let trap_lbl = f.label();
        f.branch_if_falsy(is_junk, trap_lbl);
        f.exit(payload);
        f.bind(trap_lbl);
        f.trap(2);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let req_id = f.load_i32(1);
        let zero = f.load_i32(0);
        let junk = f.hop(req_id, TAG_JUNK, zero);
        f.send(server_cap, junk);
        let payload = f.load_i32(41);
        let req = f.hop(req_id, TAG_REQ, payload);
        f.send(server_cap, req);
        let reply = f.receive_match_imm(TAG_REP as u16);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// BEAM `receive … after`: nobody sends `TAG_REQ`, so the wait writes `Unit`.
pub fn selective_receive_timeout() -> Chunk {
    let mut p = Program::new("selective-receive-timeout");
    p.function("main", 0, |f| {
        let ms = f.load_i32(40);
        let msg = f.receive_match_imm_timeout(TAG_REQ as u16, ms);
        f.return_(msg);
    });
    p.build()
}

/// Selective receive + `after`: junk stays queued, `TAG_REQ` arrives in time.
pub fn selective_receive_after() -> Chunk {
    let mut p = Program::new("selective-receive-after");
    let server = p.function("server", 0, |f| {
        let ms = f.load_i32(5_000);
        let msg = f.receive_match_imm_timeout(TAG_REQ as u16, ms);
        let payload = f.hop_payload(msg);
        f.add_imm(payload, 1);
        f.send_reply(msg, TAG_REP, payload);
        let junk = f.receive_match_imm(TAG_JUNK as u16);
        let tag = f.hop_tag(junk);
        let is_junk = f.eq_imm(tag, TAG_JUNK);
        let trap_lbl = f.label();
        f.branch_if_falsy(is_junk, trap_lbl);
        f.exit(payload);
        f.bind(trap_lbl);
        f.trap(2);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let req_id = f.load_i32(1);
        let zero = f.load_i32(0);
        let junk = f.hop(req_id, TAG_JUNK, zero);
        f.cast(server_cap, junk);
        let payload = f.load_i32(41);
        let req = f.hop(req_id, TAG_REQ, payload);
        f.cast(server_cap, req);
        let reply = f.receive_match_imm(TAG_REP as u16);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// BEAM `is_process_alive/1`: a parked child is live; after `DOWN`, its Cap is not.
pub fn process_alive() -> Chunk {
    let mut p = Program::new("process-alive");
    let parked = p.function("parked", 0, |f| {
        let _msg = f.receive_match_imm(TAG_REQ as u16);
        let z = f.load_i32(0);
        f.return_(z);
    });
    p.function("main", 0, |f| {
        let live_cap = f.spawn(parked, &[]);
        let was_live = f.is_alive(live_cap);
        let _mon = f.monitor(live_cap);
        let rid = f.load_i32(1);
        let z = f.load_i32(0);
        let hop = f.hop(rid, TAG_REQ, z);
        f.cast(live_cap, hop);
        let _down = f.receive_match_imm(crate::TAG_SYS_DOWN);
        let still_alive = f.is_alive(live_cap);
        let bad = f.label();
        let ok = f.label();
        f.branch_if_falsy(was_live, bad);
        f.branch_if_falsy(still_alive, ok);
        f.bind(bad);
        f.trap(1);
        f.bind(ok);
        let one = f.load_i32(1);
        f.return_(one);
    });
    p.build()
}

/// Process dictionary (`put` / `get` / `erase`): store 41, add 1 in a
/// register, erase still yields the stored 41. Returns `42 + 41 = 83`.
pub fn process_dict() -> Chunk {
    let mut p = Program::new("process-dict");
    p.function("main", 0, |f| {
        let key = f.load_i32(1);
        let value = f.load_i32(41);
        let _prev = f.dict_put(key, value);
        let got = f.dict_get(key);
        f.add_imm(got, 1);
        let erased = f.dict_erase(key);
        let sum = f.add(got, erased);
        f.return_(sum);
    });
    p.build()
}

/// Term equality receive: skip a `TAG_REQ` whose payload is `0`, take `41`.
pub fn receive_match_eq() -> Chunk {
    let mut p = Program::new("receive-match-eq");
    let server = p.function("server", 0, |f| {
        let tag = f.load_i32(TAG_REQ);
        let want = f.load_i32(41);
        let msg = f.receive_match_eq(tag, want);
        let payload = f.hop_payload(msg);
        f.add_imm(payload, 1);
        f.send_reply(msg, TAG_REP, payload);
        let junk = f.receive_match_imm(TAG_REQ as u16);
        let got = f.hop_payload(junk);
        let zero = f.eq_imm(got, 0);
        let trap_lbl = f.label();
        f.branch_if_falsy(zero, trap_lbl);
        f.exit(payload);
        f.bind(trap_lbl);
        f.trap(2);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let req_id = f.load_i32(1);
        let zero = f.load_i32(0);
        let junk = f.hop(req_id, TAG_REQ, zero);
        f.cast(server_cap, junk);
        let payload = f.load_i32(41);
        let req = f.hop(req_id, TAG_REQ, payload);
        f.cast(server_cap, req);
        let reply = f.receive_match_imm(TAG_REP as u16);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// BEAM `exit/2`: child parked on receive dies with `Fault`; monitor payload is `3`.
pub fn exit_signal() -> Chunk {
    let mut p = Program::new("exit-signal");
    let parked = p.function("parked", 0, |f| {
        let _msg = f.receive_match_imm(TAG_REQ as u16);
        let z = f.load_i32(0);
        f.return_(z);
    });
    p.function("main", 0, |f| {
        let cap = f.spawn(parked, &[]);
        let _mon = f.monitor(cap);
        let reason = f.load_i32(crate::FlowExitReason::Fault as i32);
        f.exit_signal(cap, reason);
        let down = f.receive_match_imm(crate::TAG_SYS_DOWN);
        let payload = f.hop_payload(down);
        f.return_(payload);
    });
    p.build()
}

/// Root (started by a host [`crate::Supervisor`]) starts a parked worker,
/// then `exit/2`s it. Returns `1` when the worker was alive and `DOWN` is `Fault`.
pub fn supervised_tree() -> Chunk {
    let mut p = Program::new("supervised-tree");
    let worker = p.function("worker", 0, |f| {
        let _msg = f.receive_match_imm(TAG_REQ as u16);
        let z = f.load_i32(0);
        f.return_(z);
    });
    p.function("root", 0, |f| {
        let cap = f.start_child(worker, crate::RestartPolicy::Never);
        let alive = f.is_alive(cap);
        let _mon = f.monitor(cap);
        let reason = f.load_i32(crate::FlowExitReason::Fault as i32);
        f.exit_signal(cap, reason);
        let down = f.receive_match_imm(crate::TAG_SYS_DOWN);
        let payload = f.hop_payload(down);
        let fault = f.eq_imm(payload, crate::FlowExitReason::Fault as i32);
        let bad = f.label();
        let ok = f.label();
        f.branch_if_falsy(alive, bad);
        f.branch_if_falsy(fault, bad);
        f.jump(ok);
        f.bind(bad);
        f.trap(1);
        f.bind(ok);
        let one = f.load_i32(1);
        f.return_(one);
    });
    p.build()
}

/// Selective receive by payload wire-tag (`ReceiveMatchKind`): skip hops
/// whose payload is not `Int` (tag 2), then reply with that payload.
pub fn receive_match_kind() -> Chunk {
    let mut p = Program::new("receive-match-kind");
    let server = p.function("server", 0, |f| {
        // BFV0 Int = 2
        let msg = f.receive_match_kind(2);
        let payload = f.hop_payload(msg);
        f.send_reply(msg, TAG_REP, payload);
        let _junk = f.receive_match_imm(TAG_JUNK as u16);
        f.exit(payload);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let req_id = f.load_i32(1);
        let decoy = f.load_str("skip-me");
        let junk = f.hop(req_id, TAG_JUNK, decoy);
        f.send(server_cap, junk);
        let payload = f.load_i32(42);
        let req = f.hop(req_id, TAG_REQ, payload);
        f.send(server_cap, req);
        let reply = f.receive_match_imm(TAG_REP as u16);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// Atomic request/reply via `Ask` (RPC hop). Server uses [`Fn::actor_loop`]
/// so repeated `Ask`s on the same Cap stay valid (CLI / long-lived service).
pub fn ask_reply() -> Chunk {
    let mut p = Program::new("ask-reply");
    let server = p.function("server", 0, |f| {
        f.actor_loop(TAG_REQ as u16, |f, msg| {
            let payload = f.hop_payload(msg);
            f.add_imm(payload, 1);
            f.send_reply(msg, TAG_REP, payload);
        });
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let payload = f.load_i32(41);
        let req = f.hop_fresh(TAG_REQ, payload);
        let reply = f.ask(server_cap, req);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// `AskTimeout` against a server that never replies → `Unit`.
pub fn ask_timeout_expires() -> Chunk {
    let mut p = Program::new("ask-timeout");
    let server = p.function("server", 0, |f| {
        let _msg = f.receive_match_imm(TAG_REQ as u16);
        let ms = f.load_i32(10_000);
        f.sleep(ms);
        let zero = f.load_i32(0);
        f.return_(zero);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let req_id = f.load_i32(1);
        let payload = f.load_i32(0);
        let req = f.hop(req_id, TAG_REQ, payload);
        let ms = f.load_i32(40);
        let reply = f.ask_timeout(server_cap, req, ms);
        f.return_(reply);
    });
    p.build()
}

/// Server takes the request and exits; client `Ask` must not hang.
pub fn ask_target_exits() -> Chunk {
    let mut p = Program::new("ask-target-exits");
    let server = p.function("server", 0, |f| {
        let _msg = f.receive_match_imm(TAG_REQ as u16);
        let z = f.load_i32(0);
        f.return_(z);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let req_id = f.load_i32(1);
        let payload = f.load_i32(0);
        let req = f.hop(req_id, TAG_REQ, payload);
        let reply = f.ask(server_cap, req);
        let tag = f.hop_tag(reply);
        f.return_(tag);
    });
    p.build()
}

/// BEAM-style server loop: `receive_match` → handle → `send_reply` → repeat.
pub fn server_loop() -> Chunk {
    let mut p = Program::new("server-loop");
    let server = p.function("server", 0, |f| {
        f.actor_loop(TAG_REQ as u16, |f, req| {
            let payload = f.hop_payload(req);
            f.add_imm(payload, 1);
            f.send_reply(req, TAG_REP, payload);
        });
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let payload = f.load_i32(41);
        let req = f.hop_fresh(TAG_REQ, payload);
        f.send(server_cap, req);
        let reply = f.receive_match_imm(TAG_REP as u16);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// Server publishes `"svc"`; client loops `whereis` until it gets a SEND Cap
/// (not ASK — use `Send` + `Receive`, not `Ask`). Returns `42` (41 + 1).
/// Discovery never exposes a raw FlowId.
pub fn named_service() -> Chunk {
    let mut p = Program::new("named-service");
    let server = p.function("server", 0, |f| {
        let name = f.load_str("svc");
        f.register_name(name);
        f.actor_loop(TAG_REQ as u16, |f, req| {
            let payload = f.hop_payload(req);
            f.add_imm(payload, 1);
            f.send_reply(req, TAG_REP, payload);
        });
    });
    p.function("main", 0, |f| {
        let _server = f.spawn(server, &[]);
        let name = f.load_str("svc");
        let retry = f.label();
        let miss = f.label();
        f.bind(retry);
        let cap = f.whereis(name);
        f.branch_if_falsy(cap, miss);
        let payload = f.load_i32(41);
        let req = f.hop_fresh(TAG_REQ, payload);
        f.send(cap, req);
        let reply = f.receive_match_imm(TAG_REP as u16);
        let out = f.hop_payload(reply);
        f.return_(out);
        f.bind(miss);
        // Sleep (not Yield): Yield re-queues on the local worker deque and
        // can starve the injector, so the server would never register.
        let ms = f.load_i32(1);
        f.sleep(ms);
        f.jump(retry);
    });
    p.build()
}

/// Canonical Atomic Hop actor demo: one server loop, two clients, each
/// doing `N` `Ask`s with minted `request_id`s. Each client asserts that the
/// reply echoes the request's `request_id` before accumulating. Main waits
/// for both DONE hops and returns the sum (`2 * 36 = 72`).
pub fn atomic_actors() -> Chunk {
    const N: i32 = 8;
    let mut p = Program::new("atomic-actors");
    let server = p.function("server", 0, |f| {
        f.actor_loop(TAG_REQ as u16, |f, req| {
            let payload = f.hop_payload(req);
            f.add_imm(payload, 1);
            f.send_reply(req, TAG_REP, payload);
        });
    });
    let client = p.function("client", 2, |f| {
        let server_cap = f.reg(0);
        let parent_cap = f.reg(1);
        let acc = f.load_i32(0);
        let i = f.load_i32(0);
        let n = f.load_i32(N);
        f.while_lt(i, n, |f| {
            let req = f.hop_fresh(TAG_REQ, i);
            let expect = f.hop_request_id(req);
            let reply = f.ask(server_cap, req);
            let got_id = f.hop_request_id(reply);
            let same = f.eq(expect, got_id);
            let bad = f.label();
            f.branch_if_falsy(same, bad);
            let got = f.hop_payload(reply);
            let sum = f.add(acc, got);
            f.mov(acc, sum);
            f.add_imm(i, 1);
            let cont = f.label();
            f.jump(cont);
            f.bind(bad);
            f.trap(1);
            f.bind(cont);
        });
        let done = f.hop_fresh(TAG_DONE, acc);
        f.send(parent_cap, done);
        f.return_(acc);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let me = f.self_cap();
        let w1 = f.window(3);
        f.mov(w1.at(1), server_cap);
        f.mov(w1.at(2), me);
        f.spawn_at(w1.at(0), client, 2);
        let w2 = f.window(3);
        f.mov(w2.at(1), server_cap);
        f.mov(w2.at(2), me);
        f.spawn_at(w2.at(0), client, 2);
        let a = f.receive_match_imm(TAG_DONE as u16);
        let b = f.receive_match_imm(TAG_DONE as u16);
        let pa = f.hop_payload(a);
        let pb = f.hop_payload(b);
        let out = f.add(pa, pb);
        f.return_(out);
    });
    p.build()
}

/// Server receives a Cap in the payload and sends `42` back through it.
/// Exercises hop-time reissue (holder becomes the recipient).
pub fn cap_in_payload() -> Chunk {
    let mut p = Program::new("cap-in-payload");
    let server = p.function("server", 0, |f| {
        let msg = f.receive_match_imm(TAG_REQ as u16);
        let dest = f.hop_payload(msg);
        let payload = f.load_i32(42);
        let reply = f.hop_fresh(TAG_REP, payload);
        f.send(dest, reply);
        f.exit(payload);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let me = f.self_cap();
        let req = f.hop_fresh(TAG_REQ, me);
        f.send(server_cap, req);
        let reply = f.receive_match_imm(TAG_REP as u16);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// Same grant as [`cap_in_payload`], with the cap nested in a list.
pub fn cap_in_list() -> Chunk {
    let mut p = Program::new("cap-in-list");
    let server = p.function("server", 0, |f| {
        let msg = f.receive_match_imm(TAG_REQ as u16);
        let list = f.hop_payload(msg);
        let dest = f.list_get(list, 0);
        let payload = f.load_i32(42);
        let reply = f.hop_fresh(TAG_REP, payload);
        f.send(dest, reply);
        f.exit(payload);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let me = f.self_cap();
        let packed = f.list_pack(&[me]);
        let req = f.hop_fresh(TAG_REQ, packed);
        f.send(server_cap, req);
        let reply = f.receive_match_imm(TAG_REP as u16);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// Security regression: forged `make_msg` sender must not survive `Send`.
pub fn forged_sender_send() -> Chunk {
    let mut p = Program::new("forged-sender-send");
    let server = p.function("server", 0, |f| {
        let msg = f.receive_match_imm(TAG_REQ as u16);
        let sender = f.hop_sender(msg);
        f.send_reply(msg, TAG_REP, sender);
        f.exit(sender);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let forged = f.load_i32(999);
        let req_id = f.load_i32(1);
        let zero = f.load_i32(0);
        let req = f.make_msg_legacy_sender(N_MAKE_MSG, forged, req_id, TAG_REQ, zero);
        f.send(server_cap, req);
        let reply = f.receive();
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// Same security property as [`forged_sender_send`], via `Ask`.
pub fn forged_sender_ask() -> Chunk {
    let mut p = Program::new("forged-sender-ask");
    let server = p.function("server", 0, |f| {
        let msg = f.receive_match_imm(TAG_REQ as u16);
        let sender = f.hop_sender(msg);
        f.send_reply(msg, TAG_REP, sender);
        f.exit(sender);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, &[]);
        let forged = f.load_i32(999);
        let req_id = f.load_i32(1);
        let zero = f.load_i32(0);
        let req = f.make_msg_legacy_sender(N_MAKE_MSG, forged, req_id, TAG_REQ, zero);
        let reply = f.ask(server_cap, req);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// Child sleeps then exits; parent monitors and returns the `DOWN` reason (`0` = normal).
pub fn monitor_down() -> Chunk {
    let mut p = Program::new("monitor-down");
    let child = p.function("child", 0, |f| {
        let ms = f.load_i32(40);
        f.sleep(ms);
        let z = f.load_i32(0);
        f.return_(z);
    });
    p.function("main", 0, |f| {
        let cap = f.spawn(child, &[]);
        let mon = f.monitor(cap);
        let msg = f.receive_match_imm(crate::TAG_SYS_DOWN);
        let id = f.hop_request_id(msg);
        let ok = f.eq(id, mon);
        let trap = f.label();
        f.branch_if_falsy(ok, trap);
        let out = f.hop_payload(msg);
        f.return_(out);
        f.bind(trap);
        f.trap(3);
    });
    p.build()
}

/// Client arity 1 (`r0` = server Cap) sends two hops; server drains both.
/// With mailbox capacity 1 the second send parks (`WAITING_SEND`).
pub fn waiting_send() -> Chunk {
    let mut p = Program::new("waiting-send");
    p.function("server", 0, |f| {
        let ms = f.load_i32(50);
        f.sleep(ms);
        let _first = f.receive_match_imm(TAG_REQ as u16);
        let second = f.receive_match_imm(TAG_REQ as u16);
        let out = f.hop_payload(second);
        f.return_(out);
    });
    p.function("client", 1, |f| {
        let server = f.reg(0);
        let req_id = f.load_i32(1);
        let one = f.load_i32(1);
        let first = f.hop(req_id, TAG_REQ, one);
        f.send(server, first);
        let forty_two = f.load_i32(42);
        let second = f.hop(req_id, TAG_REQ, forty_two);
        f.send(server, second);
        f.exit(second);
    });
    p.build()
}

/// Immediate `Trap` — used to show [`crate::Supervisor`] restart.
pub fn boom() -> Chunk {
    let mut p = Program::new("boom");
    p.function("boom", 0, |f| f.trap(1));
    p.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bytecode::CapId, decode, encode, std_native_table, verify, FlowOutcome, Message,
        QuotaConfig, Runtime, RuntimeConfig, TrustLevel, Value,
    };

    fn tiny(chunk: Chunk) -> Result<Runtime, crate::SpawnError> {
        Runtime::with_config(
            chunk,
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
                mailbox: crate::MailboxConfig::DEFAULT,
                ..Default::default()
            },
        )
    }

    fn tiny_natives(chunk: Chunk) -> Result<Runtime, crate::SpawnError> {
        Runtime::with_natives_and_config(
            chunk,
            std_native_table(),
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
                mailbox: crate::MailboxConfig::DEFAULT,
                ..Default::default()
            },
        )
    }

    #[test]
    fn add_forty_two_joins_42() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny(add_forty_two())?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(42))));
        Ok(())
    }

    #[test]
    fn ping_pong_joins_2() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = ping_pong();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes)?;
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        let sent = rt.metrics().messages_sent;
        rt.shutdown();
        assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(2))));
        assert!(sent >= 2);
        Ok(())
    }

    #[test]
    fn atomic_request_reply_joins_42() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = atomic_request_reply();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes)?;
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        let sent = rt.metrics().messages_sent;
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        assert!(sent >= 1);
        Ok(())
    }

    #[test]
    fn atomic_actors_two_clients_sum_72() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = atomic_actors();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes)?;
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        let sent = rt.metrics().messages_sent;
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(72))),
            "got {outcome:?}"
        );
        // 8 Ask + 8 reply per client, plus 2 DONE hops.
        assert!(sent >= 34);
        Ok(())
    }

    #[test]
    fn cap_in_payload_reissues_holder() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = cap_in_payload();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn cap_inside_a_list_is_reissued() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = cap_in_list();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn selective_receive_timeout_writes_unit() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = selective_receive_timeout();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Unit)),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn selective_receive_after_hits_tag() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = selective_receive_after();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn process_alive_tracks_live_then_down() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = process_alive();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(1))),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn process_dict_put_get_erase() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = process_dict();
        assert!(verify(&chunk).is_ok());
        let rt = tiny(chunk)?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(83))),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn receive_match_eq_skips_other_payload() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = receive_match_eq();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn exit_signal_down_is_fault() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = exit_signal();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(
                outcome,
                FlowOutcome::Completed(Value::Int(n))
                    if n == i64::from(crate::FlowExitReason::Fault as u8)
            ),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn supervised_tree_start_child_then_exit() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = supervised_tree();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let root = rt.function_index("root").ok_or("root")?;
        let sup = rt.supervisor()?;
        let outcome = sup
            .start_child(crate::ChildSpec::new("root", root).restart(crate::RestartPolicy::Never))?
            .join();
        sup.shutdown();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(1))),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn selective_receive_skips_junk_tag() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = selective_receive();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes)?;
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn receive_match_kind_skips_non_int_payload() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = receive_match_kind();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes)?;
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn ask_reply_joins_42() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = ask_reply();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes)?;
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        let sent = rt.metrics().messages_sent;
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        assert!(sent >= 2);
        Ok(())
    }

    #[test]
    fn log_sink_receives_the_ask_line_and_the_flow_still_returns_42(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::sync::{Arc, Mutex};

        #[derive(Debug)]
        struct MemSink {
            lines: Mutex<Vec<String>>,
        }

        impl crate::LogSink for MemSink {
            fn try_record(&self, line: &str) -> Result<(), crate::log::LogRecordError> {
                match self.lines.lock() {
                    Ok(mut lines) => {
                        lines.push(line.to_string());
                        Err(crate::log::LogRecordError)
                    }
                    Err(_) => Err(crate::log::LogRecordError),
                }
            }
        }

        let sink = Arc::new(MemSink {
            lines: Mutex::new(Vec::new()),
        });
        let rt = Runtime::with_natives_and_config(
            ask_reply(),
            std_native_table(),
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
                mailbox: crate::MailboxConfig::DEFAULT,
                log: Some(Arc::clone(&sink) as Arc<dyn crate::LogSink>),
                ..Default::default()
            },
        )?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        let lines = sink.lines.lock().map_err(|_| "log sink")?;
        let ask = lines.iter().find(|line| line.contains("ask from="));
        let Some(ask) = ask else {
            return Err(format!("missing ask line in {lines:?}").into());
        };
        assert!(ask.contains("from="), "{ask}");
        assert!(ask.contains("to="), "{ask}");
        assert!(ask.contains("req="), "{ask}");
        assert!(ask.contains("msg="), "{ask}");
        Ok(())
    }

    #[test]
    fn ask_timeout_writes_unit() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = ask_timeout_expires();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Unit)),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn ask_target_exit_writes_sys_exit() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = ask_target_exits();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(
                outcome,
                FlowOutcome::Completed(Value::Int(n)) if n == i64::from(crate::TAG_SYS_EXIT)
            ),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn server_loop_joins_42() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = server_loop();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn send_overwrites_forged_sender() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = forged_sender_send();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Pid(n)) => {
                assert_ne!(n, 999, "forged make_msg sender must not survive Send");
                assert!(n >= 1, "authenticated sender must be a live flow id");
                Ok(())
            }
            other => Err(format!("expected Completed(Pid), got {other:?}").into()),
        }
    }

    #[test]
    fn ask_overwrites_forged_request_sender() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = forged_sender_ask();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Pid(n)) => {
                assert_ne!(n, 999, "forged make_msg sender must not survive Ask");
                assert!(n >= 1, "authenticated sender must be a live flow id");
                Ok(())
            }
            other => Err(format!("expected Completed(Pid), got {other:?}").into()),
        }
    }

    #[test]
    fn send_scalar_target_traps() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("bad-cap-target");
        p.function("main", 0, |f| {
            let bad_cap = f.load_i32(99);
            let req_id = f.load_i32(1);
            let payload = f.load_i32(1);
            let msg = f.hop(req_id, TAG_PING, payload);
            f.send(bad_cap, msg);
            f.return_(msg);
        });
        let rt = tiny_natives(p.build())?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Failed(_)),
            "non-Cap Send target must fail, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn send_scalar_is_not_an_atomic_hop() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("bad-hop");
        p.function("main", 0, |f| {
            let cap = f.self_cap();
            let scalar = f.load_i32(99);
            f.send(cap, scalar);
            f.return_(scalar);
        });
        let rt = tiny(p.build())?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Failed(_)),
            "scalar Send must trap, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn monitor_down_joins_normal_reason() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = monitor_down();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(0))),
            "DOWN reason should be Normal (0), got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn waiting_send_second_hop_arrives() -> Result<(), Box<dyn std::error::Error>> {
        let cap = crate::MailboxCapacity::new(1).ok_or("cap")?;
        let rt = Runtime::with_natives_and_config(
            waiting_send(),
            std_native_table(),
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
                mailbox: crate::MailboxConfig::new(cap, crate::OverflowPolicy::Reject),
                ..Default::default()
            },
        )?;
        let server = rt.function_index("server").ok_or("server")?;
        let client = rt.function_index("client").ok_or("client")?;
        let server_h = rt.spawn(server, &[])?;
        let server_cap = rt.mint_cap(server_h.id())?;
        rt.spawn(client, &[Value::Cap(server_cap)])?;
        let outcome = server_h.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "second hop should be admitted after the first pop, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn link_kills_peer_on_fault() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("link-kill");
        p.function("park", 0, |f| {
            let _ = f.receive();
            f.trap(9);
        });
        p.function("boom", 0, |f| f.trap(1));
        let rt = tiny(p.build())?;
        let park = rt.function_index("park").ok_or("park")?;
        let boom = rt.function_index("boom").ok_or("boom")?;
        let parked = rt.spawn(park, &[])?;
        let killer = rt.spawn(boom, &[])?;
        rt.link(parked.id(), killer.id())?;
        let boom_out = killer.join();
        let park_out = parked.join();
        rt.shutdown();
        assert!(matches!(boom_out, FlowOutcome::Failed(_)), "{boom_out:?}");
        assert!(
            matches!(park_out, FlowOutcome::Failed(_)),
            "linked peer must die on fault, got {park_out:?}"
        );
        Ok(())
    }

    #[test]
    fn trap_exit_converts_link_kill_to_exit_hop() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("trap-exit");
        p.function("trapper", 0, |f| {
            let msg = f.receive_match_imm(crate::TAG_SYS_EXIT);
            let out = f.hop_payload(msg);
            f.return_(out);
        });
        p.function("boom", 0, |f| {
            let ms = f.load_i32(30);
            f.sleep(ms);
            f.trap(1);
        });
        let rt = tiny_natives(p.build())?;
        let trapper = rt.spawn(rt.function_index("trapper").ok_or("trapper")?, &[])?;
        let killer = rt.spawn(rt.function_index("boom").ok_or("boom")?, &[])?;
        rt.set_trap_exit(trapper.id(), true)?;
        rt.link(trapper.id(), killer.id())?;
        let boom_out = killer.join();
        let trap_out = trapper.join();
        rt.shutdown();
        assert!(matches!(boom_out, FlowOutcome::Failed(_)), "{boom_out:?}");
        assert!(
            matches!(
                trap_out,
                FlowOutcome::Completed(Value::Int(n))
                    if n == crate::FlowExitReason::Fault.as_u64() as i64
            ),
            "trap_exit peer must receive EXIT with Fault reason, got {trap_out:?}"
        );
        Ok(())
    }

    #[test]
    fn trap_exit_receives_normal_exit_signal() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("trap-exit-normal");
        p.function("trapper", 0, |f| {
            let msg = f.receive_match_imm(crate::TAG_SYS_EXIT);
            let out = f.hop_payload(msg);
            f.return_(out);
        });
        p.function("done", 0, |f| {
            let ms = f.load_i32(30);
            f.sleep(ms);
            let z = f.load_i32(0);
            f.return_(z);
        });
        let rt = tiny_natives(p.build())?;
        let trapper = rt.spawn(rt.function_index("trapper").ok_or("trapper")?, &[])?;
        let peer = rt.spawn(rt.function_index("done").ok_or("done")?, &[])?;
        rt.set_trap_exit(trapper.id(), true)?;
        rt.link(trapper.id(), peer.id())?;
        let _ = peer.join();
        let trap_out = trapper.join();
        rt.shutdown();
        assert!(
            matches!(
                trap_out,
                FlowOutcome::Completed(Value::Int(n))
                    if n == crate::FlowExitReason::Normal.as_u64() as i64
            ),
            "trap_exit must surface Normal as EXIT hop, got {trap_out:?}"
        );
        Ok(())
    }

    #[test]
    fn set_trap_exit_opcode_enables_trapping() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("trap-exit-opcode");
        // Parent waits for READY so SetTrapExit is known to have run before link.
        p.function("parent", 0, |f| {
            let msg = f.receive_match_imm(TAG_READY as u16);
            let out = f.hop_payload(msg);
            f.return_(out);
        });
        p.function("trapper", 1, |f| {
            let parent = f.reg(0);
            let on = f.load_i32(1);
            f.set_trap_exit(on);
            let z = f.load_i32(1);
            let ready = f.hop_fresh(TAG_READY, z);
            f.send(parent, ready);
            let msg = f.receive_match_imm(crate::TAG_SYS_EXIT);
            let out = f.hop_payload(msg);
            f.return_(out);
        });
        p.function("boom", 0, |f| {
            // Stay parked until the host wakes us *after* the link is installed.
            let _wake = f.receive_match_imm(TAG_READY as u16);
            f.trap(1);
        });
        let rt = tiny_natives(p.build())?;
        let parent = rt.spawn(rt.function_index("parent").ok_or("parent")?, &[])?;
        let parent_cap = rt.mint_cap(parent.id())?;
        let trapper = rt.spawn(
            rt.function_index("trapper").ok_or("trapper")?,
            &[Value::Cap(parent_cap)],
        )?;
        let killer = rt.spawn(rt.function_index("boom").ok_or("boom")?, &[])?;
        let ready = parent.join();
        assert!(
            matches!(ready, FlowOutcome::Completed(Value::Int(1))),
            "trapper must signal READY after SetTrapExit, got {ready:?}"
        );
        rt.link(trapper.id(), killer.id())?;
        rt.send(
            killer.id(),
            Value::Message(Message::request(0, TAG_READY as u16, 0)),
        )?;
        let boom_out = killer.join();
        let trap_out = trapper.join();
        rt.shutdown();
        assert!(matches!(boom_out, FlowOutcome::Failed(_)), "{boom_out:?}");
        assert!(
            matches!(
                trap_out,
                FlowOutcome::Completed(Value::Int(n))
                    if n == crate::FlowExitReason::Fault.as_u64() as i64
            ),
            "bytecode SetTrapExit must receive EXIT with Fault reason, got {trap_out:?}"
        );
        Ok(())
    }

    #[test]
    fn linked_exit_down_carries_link_reason() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("link-down-reason");
        p.function("watcher", 0, |f| {
            let msg = f.receive_match_imm(crate::TAG_SYS_DOWN);
            let out = f.hop_payload(msg);
            f.return_(out);
        });
        p.function("park", 0, |f| {
            let _ = f.receive();
            f.trap(9);
        });
        p.function("boom", 0, |f| f.trap(1));
        let rt = tiny_natives(p.build())?;
        let watcher = rt.spawn(rt.function_index("watcher").ok_or("watcher")?, &[])?;
        let parked = rt.spawn(rt.function_index("park").ok_or("park")?, &[])?;
        let killer = rt.spawn(rt.function_index("boom").ok_or("boom")?, &[])?;
        rt.monitor(watcher.id(), parked.id())?;
        rt.link(parked.id(), killer.id())?;
        let _ = killer.join();
        let watched = watcher.join();
        let _ = parked.join();
        rt.shutdown();
        assert!(
            matches!(
                watched,
                FlowOutcome::Completed(Value::Int(n)) if n == crate::FlowExitReason::Link.as_u64() as i64
            ),
            "DOWN payload must be Link, got {watched:?}"
        );
        Ok(())
    }

    #[test]
    fn monitor_dead_owner_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny(add_forty_two())?;
        let first = rt.spawn(0, &[])?;
        let dead = first.id();
        let done = first.join();
        assert!(matches!(done, FlowOutcome::Completed(_)));
        let live = rt.spawn(0, &[])?;
        let err = rt.monitor(dead, live.id());
        live.join();
        rt.shutdown();
        assert!(
            matches!(err, Err(crate::LifecycleError::NoSuchFlow(_))),
            "{err:?}"
        );
        Ok(())
    }

    #[test]
    fn forged_cap_cannot_be_registered() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny(add_forty_two())?;
        let err = rt.register_name("svc", CapId::from_raw(99_999));
        rt.shutdown();
        assert_eq!(err, Err(crate::LifecycleError::InvalidCapability));
        Ok(())
    }

    #[test]
    fn bytecode_register_name_joins() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("reg-self");
        p.function("main", 0, |f| {
            let name = f.load_str("svc");
            f.register_name(name);
            let z = f.load_i32(1);
            f.return_(z);
        });
        let rt = tiny(p.build())?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(1))),
            "host-spawned flow must be able to register_name, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn named_service_whereis_returns_cap() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = named_service();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt
            .spawn(idx, &[])?
            .join_timeout(std::time::Duration::from_secs(2))
            .ok_or("named_service timed out")?;
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "named whereis should yield a usable SEND Cap, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn confined_spawn_cannot_register_name() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("confined-reg");
        let child = p.function("child", 0, |f| {
            let name = f.load_str("stolen");
            f.register_name(name);
            // Stay alive so a successful register would remain visible.
            // Sleep does not require RECV (a confined child has no RECV).
            let ms = f.load_i32(5_000);
            f.sleep(ms);
            let z = f.load_i32(1);
            f.return_(z);
        });
        p.function("main", 0, |f| {
            let _c = f.spawn_confined(child, &[]);
            let ms = f.load_i32(80);
            f.sleep(ms);
            let name = f.load_str("stolen");
            let cap = f.whereis(name);
            let ok = f.label();
            f.branch_if_falsy(cap, ok);
            let one = f.load_i32(1);
            f.return_(one);
            f.bind(ok);
            let zero = f.load_i32(0);
            f.return_(zero);
        });
        let rt = tiny(p.build())?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(0))),
            "confined child must not publish a name, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn heap_quota_rejects_oversized_str() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("heap-str");
        p.function("main", 0, |f| {
            let s = f.load_str("x".repeat(200));
            f.return_(s);
        });
        let mut quota = QuotaConfig::permissive();
        quota.mem_limit = 64;
        let rt = Runtime::with_config(
            p.build(),
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
                mailbox: crate::MailboxConfig::DEFAULT,
                quota,
                ..Default::default()
            },
        )?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Failed(_)),
            "200-byte Str must exceed 64-byte heap quota, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn heap_quota_rejects_oversized_bytes() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("heap-bytes");
        p.function("main", 0, |f| {
            let b = f.load_bytes(vec![0u8; 200]);
            f.return_(b);
        });
        let mut quota = QuotaConfig::permissive();
        quota.mem_limit = 64;
        let rt = Runtime::with_config(
            p.build(),
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
                mailbox: crate::MailboxConfig::DEFAULT,
                quota,
                ..Default::default()
            },
        )?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Failed(_)),
            "200-byte Bytes must exceed 64-byte heap quota, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn send_does_not_charge_a_held_payload_twice() -> Result<(), Box<dyn std::error::Error>> {
        let payload_len = 200usize;
        let hop = Value::Message(Message::new(0, 1, 1, Value::str("x".repeat(payload_len))));
        let inflight = crate::vm::in_flight_hop_charge(&hop);
        // `hop_fresh` keeps the source string and a copy in the make_msg
        // window, then the message itself. Peak is those three. After the
        // source is overwritten, the window copy and the message remain.
        let peak = payload_len * 3;
        let held_at_send = payload_len * 2;
        let limit = peak.max(held_at_send + inflight);
        assert!(
            held_at_send + hop.memory_size() > limit,
            "limit must reject a send that charges the payload a second time"
        );

        let mut p = Program::new("send-charge");
        let child = p.function("child", 0, |f| {
            let _msg = f.receive_match_imm(1);
            let one = f.load_i32(1);
            f.return_(one);
        });
        p.function("main", 0, |f| {
            let to = f.spawn(child, &[]);
            let payload = f.load_str("x".repeat(payload_len));
            let msg = f.hop_fresh(1, payload);
            let zero = f.load_i32(0);
            f.mov(payload, zero);
            f.send(to, msg);
            let one = f.load_i32(1);
            f.return_(one);
        });
        let mut quota = QuotaConfig::permissive();
        quota.mem_limit = limit;
        let rt = Runtime::with_natives_and_config(
            p.build(),
            std_native_table(),
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
                mailbox: crate::MailboxConfig::DEFAULT,
                quota,
                ..Default::default()
            },
        )?;
        let main = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(main, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(1))),
            "send of a held payload must fit, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn host_send_stamps_sender_zero() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("host-send-sender");
        p.function("main", 0, |f| {
            let msg = f.receive();
            let sender = f.hop_sender(msg);
            f.return_(sender);
        });
        let rt = tiny_natives(p.build())?;
        let h = rt.spawn(0, &[])?;
        // Non-zero request_id so minting is not under test here.
        rt.send(h.id(), Value::Message(Message::request(7, 1, 0)))?;
        let outcome = h.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Pid(0))),
            "host send must stamp sender=0 (Pid), got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn host_send_keeps_a_nested_sender_at_zero() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("host-nested-sender");
        p.function("main", 0, |f| {
            let msg = f.receive();
            let inner = f.hop_payload(msg);
            let sender = f.hop_sender(inner);
            f.return_(sender);
        });
        let rt = tiny_natives(p.build())?;
        let h = rt.spawn(0, &[])?;
        let inner = Message::request(0, 2, 1);
        rt.send(h.id(), Value::Message(Message::request(7, 1, inner)))?;
        let outcome = h.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Pid(0))),
            "a host nested envelope stays sender 0, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn fresh_nested_envelope_is_stamped_with_the_sender() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut p = Program::new("nested-stamp");
        let server = p.function("server", 0, |f| {
            let msg = f.receive_match_imm(2);
            let inner = f.hop_payload(msg);
            let who = f.hop_sender(inner);
            f.send_reply(msg, 1, who);
            f.return_(who);
        });
        p.function("main", 0, |f| {
            let server = f.spawn(server, &[]);
            let body = f.load_i32(1);
            let inner = f.hop_fresh(1, body);
            let outer = f.hop_fresh(2, inner);
            let reply = f.ask(server, outer);
            let who = f.hop_payload(reply);
            f.return_(who);
        });
        let rt = tiny_natives(p.build())?;
        let main = rt.function_index("main").ok_or("main")?;
        let h = rt.spawn(main, &[])?;
        let me = h.id().as_u64();
        let outcome = h.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Pid(id)) if id == me),
            "unset nested sender must become the sending flow, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn forwarded_envelope_keeps_its_authenticated_sender() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut p = Program::new("nested-forward");
        let checker = p.function("checker", 0, |f| {
            let msg = f.receive_match_imm(2);
            let inner = f.hop_payload(msg);
            let who = f.hop_sender(inner);
            f.send_reply(msg, 1, who);
            f.return_(who);
        });
        let relay = p.function("relay", 1, |f| {
            let checker = f.reg(0);
            let incoming = f.receive_match_imm(1);
            let wrapped = f.hop_fresh(2, incoming);
            let reply = f.ask(checker, wrapped);
            let who = f.hop_payload(reply);
            f.send_reply(incoming, 1, who);
            f.return_(who);
        });
        p.function("main", 0, |f| {
            let checker = f.spawn(checker, &[]);
            let relay = f.spawn(relay, &[checker]);
            let body = f.load_i32(1);
            let req = f.hop_fresh(1, body);
            let reply = f.ask(relay, req);
            let who = f.hop_payload(reply);
            f.return_(who);
        });
        let rt = tiny_natives(p.build())?;
        let main = rt.function_index("main").ok_or("main")?;
        let h = rt.spawn(main, &[])?;
        let me = h.id().as_u64();
        let outcome = h.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Pid(id)) if id == me),
            "forwarding must keep the already stamped sender, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn refused_cap_does_not_spend_a_send_token() -> Result<(), Box<dyn std::error::Error>> {
        use crate::bytecode::builder::ChunkBuilder;
        let mut b = ChunkBuilder::new("quota-cap");
        b.begin_function("main", 0, 8);
        let forged = b.const_(Value::Cap(CapId::from_raw(1)));
        b.emit_self_pid(0);
        b.emit_load_imm(1, 1);
        b.emit_load_imm(2, 1);
        b.emit_load_const(3, forged);
        b.emit_call_native(1, 2, 3);
        b.emit_send(0, 1);
        b.emit_return(0);
        let rt = Runtime::with_natives_and_config(
            b.finish(),
            std_native_table(),
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
                trust: TrustLevel::Trusted,
                quota: QuotaConfig {
                    send_burst: 0,
                    send_per_sec: 0,
                    ..QuotaConfig::permissive()
                },
                ..RuntimeConfig::default()
            },
        )?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Failed(reason) => {
                if reason.contains("send rate") {
                    return Err(format!("quota ran before the grant check: {reason}").into());
                }
                if !reason.contains("capability") {
                    return Err(format!("expected a capability refusal, got {reason}").into());
                }
                Ok(())
            }
            other => Err(format!("expected a failed grant, got {other:?}").into()),
        }
    }

    #[test]
    fn spawn_arg_envelope_is_stamped_with_the_parent() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("spawn-stamp");
        let child = p.function("child", 2, |f| {
            let parent = f.reg(0);
            let msg = f.reg(1);
            let who = f.hop_sender(msg);
            let reply = f.hop_fresh(1, who);
            f.send(parent, reply);
            f.return_(who);
        });
        p.function("main", 0, |f| {
            let me = f.self_cap();
            let body = f.load_i32(1);
            let msg = f.hop_fresh(1, body);
            let _child = f.spawn(child, &[me, msg]);
            let got = f.receive();
            let who = f.hop_payload(got);
            f.return_(who);
        });
        let rt = tiny_natives(p.build())?;
        let main = rt.function_index("main").ok_or("main")?;
        let h = rt.spawn(main, &[])?;
        let me = h.id().as_u64();
        let outcome = h.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Pid(id)) if id == me),
            "a spawn argument with sender 0 must become the parent, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn host_send_mints_request_id() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("host-send-rid");
        p.function("main", 0, |f| {
            let msg = f.receive();
            let rid = f.hop_request_id(msg);
            f.return_(rid);
        });
        let rt = tiny_natives(p.build())?;
        let h = rt.spawn(0, &[])?;
        rt.send(h.id(), Value::Message(Message::request(0, 1, 0)))?;
        let outcome = h.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(n)) if n > 0),
            "host send must mint a non-zero request_id when unset, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn hop_fresh_mints_distinct_request_ids() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("fresh-ids");
        p.function("main", 0, |f| {
            let z = f.load_i32(0);
            let a = f.hop_fresh(TAG_REQ, z);
            let b = f.hop_fresh(TAG_REQ, z);
            let ida = f.hop_request_id(a);
            let idb = f.hop_request_id(b);
            let same = f.eq(ida, idb);
            let ok = f.label();
            let miss = f.label();
            // Distinct ids → eq is falsy → branch to ok.
            f.branch_if_falsy(same, ok);
            f.trap(1);
            f.bind(ok);
            f.branch_if_falsy(ida, miss);
            f.branch_if_falsy(idb, miss);
            f.return_(ida);
            f.bind(miss);
            let neg = f.load_i32(-1);
            f.return_(neg);
        });
        let rt = tiny_natives(p.build())?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(n)) if n > 0),
            "hop_fresh must mint two distinct non-zero request_ids, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn ask_reply_preserves_request_id() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("ask-corr");
        let server = p.function("server", 0, |f| {
            f.actor_loop(TAG_REQ as u16, |f, msg| {
                let payload = f.hop_payload(msg);
                f.add_imm(payload, 1);
                f.send_reply(msg, TAG_REP, payload);
            });
        });
        p.function("main", 0, |f| {
            let server_cap = f.spawn(server, &[]);
            let payload = f.load_i32(41);
            let req = f.hop_fresh(TAG_REQ, payload);
            let expect = f.hop_request_id(req);
            let reply = f.ask(server_cap, req);
            // Correlate explicitly: wait would have already matched by Ask;
            // assert the delivered reply carries the same request_id.
            let got = f.hop_request_id(reply);
            let same = f.eq(expect, got);
            let bad = f.label();
            f.branch_if_falsy(same, bad);
            let out = f.hop_payload(reply);
            f.return_(out);
            f.bind(bad);
            let neg = f.load_i32(-1);
            f.return_(neg);
        });
        let rt = tiny_natives(p.build())?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt
            .spawn(idx, &[])?
            .join_timeout(std::time::Duration::from_secs(2))
            .ok_or("ask_reply_preserves_request_id timed out")?;
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "Ask reply must echo request_id and payload+1, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn send_receive_match_corr_binds_request_id() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = atomic_request_reply();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt
            .spawn(idx, &[])?
            .join_timeout(std::time::Duration::from_secs(2))
            .ok_or("atomic_request_reply timed out")?;
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "ReceiveMatchCorr must bind the minted request_id, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn registry_clears_on_exit() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("reg");
        p.function("main", 0, |f| {
            let ms = f.load_i32(80);
            f.sleep(ms);
            let z = f.load_i32(1);
            f.return_(z);
        });
        let rt = tiny(p.build())?;
        let h = rt.spawn(0, &[])?;
        let cap = rt.mint_cap(h.id())?;
        rt.register_name("svc", cap)?;
        assert_eq!(rt.whereis("svc")?, Some(cap));
        let _ = h.join();
        assert_eq!(rt.whereis("svc")?, None);
        rt.shutdown();
        Ok(())
    }
}
