mod select_all;

use crate::select_all::select_all;
use anyhow::Result;
use futures::{future::LocalBoxFuture, Future, FutureExt, StreamExt};
use std::{
    pin::{pin, Pin},
    task::{Context, Poll},
};
use tokio::signal;
use tokio_util::sync::CancellationToken;

fn root_shutdown() -> Result<LocalBoxFuture<'static, ()>> {
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    Ok(Box::pin(
        futures::future::select(
            Box::pin(async move { sigterm.recv().await }),
            Box::pin(signal::ctrl_c()),
        )
        .map(|_| ()),
    ))
}

pub trait ManagedProc {
    fn start_proc(
        self: Box<Self>,
        shutdown: CancellationToken,
    ) -> LocalBoxFuture<'static, Result<()>>;
}

pub struct Supervisor {
    procs: Vec<Box<dyn ManagedProc>>,
}

impl ManagedProc for Supervisor {
    fn start_proc(
        self: Box<Self>,
        shutdown: CancellationToken,
    ) -> LocalBoxFuture<'static, Result<()>> {
        let cancel_listener = shutdown.cancelled_owned();
        Box::pin(self.do_start(Box::pin(cancel_listener)))
    }
}

pub struct SupervisorBuilder {
    procs: Vec<Box<dyn ManagedProc>>,
}

struct CancelableLocalFuture {
    cancel_token: CancellationToken,
    future: LocalBoxFuture<'static, Result<()>>,
}

impl Future for CancelableLocalFuture {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        pin!(&mut self.future).poll(ctx)
    }
}

impl<F, O> ManagedProc for F
where
    O: Future<Output = Result<()>> + 'static,
    F: FnOnce(CancellationToken) -> O,
{
    fn start_proc(
        self: Box<Self>,
        shutdown: CancellationToken,
    ) -> LocalBoxFuture<'static, Result<()>> {
        Box::pin(self(shutdown))
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub fn new() -> Self {
        Self { procs: Vec::new() }
    }

    pub fn builder() -> SupervisorBuilder {
        SupervisorBuilder { procs: Vec::new() }
    }

    pub fn add(&mut self, proc: impl ManagedProc + 'static) {
        self.procs.push(Box::new(proc));
    }

    pub async fn start(self) -> Result<()> {
        self.do_start(root_shutdown()?).await
    }

    async fn do_start(self, mut shutdown: LocalBoxFuture<'static, ()>) -> Result<()> {
        let mut futures = start_futures(self.procs);

        loop {
            if futures.is_empty() {
                break;
            }

            let mut select = select_all(futures);

            tokio::select! {
                biased;
                _ = &mut shutdown => return stop_all(select.into_inner()).await,
                (result, _index, remaining) = &mut select => match result {
                    Ok(_) => futures = remaining,
                    Err(err) => {
                        let _ = stop_all(remaining).await;
                        return Err(err);
                    }
                }
            }
        }

        Ok(())
    }
}

impl SupervisorBuilder {
    pub fn add_proc(mut self, proc: impl ManagedProc + 'static) -> Self {
        self.procs.push(Box::new(proc));
        self
    }

    pub fn build(self) -> Supervisor {
        Supervisor { procs: self.procs }
    }
}

fn start_futures(procs: Vec<Box<dyn ManagedProc>>) -> Vec<CancelableLocalFuture> {
    procs
        .into_iter()
        .map(|proc| {
            let cancel_token = CancellationToken::new();
            let child_token = cancel_token.child_token();
            CancelableLocalFuture {
                cancel_token,
                future: proc.start_proc(child_token),
            }
        })
        .collect()
}

async fn stop_all(procs: Vec<CancelableLocalFuture>) -> Result<()> {
    futures::stream::iter(procs.into_iter().rev())
        .then(|proc| async move {
            proc.cancel_token.cancel();
            proc.future.await
        })
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use futures::TryFutureExt;
    use tokio::sync::mpsc;

    struct TestProc {
        name: &'static str,
        delay: u64,
        result: Result<()>,
        sender: mpsc::Sender<&'static str>,
    }

    impl ManagedProc for TestProc {
        fn start_proc(
            self: Box<Self>,
            shutdown: CancellationToken,
        ) -> LocalBoxFuture<'static, Result<()>> {
            let handle = tokio::spawn(async move {
                tokio::select! {
                    _ = shutdown.cancelled() => (),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(self.delay)) => (),
                }
                self.sender.send(self.name).await.expect("unable to send");
                self.result
            });

            Box::pin(
                handle
                    .map_err(|err| err.into())
                    .and_then(|result| async move { result }),
            )
        }
    }

    #[tokio::test]
    async fn stop_when_all_tasks_have_completed() {
        let (sender, mut receiver) = mpsc::channel(5);

        let result = Supervisor::builder()
            .add_proc(TestProc {
                name: "1",
                delay: 50,
                result: Ok(()),
                sender: sender.clone(),
            })
            .add_proc(TestProc {
                name: "2",
                delay: 100,
                result: Ok(()),
                sender: sender.clone(),
            })
            .build()
            .start()
            .await;

        assert_eq!(Some("1"), receiver.recv().await);
        assert_eq!(Some("2"), receiver.recv().await);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn will_stop_all_in_reverse_order_after_error() {
        let (sender, mut receiver) = mpsc::channel(5);

        let result = Supervisor::builder()
            .add_proc(TestProc {
                name: "1",
                delay: 1000,
                result: Ok(()),
                sender: sender.clone(),
            })
            .add_proc(TestProc {
                name: "2",
                delay: 50,
                result: Err(anyhow!("error")),
                sender: sender.clone(),
            })
            .add_proc(TestProc {
                name: "3",
                delay: 1000,
                result: Ok(()),
                sender: sender.clone(),
            })
            .build()
            .start()
            .await;

        assert_eq!(Some("2"), receiver.recv().await);
        assert_eq!(Some("3"), receiver.recv().await);
        assert_eq!(Some("1"), receiver.recv().await);
        assert_eq!("error", result.unwrap_err().to_string());
    }

    #[tokio::test]
    async fn will_return_first_error_returned() {
        let (sender, mut receiver) = mpsc::channel(5);

        let result = Supervisor::builder()
            .add_proc(TestProc {
                name: "1",
                delay: 1000,
                result: Ok(()),
                sender: sender.clone(),
            })
            .add_proc(TestProc {
                name: "2",
                delay: 50,
                result: Err(anyhow!("error")),
                sender: sender.clone(),
            })
            .add_proc(TestProc {
                name: "3",
                delay: 200,
                result: Err(anyhow!("second error")),
                sender: sender.clone(),
            })
            .build()
            .start()
            .await;

        assert_eq!(Some("2"), receiver.recv().await);
        assert_eq!(Some("3"), receiver.recv().await);
        assert_eq!(Some("1"), receiver.recv().await);
        assert_eq!("error", result.unwrap_err().to_string());
    }

    #[tokio::test]
    async fn nested_procs_will_stop_parent_then_move_up() {
        let (sender, mut receiver) = mpsc::channel(10);

        let result = Supervisor::builder()
            .add_proc(TestProc {
                name: "proc-1",
                delay: 500,
                result: Ok(()),
                sender: sender.clone(),
            })
            .add_proc(
                Supervisor::builder()
                    .add_proc(TestProc {
                        name: "proc-2-1",
                        delay: 500,
                        result: Ok(()),
                        sender: sender.clone(),
                    })
                    .add_proc(TestProc {
                        name: "proc-2-2",
                        delay: 100,
                        result: Err(anyhow!("error")),
                        sender: sender.clone(),
                    })
                    .add_proc(TestProc {
                        name: "proc-2-3",
                        delay: 500,
                        result: Ok(()),
                        sender: sender.clone(),
                    })
                    .add_proc(TestProc {
                        name: "proc-2-4",
                        delay: 500,
                        result: Ok(()),
                        sender: sender.clone(),
                    })
                    .build(),
            )
            .add_proc(
                Supervisor::builder()
                    .add_proc(TestProc {
                        name: "proc-3-1",
                        delay: 1000,
                        result: Ok(()),
                        sender: sender.clone(),
                    })
                    .add_proc(TestProc {
                        name: "proc-3-2",
                        delay: 1000,
                        result: Ok(()),
                        sender: sender.clone(),
                    })
                    .add_proc(TestProc {
                        name: "proc-3-3",
                        delay: 1000,
                        result: Ok(()),
                        sender: sender.clone(),
                    })
                    .build(),
            )
            .build()
            .start()
            .await;

        assert_eq!(Some("proc-2-2"), receiver.recv().await);
        assert_eq!(Some("proc-2-4"), receiver.recv().await);
        assert_eq!(Some("proc-2-3"), receiver.recv().await);
        assert_eq!(Some("proc-2-1"), receiver.recv().await);
        assert_eq!(Some("proc-3-3"), receiver.recv().await);
        assert_eq!(Some("proc-3-2"), receiver.recv().await);
        assert_eq!(Some("proc-3-1"), receiver.recv().await);
        assert_eq!(Some("proc-1"), receiver.recv().await);
        assert!(result.is_err());
    }
}
