# byteflow-io

Host-side I/O integration for [`byteflow-actors`](https://github.com/mchael158/byteflow-actors)
(Rust import: `byteflow`).

This crate keeps device and protocol concerns **outside** the Byteflow VM.
It does not add I/O opcodes, open sockets, or embed MQTT, UART, GPIO, or
payment SDKs. The host application owns those systems and plugs them in
through the contracts below.

```text
bytecode / CallNative / HostAwait
              │
              ▼
         byteflow-io          ← bounded MPMC queues, natives 8..=10, HostAwait workers
              │
              ▼
      host IoDriver           ← MQTT, UART, GPIO, payments, ESP, …
```

## What it provides

| Surface | Role |
|---------|------|
| Fast `CallNative` adapters | Validate arguments and enqueue into a bounded host queue |
| [`WorkerHostAwaitBridge`](src/bridge.rs) | MPMC HostAwait worker pool (true parallelism for `N > 1`) |
| [`EventRouter`](src/events.rs) | Inject external events as authenticated `Value::Message` hops |
| [`build_native_table`](src/native_table.rs) | Preserve Byteflow slots `0..=7` and append I/O slots `8..=10` |
| [`QueuedIoDriver`](src/queue.rs) | Ready-made bounded command queue for the fast path |
| [`IoMetrics`](src/metrics.rs) | Submit / full / settle counters for bridge and queue |
| [`fleet`](src/fleet) | Generic device fleet + [`bootstrap_composite`](src/fleet/mod.rs) (queue + HostAwait) |
| [`laundry`](src/laundry) | Optional washer profile (topics / naming) over `fleet` |

Dependency direction is one-way: `byteflow-io` → `byteflow`. The core never
depends on this crate.

## Compatibility

**Rust 1.99.1+** e **`byteflow-actors` 0.9.21+** (import `byteflow`).

O repositório inclui [`rust-toolchain.toml`](rust-toolchain.toml) (`channel = "1.99.1"`);
dentro da pasta o `cargo`/`rustc` usam essa versão automaticamente via rustup.

Globalmente:

```powershell
rustup default 1.99.1-x86_64-pc-windows-msvc
# ou, quando `rustup update stable` funcionar de novo:
rustup update stable && rustup default stable
rustc --version   # >= 1.99.1
```

Se `rustup update stable` falhar por `rust-src` corrompido, instale ou selecione
`1.99.1` como acima; paliativo: `cargo build --ignore-rust-version`.

```toml
[dependencies]
byteflow-actors = "0.9.21"
byteflow-io = "0.1"
```

Inside the **Bytecode** monorepo (`mchael158/byteflow-actors`), `byteflow-io` lives at
`crates/byteflow-io` and depends on `path = "../byteflow"`. CI runs
`cargo test --workspace`, so integration always validates against **local core HEAD**.

For a standalone host crate:

```toml
[dependencies]
byteflow-actors = { path = "../byteflow" }
byteflow-io = { path = "../byteflow-io" }
```

## Quick start

```rust
use std::sync::Arc;
use byteflow::{NullSink, Runtime};
use byteflow_io::{build_native_table, ByteflowIo, IoConfig};

let output = Arc::new(NullSink);
let io = ByteflowIo::new(driver, IoConfig::default())?;
let natives = build_native_table(io.driver().clone(), output.clone())?;
let config = io.runtime_config(output);
let runtime = Runtime::with_natives_and_config(chunk, natives, config)?;

// … host application …

runtime.shutdown();
io.shutdown()?;
```

Shutdown order matters:

1. Stop external callbacks and host-owned device consumer loops
   (`recv_timeout` / `try_recv` plus an application stop signal).
2. Call `Runtime::shutdown()` — cancels outstanding HostAwait tickets.
3. Join device threads, then call `ByteflowIo::shutdown()`.

## Fast natives

| Index | Name | Arguments | Result |
|------:|------|-----------|--------|
| 8 | `io_gpio_write` | `(pin: Int, high: Bool/Int)` | host command ID |
| 9 | `io_mqtt_publish_fast` | `(topic: Str, payload: Value, qos: Int)` | host command ID |
| 10 | `io_uart_write_fast` | `(port: Str, bytes: Bytes)` | host command ID |

Each native calls only [`IoDriver::try_submit`](src/driver.rs). The driver must
enqueue and return immediately. The returned ID means *accepted by the host
queue*, not *device operation completed*.

For host-specific fast commands that are not one of the built-in natives, enqueue
[`IoCommand::Custom`](src/driver.rs) from host code.

## HostAwait operations

Sample application operation IDs live in [`samples::host_op`](src/samples.rs)
(non-normative). Generic aliases (`START_ACTION_AND_WAIT`, `APPLY_CREDIT`, …)
share the same numeric ids as the laundry-oriented names. Hosts should define
their own stable discriminators when shipping independent `.bf` modules.

Implement [`IoDriver::execute_host_await`](src/driver.rs) (or
[`HostAwaitHandler`](src/queue.rs)). Work runs on a `byteflow-io` worker
thread — never on a Byteflow scheduler worker. Apply finite deadlines and
poll [`CancellationToken`](src/bridge.rs) between blocking steps.

The HostAwait pool is an **MPMC** bounded queue: every idle worker may take
the next job. `IoConfig::worker_threads > 1` is true parallel execution.

## Inbound events

```rust
use std::sync::Arc;
use byteflow::{Runtime, Value};
use byteflow_io::samples::event_tag;
use byteflow_io::EventRouter;

// Retain Arc so device callbacks on other threads can deliver events.
let events = EventRouter::new(Arc::clone(&runtime));
events.send(
    target_flow,
    1001,
    event_tag::COIN_INSERTED,
    Value::Int(500),
)?;
```

`EventRouter` wraps the payload in `Message::request` and calls
`Runtime::send`. Bare scalars are never delivered on the wire path.
`Runtime::shutdown` takes ownership; drop all `Arc` clones (or
`Arc::try_unwrap`) when tearing down.

## Queued driver

[`QueuedIoDriver`](src/queue.rs) implements the fast path with a bounded
channel. The host drains [`IoCommandReceiver`](src/queue.rs) on its own
device thread. A full queue fails immediately; the native surfaces that as a
Byteflow `Fault`.

`QueuedIoDriver::shutdown` takes the sender so later submits fail closed even
under races. It does not join the application’s device consumer thread.

Command IDs increase from `1` but may contain **gaps** after a failed enqueue.
Treat them as opaque correlation tokens, not dense counters.

Inspect [`QueuedIoDriver::metrics`](src/queue.rs) /
[`ByteflowIo::metrics`](src/lib.rs) for submit / full / settle counters.

## Design constraints

- No new I/O opcodes; the VM never opens sockets, files, UART, or GPIO.
- `CallNative` must not wait on network, brokers, devices, or long locks.
- HostAwait workers are bounded MPMC; queue saturation fails closed.
- Completions use Byteflow’s `finish_host_await` settle path.
- Native slots `0..=7` are append-only ABI; I/O natives start at `8`.
- Access remains gated by `CapRights::NATIVE` and `NativeMask` in the core.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full contract map.

## Device fleet (generic, low-cost)

[`fleet`](src/fleet) is product-agnostic: lockers, sensors, kiosks, washers —
same contracts, different [`FleetProfile`](src/fleet/profile.rs) (capacity,
topic root, label). ESP32 is an optional edge MCU, not a hard dependency.

- No MQTT/hardware SDK in this crate (keeps host cost low)
- Default `InMemoryDeviceTransport` for tests / dry-runs
- One `FleetRegistry` + `FleetDriver` per site
- HostAwait: arm/credit, start action, OTA chunk, MQTT wait-ACK
- [`bootstrap_composite`](src/fleet/mod.rs): fast queue + fleet HostAwait on one [`ByteflowIo`]
- [`dispatch_queued_command`](src/fleet/dispatch.rs): **required** on the device thread so MQTT/fleet `Custom` reach the edge transport

```rust
use byteflow_io::fleet::{
    bootstrap_composite, dispatch_queued_command, FleetProfile, DEFAULT_FAST_QUEUE_CAPACITY,
};
use byteflow_io::IoConfig;

let (io, fleet, device_rx, fleet_driver) =
    bootstrap_composite(FleetProfile::generic_site(), IoConfig::default(), DEFAULT_FAST_QUEUE_CAPACITY)?;
// On your device thread:
while let Ok(queued) = device_rx.try_recv() {
    match dispatch_queued_command(&fleet_driver, queued)? { /* GPIO vs fleet */ }
}
```

HostAwait must run through the runtime bridge (`f.host_await` / [`ByteflowIo::runtime_config`]), not by calling [`IoDriver::execute_host_await`] on the main thread.

For HostAwait-only sites (no GPIO/UART natives), [`fleet::bootstrap`](src/fleet/mod.rs) is enough.

### Simulador ESP32 no PC (sem hardware)

[`InMemoryDeviceTransport`](src/fleet/transport.rs) imita ACK/estado do firmware; [`esp32_sim`](src/fleet/esp32_sim.rs) roda a thread “sketch” que drena a fila composite. Exemplo:

```sh
cargo run --example esp32_simulator
```

Com **Wokwi/QEMU** e firmware real: Byteflow fica no PC; o ESP publica MQTT no broker; você implementa [`DeviceTransport`](src/fleet/transport.rs) apontando para esse broker (sem mudar o runtime).

[`laundry`](src/laundry) is only a thin washer profile (`MachineId`,
`laundry/washer/…` topics) on top of `fleet` — not the core of the crate.

## Development

```sh
cargo test --all-targets
cargo run --example host_await_echo
cargo run --example fleet_two_washers
cargo run --example fleet_queued_host
cargo run --example esp32_simulator
```

Default tests use in-memory fakes (queue full, cancel, panic, parallel
workers, EventRouter, 56-washer fleet). Hardware-in-the-loop adapters belong
in the host application or optional feature crates.

## License

Licensed under either of

- Apache License, Version 2.0, or
- MIT license

at your option.
