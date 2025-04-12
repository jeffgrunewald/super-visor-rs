# super-visor

super-visor is a simple crate for orchestrating an ordered startup and shutdown of long-running tokio processes/

Modeled after the application behavior in Erlang/Elixir, it's intention is to allow operators of Rust binary crates to control the order in which daemonized processes and tasks are run as well as stopped, such that runtime dependencies reflected in the process management.

It is based on the `task-manager` crate from the Helium Oracles project but removes the external dependency on the `triggered` crate to simplify the dependency requirements as well as provide the more granular control for future process tree strategies provided by tokio's native `CancellationToken` type.


Any tokio process can be converted to a super-visor-managed process by having the central type implement the `ManagedProc` trait. This trait requires the `run_proc` function which takes a boxed `Self` and a `CancellationToken` and expects the implementer to set the process to listen for the token to be cancelled and take appropriate action to gracefully stop its work and return a `anyhow::Result<()>` to the caller.
A Supervisor can itself be a supervised process, allowing for process grouping and nested orchestration.
Finally, the application's `#[tokio::main]` function should utilize the `SupervisorBuilder` to construct a root process supervisor and then `start()` it, which will start all the managed processes added to its managed procs list in the order defined, and continue to drive them to completion (or indefinitely for servers) until the root process receives a `ctrl_c` or a `sigterm` from the OS.

## Example

```rust
struct AxumServer {
  listen_addr: SocketAddr,
  ...
}

impl ManagedProc for AxumServer {
    async fn run_proc(self: Box<Self>, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        ...do some setup stuff...

        Box::pin(axum::serve(
            TcpListener::bind(&self.listen_addr)
                .await
                .expect("bind address error"),
            router.into_make_service(),
        )
        .with_graceful_shutdown(shutdown))
        .await
        .map_err(anyhow::Error::from))
    }
}

struct SomeTask {
    ...
}

impl SomeTask {
    fn new(state: TaskState) -> Self {
        Self { state }
    }

    async fn big_recurring_task(&self, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                biased;
                _ = shutdown => break,
                signal = self.state.listener.recv() => {
                    ...do something important...
                }
            }
        }

        Ok(())
    }
}

impl ManagedProc for SomeTask {
    fn run_proc(
        self: Box<Self>,
        shutdown: ShutdownSignal,
    ) -> futures::future::LocalBoxFuture<'static, anyhow::Result<()>> {
        Box::pin(self.big_recurring_task(shutdown))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listen_addr = get_listen_addr_from_config();
    let axum_server = AxumServer { listen_addr, ... };

    let task_state = task_state_from_config_too();
    let task_proc = SomeTask::new(task_state);

    Supervisor::builder()
        .add_proc(axum_server)
        .add_proc(task_proc)
        .build()
        .start()
        .await
}
```
