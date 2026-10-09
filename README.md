# super-visor

Erlang/OTP-style supervision trees for long-running tokio processes.

A `Supervisor` starts its children in order, restarts them by policy when they exit,
gives up when restarts happen too often, and stops its children in reverse order.
A supervisor is itself a managed process, so supervisors nest into a tree: when a
supervisor gives up, its parent sees one failed child and applies its own policy.

tokio already supplies the mechanism (tasks, cancellation, abort). This crate adds
the OTP policy on top of it.

## Concepts

| OTP | super-visor |
|---|---|
| child spec | `ChildSpec` |
| `{M, F, A}` start function | factory closure (`ChildSpec::new`) |
| `init/1` that `start_link` waits for | async factory (`ChildSpec::with_init`) |
| `permanent` / `transient` / `temporary` | `Restart::{Permanent, Transient, Temporary}` |
| `shutdown`: timeout / `brutal_kill` / `infinity` | `Shutdown::{Timeout, Kill, Infinity}` |
| `one_for_one` / `one_for_all` / `rest_for_one` | `Strategy::{OneForOne, OneForAll, RestForOne}` |
| `intensity` / `period` | `Intensity { max_restarts, period }` (default 1 in 5 s) |
| `auto_shutdown` + `significant` | `AutoShutdown` + `ChildSpec::significant` |
| SASL supervisor reports | `on_event` hook, and `tracing` events with the `tracing` feature |
| `supervisor:start_child` and the other `*_child` calls | `SupervisorHandle` |
| `simple_one_for_one` / Elixir `DynamicSupervisor` | `DynamicSupervisor` |
| normal exit / crash | `Ok(())` / `Err(_)` or a panic |

Not included: hot code upgrade, distribution, and a global process registry.

### Exits and restarts

- A child that returns `Ok(())` exited normally. `Err` or a panic is an abnormal exit.
- `Permanent` children restart after any exit, `Transient` children only after an
  abnormal exit, and `Temporary` children never.
- A child that exits after the supervisor signaled shutdown is never restarted.
- Each restart counts toward the intensity. When there are more than `max_restarts`
  restarts within `period`, the supervisor stops all children in reverse order and
  returns `SupervisorError::MaxIntensity`. Its parent then treats it as a failed child.
- `Backoff` adds a delay before a restart. Erlang has no delay; the default is none.

### Start order

Children start one at a time, in the order they were added. The factory of a
`with_init` child runs to completion before the next child starts, so the child can
bind a socket or open a connection first. An `Err` from the first start stops the
children that already started and returns `SupervisorError::StartFailed`; it does not
restart. An `Err` from a later restart counts as an abnormal exit.

### Shutdown

On SIGTERM or Ctrl+C (`start`), or when the parent supervisor stops it, the supervisor
stops its children one at a time in reverse start order. Each child gets its shutdown
signal and its `Shutdown` limit (default 5 s); after that the supervisor drops the
child's future. Use `super_visor::spawn` in `run_proc` so that this drop aborts the task.
Use `Shutdown::Infinity` (`ChildSpec::supervisor` sets it) for a nested supervisor, so
its own children have time to stop in order.

## Example

```rust,no_run
use std::time::Duration;
use super_visor::{
    ChildSpec, Intensity, ManagedFuture, ManagedProc, ShutdownSignal, Strategy, Supervisor,
};
use tokio::net::TcpListener;

struct Acceptor {
    listener: TcpListener,
}

impl Acceptor {
    async fn run(self, mut shutdown: ShutdownSignal) -> std::io::Result<()> {
        loop {
            tokio::select! {
                _ = &mut shutdown => return Ok(()),
                accepted = self.listener.accept() => drop(accepted?),
            }
        }
    }
}

impl ManagedProc for Acceptor {
    fn run_proc(self: Box<Self>, shutdown: ShutdownSignal) -> ManagedFuture {
        super_visor::spawn(self.run(shutdown))
    }
}

struct Heartbeat;

impl ManagedProc for Heartbeat {
    fn run_proc(self: Box<Self>, mut shutdown: ShutdownSignal) -> ManagedFuture {
        super_visor::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tokio::select! {
                    _ = &mut shutdown => return Ok::<_, std::io::Error>(()),
                    _ = tick.tick() => println!("alive"),
                }
            }
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), super_visor::SupervisorError> {
    Supervisor::builder()
        .name("root")
        // The heartbeat depends on the acceptor: if the acceptor restarts, so does it.
        .strategy(Strategy::RestForOne)
        .intensity(Intensity::new(3, Duration::from_secs(60)))
        .on_event(|event| eprintln!("{event:?}"))
        .child(ChildSpec::with_init("acceptor", || async {
            let listener = TcpListener::bind("127.0.0.1:8080").await?;
            Ok::<_, std::io::Error>(Acceptor { listener })
        }))
        .child(ChildSpec::new("heartbeat", || Heartbeat))
        .build()
        .start()
        .await
}
```

## Changing children at runtime

`Supervisor::handle()` returns a `SupervisorHandle`. Its calls match the OTP functions:

| Call | Effect |
|---|---|
| `start_child(spec)` | Adds the child after the existing children and waits for its init. A failed start does not keep the spec. |
| `terminate_child(id)` | Stops the child by its `Shutdown` policy and keeps the spec (a `Temporary` spec is removed). It does not cause an auto shutdown. |
| `restart_child(id)` | Starts a terminated child again. This does not count toward the intensity. |
| `delete_child(id)` | Removes the spec of a child that is not running. |
| `which_children()`, `count_children()` | Report the children and their status. |

Calls made before the supervisor starts wait until it runs. After it stops, calls fail
with `HandleError::NotRunning`. A child supervisor that restarts is a new instance, so
create the handle once and give it to each instance from the factory:

```rust,no_run
use super_visor::{ChildSpec, Supervisor, SupervisorHandle};

let workers = SupervisorHandle::new();
let root = Supervisor::builder().child(ChildSpec::supervisor("workers", {
    let workers = workers.clone();
    move || Supervisor::builder().handle(workers.clone()).build()
}));
```

## Dynamic supervisor

A `DynamicSupervisor` starts with no children. You add them through its handle, for
example one child for each connection or each key. Each child runs in its own task and
restarts by itself (one-for-one), with the same restart types and intensity. Child ids
must be unique among its current children. A child that ends for good is removed.
`max_children` sets a limit. On shutdown all children stop at the same time, each with
its own `Shutdown` limit. `restart_child` and `delete_child` return
`HandleError::NotSupported`, as in OTP.

```rust
use super_visor::{
    CancellationToken, ChildSpec, DynamicSupervisor, ManagedProc, ProcResult, ShutdownSignal,
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let pool = DynamicSupervisor::builder().max_children(100).build();
    let handle = pool.handle();
    let token = CancellationToken::new();
    let running = tokio::spawn(Box::new(pool).run_proc(ShutdownSignal::new(token.clone())));

    for key in ["alpha", "beta"] {
        let worker = || |shutdown: ShutdownSignal| async move {
            shutdown.await;
            ProcResult::Ok(())
        };
        handle.start_child(ChildSpec::new(key, worker)).await.unwrap();
    }
    assert_eq!(handle.count_children().await.unwrap().active, 2);

    token.cancel();
    running.await.unwrap().unwrap();
}
```

## Tracing

With the `tracing` feature, each supervisor event is also a `tracing` event with the
fields `supervisor`, `child` and, where it applies, `error`, `delay_ms` or `timeout_ms`.
The messages do not change, so log alerts can match on them:

| Message | Level |
|---|---|
| `child started` | INFO |
| `child exited` | INFO (normal exit), WARN (with `error`) |
| `child failed to start` | ERROR |
| `child restarting` | INFO |
| `child killed after shutdown timeout` | WARN |
| `supervisor reached max restart intensity` | ERROR |

## Migrating from 0.3

- `add_proc` and `add` keep the 0.3 behavior. The child is once-only, so any restart
  that needs it exceeds the intensity: the first `Err` stops all children in reverse
  order. The supervisor returns when all such children have exited.
- That first `Err` now arrives as `SupervisorError::MaxIntensity { id, source }`, with
  the child's own error as `source`. Errors from children during shutdown arrive as
  `SupervisorError::Child { id, source }`. `SupervisorError` is `#[non_exhaustive]`.
- Dropping the future from `super_visor::spawn` now aborts the task.
- A panic in a child is an abnormal exit (`SupervisorError::Panic`), also in
  `super_visor::run` futures.
- A supervisor with no children now runs until shutdown is signaled.
