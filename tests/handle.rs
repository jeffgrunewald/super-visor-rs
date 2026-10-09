mod common;

use common::*;
use std::time::Duration;
use super_visor::{
    Backoff, ChildCounts, ChildSpec, ChildStatus, HandleError, Intensity, Restart, Strategy,
    Supervisor, SupervisorError, SupervisorHandle,
};

fn statuses(children: &[super_visor::ChildInfo]) -> Vec<(String, ChildStatus)> {
    children
        .iter()
        .map(|child| (child.id.to_string(), child.status))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn start_child_starts_after_existing_children_and_stops_first() {
    let log = Log::default();
    let supervisor = Supervisor::builder()
        .child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    handle
        .start_child(ChildSpec::new("b", log.child("b", always(Exit::Never))))
        .await
        .unwrap();
    let children = handle.which_children().await.unwrap();
    tokio::time::sleep(10 * MS).await;
    assert!(stop(token, task).await.is_ok());

    assert_eq!(
        statuses(&children),
        [
            ("a".to_string(), ChildStatus::Running),
            ("b".to_string(), ChildStatus::Running)
        ]
    );
    let stops: Vec<_> = log
        .entries()
        .into_iter()
        .filter(|e| e.starts_with("stop"))
        .collect();
    assert_eq!(stops, ["stop:b", "stop:a"]);
}

#[tokio::test(start_paused = true)]
async fn calls_before_start_wait_and_calls_after_stop_fail() {
    let log = Log::default();
    let supervisor = Supervisor::new();
    let handle = supervisor.handle();
    let early = tokio::spawn({
        let handle = handle.clone();
        let spec = ChildSpec::new("a", log.child("a", always(Exit::Never)));
        async move { handle.start_child(spec).await }
    });
    tokio::task::yield_now().await;
    let (token, task) = run(supervisor);

    assert!(early.await.unwrap().is_ok());
    assert!(stop(token, task).await.is_ok());
    assert!(matches!(
        handle.count_children().await,
        Err(HandleError::NotRunning)
    ));
}

#[tokio::test(start_paused = true)]
async fn start_child_refuses_duplicates_and_invalid_specs() {
    let log = Log::default();
    let supervisor = Supervisor::builder()
        .child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    let duplicate = handle
        .start_child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .await;
    let invalid = handle
        .start_child(ChildSpec::new("p", log.child("p", always(Exit::Never))).significant(true))
        .await;
    assert!(stop(token, task).await.is_ok());

    assert!(matches!(duplicate, Err(HandleError::AlreadyPresent(ref id)) if id.as_str() == "a"));
    assert!(matches!(
        invalid,
        Err(HandleError::Start(SupervisorError::InvalidSpec { .. }))
    ));
}

#[tokio::test(start_paused = true)]
async fn failed_start_child_discards_the_spec() {
    let supervisor = Supervisor::new();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    let result = handle
        .start_child(ChildSpec::with_init("db", || async {
            Err::<TestProc, _>(TestError("refused"))
        }))
        .await;
    let counts = handle.count_children().await.unwrap();
    assert!(stop(token, task).await.is_ok());

    assert!(matches!(result, Err(HandleError::Start(_))));
    assert_eq!(counts, ChildCounts::default());
}

#[tokio::test(start_paused = true)]
async fn terminate_keeps_the_spec_and_restart_starts_it_again() {
    let log = Log::default();
    let supervisor = Supervisor::builder()
        .intensity(Intensity::new(0, Duration::from_secs(5)))
        .child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    handle.terminate_child("a").await.unwrap();
    let stopped = handle.count_children().await.unwrap();
    let running_again = handle.restart_child("a").await;
    let while_running = handle.restart_child("a").await;
    handle.terminate_child("a").await.unwrap();
    handle.restart_child("a").await.unwrap();
    assert!(stop(token, task).await.is_ok());

    assert_eq!(
        stopped,
        ChildCounts {
            specs: 1,
            active: 0
        }
    );
    assert!(running_again.is_ok());
    assert!(matches!(while_running, Err(HandleError::Running(_))));
    assert_eq!(
        log.count("start:a"),
        3,
        "manual restarts do not count toward intensity 0"
    );
}

#[tokio::test(start_paused = true)]
async fn terminating_a_temporary_child_removes_its_spec() {
    let log = Log::default();
    let supervisor = Supervisor::builder()
        .child(
            ChildSpec::new("tmp", log.child("tmp", always(Exit::Never)))
                .restart(Restart::Temporary),
        )
        .build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    handle.terminate_child("tmp").await.unwrap();
    let missing = handle.restart_child("tmp").await;
    assert!(stop(token, task).await.is_ok());

    assert!(matches!(missing, Err(HandleError::NotFound(_))));
}

#[tokio::test(start_paused = true)]
async fn delete_child_needs_a_stopped_child() {
    let log = Log::default();
    let supervisor = Supervisor::builder()
        .child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    let while_running = handle.delete_child("a").await;
    handle.terminate_child("a").await.unwrap();
    handle.delete_child("a").await.unwrap();
    let after_delete = handle.delete_child("a").await;
    assert!(stop(token, task).await.is_ok());

    assert!(matches!(while_running, Err(HandleError::Running(_))));
    assert!(matches!(after_delete, Err(HandleError::NotFound(_))));
}

#[tokio::test(start_paused = true)]
async fn restart_child_refuses_a_child_waiting_for_backoff() {
    let log = Log::default();
    let supervisor = Supervisor::builder()
        .backoff(Backoff::Fixed(Duration::from_secs(60)))
        .child(ChildSpec::new("a", log.child("a", always(Exit::Err(10)))))
        .build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    tokio::time::sleep(100 * MS).await;
    let children = handle.which_children().await.unwrap();
    let result = handle.restart_child("a").await;
    assert!(stop(token, task).await.is_ok());

    assert_eq!(children[0].status, ChildStatus::Restarting);
    assert!(matches!(result, Err(HandleError::Restarting(_))));
}

#[tokio::test(start_paused = true)]
async fn once_only_child_cannot_be_restarted_by_hand() {
    let log = Log::default();
    let supervisor = Supervisor::builder()
        .child(ChildSpec::once("once", log.once("once", Exit::Never)))
        .child(ChildSpec::new("b", log.child("b", always(Exit::Never))))
        .build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    handle.terminate_child("once").await.unwrap();
    let result = handle.restart_child("once").await;
    assert!(stop(token, task).await.is_ok());

    assert!(matches!(result, Err(HandleError::NotRestartable(_))));
}

#[tokio::test(start_paused = true)]
async fn terminating_a_significant_child_does_not_auto_shutdown() {
    let log = Log::default();
    let supervisor = Supervisor::builder()
        .add_proc(log.once("legacy", Exit::Never))
        .build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    let legacy_id = handle.which_children().await.unwrap()[0].id.clone();
    handle.terminate_child(legacy_id).await.unwrap();
    tokio::time::sleep(10 * MS).await;
    assert!(!task.is_finished());
    assert!(stop(token, task).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn children_added_by_handle_take_part_in_rest_for_one() {
    let log = Log::default();
    let supervisor = Supervisor::builder()
        .strategy(Strategy::RestForOne)
        .child(ChildSpec::new(
            "a",
            log.child("a", first_then_never(Exit::Err(50))),
        ))
        .build();
    let handle = supervisor.handle();
    let (token, task) = run(supervisor);

    handle
        .start_child(ChildSpec::new("b", log.child("b", always(Exit::Never))))
        .await
        .unwrap();
    tokio::time::sleep(100 * MS).await;
    assert!(stop(token, task).await.is_ok());

    assert_eq!(log.count("start:a"), 2);
    assert_eq!(log.count("start:b"), 2);
}

#[tokio::test(start_paused = true)]
async fn a_shared_handle_follows_a_restarted_nested_supervisor() {
    let log = Log::default();
    let inner_handle = SupervisorHandle::new();
    let inner = {
        let inner_handle = inner_handle.clone();
        let make = std::sync::Arc::new(log.child("w", first_then_never(Exit::Err(10))));
        move || {
            let make = make.clone();
            Supervisor::builder()
                .handle(inner_handle.clone())
                .intensity(Intensity::new(0, Duration::from_secs(1)))
                .child(ChildSpec::new("w", move || make()))
                .build()
        }
    };
    let (token, task) = run(Supervisor::builder()
        .child(ChildSpec::supervisor("inner", inner))
        .build());

    tokio::time::sleep(100 * MS).await;
    let started = inner_handle
        .start_child(ChildSpec::new(
            "extra",
            log.child("extra", always(Exit::Never)),
        ))
        .await;
    let children = inner_handle.which_children().await.unwrap();
    assert!(stop(token, task).await.is_ok());

    assert_eq!(
        log.count("start:w"),
        2,
        "the inner supervisor restarted once"
    );
    assert!(started.is_ok());
    assert_eq!(children.len(), 2);
}
