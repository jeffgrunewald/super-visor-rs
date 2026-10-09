mod common;

use common::*;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use super_visor::{
    ChildCounts, ChildSpec, ChildStatus, DynamicSupervisor, EventKind, HandleError, Intensity,
    Restart, Shutdown, Supervisor, SupervisorError, SupervisorHandle,
};
use tokio::time::Instant;

#[test]
fn dynamic_supervisor_is_send_and_sync() {
    assert_send_sync::<DynamicSupervisor>();
    assert_send_sync::<SupervisorHandle>();
}

#[tokio::test(start_paused = true)]
async fn children_are_added_at_runtime() {
    let log = Log::default();
    let supervisor = DynamicSupervisor::new();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    let empty = handle.count_children().await.unwrap();
    for name in ["a", "b", "c"] {
        handle
            .start_child(ChildSpec::new(name, log.child(name, always(Exit::Never))))
            .await
            .unwrap();
    }
    let counts = handle.count_children().await.unwrap();
    assert!(stop(token, task).await.is_ok());

    assert_eq!(empty, ChildCounts::default());
    assert_eq!(
        counts,
        ChildCounts {
            specs: 3,
            active: 3
        }
    );
    assert_eq!(
        log.count("stop:a") + log.count("stop:b") + log.count("stop:c"),
        3
    );
}

#[tokio::test(start_paused = true)]
async fn start_child_refusals() {
    let log = Log::default();
    let supervisor = DynamicSupervisor::builder().max_children(1).build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    handle
        .start_child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .await
        .unwrap();
    let duplicate = handle
        .start_child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .await;
    let over_max = handle
        .start_child(ChildSpec::new("b", log.child("b", always(Exit::Never))))
        .await;
    handle.terminate_child("a").await.unwrap();
    let significant = handle
        .start_child(
            ChildSpec::new("s", log.child("s", always(Exit::Never)))
                .restart(Restart::Transient)
                .significant(true),
        )
        .await;
    let failed_init = handle
        .start_child(ChildSpec::with_init("db", || async {
            Err::<TestProc, _>(TestError("refused"))
        }))
        .await;
    let counts = handle.count_children().await.unwrap();
    assert!(stop(token, task).await.is_ok());

    assert!(matches!(duplicate, Err(HandleError::AlreadyPresent(_))));
    assert!(matches!(over_max, Err(HandleError::MaxChildren(1))));
    assert!(matches!(
        significant,
        Err(HandleError::Start(SupervisorError::InvalidSpec { .. }))
    ));
    assert!(matches!(failed_init, Err(HandleError::Start(_))));
    assert_eq!(counts, ChildCounts::default());
}

#[tokio::test(start_paused = true)]
async fn restart_and_delete_are_not_supported() {
    let supervisor = DynamicSupervisor::new();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    let restart = handle.restart_child("a").await;
    let delete = handle.delete_child("a").await;
    assert!(stop(token, task).await.is_ok());

    assert!(matches!(restart, Err(HandleError::NotSupported)));
    assert!(matches!(delete, Err(HandleError::NotSupported)));
}

#[tokio::test(start_paused = true)]
async fn a_failing_child_restarts_alone() {
    let log = Log::default();
    let supervisor = DynamicSupervisor::builder()
        .intensity(Intensity::new(5, Duration::from_secs(1)))
        .build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    handle
        .start_child(ChildSpec::new("ok", log.child("ok", always(Exit::Never))))
        .await
        .unwrap();
    handle
        .start_child(ChildSpec::new(
            "bad",
            log.child("bad", first_then_never(Exit::Err(10))),
        ))
        .await
        .unwrap();
    handle
        .start_child(ChildSpec::new(
            "boom",
            log.child("boom", first_then_never(Exit::Panic(10))),
        ))
        .await
        .unwrap();
    tokio::time::sleep(100 * MS).await;
    assert!(stop(token, task).await.is_ok());

    assert_eq!(log.count("start:ok"), 1);
    assert_eq!(log.count("start:bad"), 2);
    assert_eq!(log.count("start:boom"), 2);
}

#[tokio::test(start_paused = true)]
async fn children_that_end_for_good_are_removed() {
    let log = Log::default();
    let supervisor = DynamicSupervisor::new();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    handle
        .start_child(
            ChildSpec::new("job", log.child("job", always(Exit::Ok(10))))
                .restart(Restart::Transient),
        )
        .await
        .unwrap();
    handle
        .start_child(
            ChildSpec::new("tmp", log.child("tmp", always(Exit::Err(10))))
                .restart(Restart::Temporary),
        )
        .await
        .unwrap();
    tokio::time::sleep(100 * MS).await;
    let counts = handle.count_children().await.unwrap();
    assert!(stop(token, task).await.is_ok());

    assert_eq!(counts, ChildCounts::default());
}

#[tokio::test(start_paused = true)]
async fn exceeding_intensity_stops_every_child_and_escalates() {
    let log = Log::default();
    let supervisor = DynamicSupervisor::builder()
        .intensity(Intensity::new(1, Duration::from_secs(1)))
        .build();
    let handle = supervisor.handle();
    let (_token, task) = run(supervisor);

    handle
        .start_child(ChildSpec::new("ok", log.child("ok", always(Exit::Never))))
        .await
        .unwrap();
    handle
        .start_child(ChildSpec::new(
            "bad",
            log.child("bad", always(Exit::Err(10))),
        ))
        .await
        .unwrap();
    let result = task.await.unwrap();

    assert_eq!(log.count("stop:ok"), 1);
    assert!(
        matches!(result, Err(SupervisorError::MaxIntensity { ref id, .. }) if id.as_str() == "bad")
    );
    assert!(matches!(
        handle.count_children().await,
        Err(HandleError::NotRunning)
    ));
}

#[tokio::test(start_paused = true)]
async fn terminate_child_kills_after_timeout_without_blocking_other_calls() {
    let log = Log::default();
    let killed = Arc::new(Mutex::new(Vec::new()));
    let supervisor = DynamicSupervisor::builder()
        .on_event({
            let killed = killed.clone();
            move |event| {
                if let EventKind::Killed { timeout } = event.kind {
                    killed
                        .lock()
                        .unwrap()
                        .push((event.child.to_string(), timeout));
                }
            }
        })
        .build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    handle
        .start_child(
            ChildSpec::new("hang", log.child("hang", always(Exit::Hang)))
                .shutdown(Shutdown::Timeout(Duration::from_secs(2))),
        )
        .await
        .unwrap();
    let origin = Instant::now();
    let terminate = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle.terminate_child("hang").await.unwrap();
            origin.elapsed()
        }
    });
    tokio::task::yield_now().await;
    handle
        .start_child(ChildSpec::new("b", log.child("b", always(Exit::Never))))
        .await
        .unwrap();
    let started_b_at = origin.elapsed();
    let terminated_at = terminate.await.unwrap();
    let children = handle.which_children().await.unwrap();
    assert!(stop(token, task).await.is_ok());

    assert_eq!(started_b_at, Duration::ZERO);
    assert_eq!(terminated_at, Duration::from_secs(2));
    assert_eq!(log.count("dropped:hang"), 1);
    assert_eq!(
        *killed.lock().unwrap(),
        [("hang".to_string(), Duration::from_secs(2))]
    );
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].status, ChildStatus::Running);
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_children_concurrently() {
    let log = Log::default();
    let supervisor = DynamicSupervisor::new();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    for name in ["h1", "h2", "h3"] {
        handle
            .start_child(
                ChildSpec::new(name, log.child(name, always(Exit::Hang)))
                    .shutdown(Shutdown::Timeout(Duration::from_secs(1))),
            )
            .await
            .unwrap();
    }
    let origin = Instant::now();
    let result = stop(token, task).await;

    assert_eq!(origin.elapsed(), Duration::from_secs(1));
    assert!(matches!(
        result,
        Err(SupervisorError::Child { ref source, .. }) if matches!(**source, SupervisorError::ShutdownTimeout(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn a_thousand_children_start_and_stop() {
    let log = Log::default();
    let supervisor = DynamicSupervisor::new();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    for n in 0..1000 {
        handle
            .start_child(ChildSpec::new(
                format!("w{n}"),
                log.child("w", always(Exit::Never)),
            ))
            .await
            .unwrap();
    }
    let counts = handle.count_children().await.unwrap();
    assert!(stop(token, task).await.is_ok());

    assert_eq!(counts.active, 1000);
    assert_eq!(log.count("stop:w"), 1000);
}

#[tokio::test(start_paused = true)]
async fn a_dynamic_supervisor_nests_under_a_static_one() {
    let log = Log::default();
    let pool = SupervisorHandle::new();
    let (token, task) = run(Supervisor::builder()
        .child(ChildSpec::new(
            "first",
            log.child("first", always(Exit::Never)),
        ))
        .child(ChildSpec::supervisor("pool", {
            let pool = pool.clone();
            move || DynamicSupervisor::builder().handle(pool.clone()).build()
        }))
        .build());

    pool.start_child(ChildSpec::new("w", log.child("w", always(Exit::Never))))
        .await
        .unwrap();
    assert!(stop(token, task).await.is_ok());

    let stops: Vec<_> = log
        .entries()
        .into_iter()
        .filter(|e| e.starts_with("stop"))
        .collect();
    assert_eq!(stops, ["stop:w", "stop:first"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn works_across_worker_threads_in_real_time() {
    let log = Log::default();
    let supervisor = DynamicSupervisor::builder()
        .intensity(Intensity::new(200, Duration::from_secs(10)))
        .build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    for n in 0..100 {
        let exit = if n % 10 == 0 {
            first_then_never(Exit::Err(5))
        } else {
            first_then_never(Exit::Never)
        };
        handle
            .start_child(ChildSpec::new(format!("w{n}"), log.child("w", exit)))
            .await
            .unwrap();
    }
    for n in (1..100).step_by(10) {
        handle.terminate_child(format!("w{n}")).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let counts = handle.count_children().await.unwrap();
    assert!(stop(token, task).await.is_ok());

    assert_eq!(
        counts,
        ChildCounts {
            specs: 90,
            active: 90
        }
    );
    assert_eq!(
        log.count("start:w"),
        110,
        "10 failing children restarted once"
    );
}
