//! Gate tests inject a fresh BootstrapProgress and assert only on their own
//! per-tmpdir database state.

use super::*;
use crate::session::info::Info;
use crate::session::storage::jsonl::JsonlStorageAdapter;
use crate::session::storage::search_fts::META_KEY_SCHEMA_VERSION;
use agent_client_protocol as acp;

fn write_last_bootstrap_at(db_path: &Path) -> io::Result<()> {
    let now = chrono::Utc::now().timestamp();
    with_search_index(db_path, |index| {
        index.set_meta(META_KEY_LAST_BOOTSTRAP, &now.to_string())
    })
}

const TEST_TIMING: BootstrapTiming = BootstrapTiming {
    lease: Duration::from_secs(300),
    refresh: Duration::from_millis(50),
    peer_wait: Duration::from_millis(200),
    poll: Duration::from_millis(10),
};
const _: () = assert!(TEST_TIMING.refresh.as_millis() < TEST_TIMING.lease.as_millis());
const _: () = assert!(TEST_TIMING.poll.as_millis() < TEST_TIMING.peer_wait.as_millis());

fn stamp_marker(db_path: &Path, value: &str) {
    with_search_index(db_path, |index| {
        index.set_meta(META_KEY_LAST_BOOTSTRAP, value)
    })
    .unwrap();
}

fn read_marker(db_path: &Path) -> Option<String> {
    with_search_index(db_path, |index| index.get_meta(META_KEY_LAST_BOOTSTRAP)).unwrap()
}

#[tokio::test]
async fn test_claimant_reindexes_even_when_marker_exists() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = search_db_path(tmp.path());
    stamp_marker(&db_path, "123");

    let storage = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    bootstrap_with_lease_inner(
        tmp.path(),
        &storage,
        &Arc::new(BootstrapProgress::default()),
        &TEST_TIMING,
        BootstrapRole::Launch,
    )
    .await
    .unwrap();

    // The reindex rewrote the marker and released the claim.
    assert_ne!(read_marker(&db_path).as_deref(), Some("123"));
    let claim =
        with_search_index(&db_path, |index| index.get_meta(META_KEY_BOOTSTRAP_CLAIM)).unwrap();
    assert_eq!(claim, None);
}

#[tokio::test]
async fn test_has_completed_bootstrap_marker_lifecycle() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let db_path = search_db_path(root);

    assert_eq!(has_completed_bootstrap_marker(root).await, Some(false));

    write_last_bootstrap_at(&db_path).unwrap();
    assert_eq!(has_completed_bootstrap_marker(root).await, Some(true));

    // An older binary re-stamped a downgraded schema version.
    {
        let index = SessionSearchIndex::open_or_create(&db_path).unwrap();
        index.set_meta(META_KEY_SCHEMA_VERSION, "3").unwrap();
    }
    assert_eq!(
        has_completed_bootstrap_marker(root).await,
        Some(false),
        "a downgraded index must not count as bootstrapped even with a recent marker"
    );

    write_last_bootstrap_at(&db_path).unwrap();
    assert_eq!(has_completed_bootstrap_marker(root).await, Some(true));
}

#[tokio::test]
async fn test_waiter_adopts_peer_marker_without_reindexing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = search_db_path(tmp.path());

    let now = chrono::Utc::now().timestamp();
    with_search_index(&db_path, |index| {
        index.try_claim_bootstrap(now, TEST_TIMING.lease, "peer")
    })
    .unwrap();
    stamp_marker(&db_path, "123");

    let storage = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    bootstrap_with_lease_inner(
        tmp.path(),
        &storage,
        &Arc::new(BootstrapProgress::default()),
        &TEST_TIMING,
        BootstrapRole::Launch,
    )
    .await
    .unwrap();

    assert_eq!(read_marker(&db_path).as_deref(), Some("123"));
}

#[tokio::test]
async fn test_try_bootstrap_returns_at_once_when_peer_holds_claim() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = search_db_path(tmp.path());

    let now = chrono::Utc::now().timestamp();
    with_search_index(&db_path, |index| {
        index.try_claim_bootstrap(now, TIMING.lease, "peer")
    })
    .unwrap();

    let storage = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    let started = std::time::Instant::now();
    try_bootstrap_with_lease(
        tmp.path(),
        &storage,
        &Arc::new(BootstrapProgress::default()),
    )
    .await
    .unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a held claim must not block the recheck for the full peer wait"
    );
    assert_eq!(read_marker(&db_path), None, "no reindex ran");
}

#[tokio::test]
async fn test_recheck_adopts_marker_completed_after_its_probe() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = search_db_path(tmp.path());
    // A peer finished and released between the recheck's marker probe
    // and its claim attempt: the marker exists and the lease is free.
    stamp_marker(&db_path, "123");

    let storage = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    try_bootstrap_with_lease(
        tmp.path(),
        &storage,
        &Arc::new(BootstrapProgress::default()),
    )
    .await
    .unwrap();

    assert_eq!(
        read_marker(&db_path).as_deref(),
        Some("123"),
        "the recheck must adopt the fresh marker, not reindex over it"
    );
    assert!(!has_bootstrap_claim(&db_path).unwrap());
}

#[tokio::test]
async fn test_waiter_gives_up_after_peer_wait() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = search_db_path(tmp.path());

    let now = chrono::Utc::now().timestamp();
    with_search_index(&db_path, |index| {
        index.try_claim_bootstrap(now, TEST_TIMING.lease, "peer")
    })
    .unwrap();

    let storage = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    bootstrap_with_lease_inner(
        tmp.path(),
        &storage,
        &Arc::new(BootstrapProgress::default()),
        &TEST_TIMING,
        BootstrapRole::Launch,
    )
    .await
    .unwrap();

    assert_eq!(read_marker(&db_path), None, "no reindex ran");
}

#[test]
fn test_shared_index_reopens_after_epoch_change() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = search_db_path(tmp.path());
    let shared = SharedIndex::new();
    shared
        .with(&db_path, |index| index.set_meta("k", "v"))
        .unwrap();

    // A heal bumps the epoch and replaces the file.
    search_recovery::heal_unusable(
        &db_path,
        &rusqlite::Error::QueryReturnedNoRows,
        |_| Ok(false),
        |p| SessionSearchIndex::open_or_create(p).map(|_| ()),
    );

    let value = shared.with(&db_path, |index| index.get_meta("k")).unwrap();
    assert_eq!(
        value, None,
        "the connection must re-open at the new epoch, not keep the old fd"
    );
}

#[test]
fn test_read_write_last_bootstrap_at() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("session_search.sqlite");

    assert_eq!(try_read_last_bootstrap_at(&db_path).unwrap(), None);
    write_last_bootstrap_at(&db_path).unwrap();

    let ts = try_read_last_bootstrap_at(&db_path).unwrap().unwrap();
    let now = chrono::Utc::now().timestamp();
    assert!((now - ts).abs() < 5);
}

#[test]
fn test_clear_last_bootstrap_at() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("session_search.sqlite");

    write_last_bootstrap_at(&db_path).unwrap();
    assert!(try_read_last_bootstrap_at(&db_path).unwrap().is_some());

    clear_last_bootstrap_at(&db_path).unwrap();
    assert_eq!(try_read_last_bootstrap_at(&db_path).unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_concurrent_gates_single_flight() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let storage = JsonlStorageAdapter::with_root(root.clone());
    for id in ["s1", "s2"] {
        let info = Info {
            id: acp::SessionId::new(id),
            cwd: "/ws".to_string(),
        };
        storage
            .init_session(&info, acp::ModelId::new("test"))
            .await
            .unwrap();
    }
    with_search_index(&search_db_path(&root), |_| Ok(())).unwrap();

    let progress_a = Arc::new(BootstrapProgress::default());
    let progress_b = Arc::new(BootstrapProgress::default());
    let storage_a = storage.clone();
    let storage_b = storage.clone();
    let root_a = root.clone();
    let root_b = root;
    let pa = Arc::clone(&progress_a);
    let pb = Arc::clone(&progress_b);
    let start = Arc::new(tokio::sync::Barrier::new(2));
    let start_a = Arc::clone(&start);
    let start_b = Arc::clone(&start);
    let (a, b) = tokio::join!(
        tokio::spawn(async move {
            start_a.wait().await;
            bootstrap_with_lease_inner(
                &root_a,
                &storage_a,
                &pa,
                &TEST_TIMING,
                BootstrapRole::Launch,
            )
            .await
        }),
        tokio::spawn(async move {
            start_b.wait().await;
            bootstrap_with_lease_inner(
                &root_b,
                &storage_b,
                &pb,
                &TEST_TIMING,
                BootstrapRole::Launch,
            )
            .await
        }),
    );
    let a = a.expect("gate a task panicked");
    let b = b.expect("gate b task panicked");
    assert!(a.is_ok(), "gate a: {a:?}");
    assert!(b.is_ok(), "gate b: {b:?}");

    let db_path = search_db_path(tmp.path());
    assert!(
        read_marker(&db_path).is_some(),
        "completion marker must exist after concurrent gates"
    );
    assert!(
        !has_bootstrap_claim(&db_path).unwrap(),
        "claim must be released after concurrent gates"
    );

    let a_ran = progress_a.total.load(Ordering::Relaxed) > 0;
    let b_ran = progress_b.total.load(Ordering::Relaxed) > 0;
    assert_eq!(
        usize::from(a_ran) + usize::from(b_ran),
        1,
        "exactly one gate must reindex, a_total={}, b_total={}",
        progress_a.total.load(Ordering::Relaxed),
        progress_b.total.load(Ordering::Relaxed),
    );
}

/// Peer finished and released after this launch snapped an empty marker but
/// before its first claim ran (`spawn_blocking` delay). Without adopting the
/// new marker, the late first claim would reindex again as a "fresh" launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_launch_adopts_marker_written_before_first_claim() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let storage = JsonlStorageAdapter::with_root(root.clone());
    let info = Info {
        id: acp::SessionId::new("s1"),
        cwd: "/ws".to_string(),
    };
    storage
        .init_session(&info, acp::ModelId::new("test"))
        .await
        .unwrap();
    with_search_index(&search_db_path(&root), |_| Ok(())).unwrap();

    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    *super::TEST_PAUSE_AFTER_MARKER_SNAPSHOT
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some((root.clone(), release_rx));

    let progress = Arc::new(BootstrapProgress::default());
    let late_root = root.clone();
    let late_storage = storage.clone();
    let late_progress = Arc::clone(&progress);
    let late = tokio::spawn(async move {
        bootstrap_with_lease_inner(
            &late_root,
            &late_storage,
            &late_progress,
            &TEST_TIMING,
            BootstrapRole::Launch,
        )
        .await
    });

    // Wait until the late gate has snapped the empty marker and parked. The
    // gate reaches the pause after disk I/O, so budget for a busy machine
    // (a tight poll window flakes under full-suite parallel load) — but the
    // loop exits as soon as the pause is observed.
    let pause_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while super::TEST_PAUSE_AFTER_MARKER_SNAPSHOT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
    {
        assert!(
            std::time::Instant::now() < pause_deadline,
            "late gate never reached the post-snapshot pause"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    bootstrap_with_lease_inner(
        &root,
        &storage,
        &Arc::new(BootstrapProgress::default()),
        &TEST_TIMING,
        BootstrapRole::Launch,
    )
    .await
    .unwrap();
    assert!(
        has_completed_bootstrap_marker(&root).await == Some(true),
        "peer must leave a completion marker before the late claim"
    );

    release_tx.send(()).expect("late gate still waiting");
    let late = late.await.expect("late task panicked");
    assert!(late.is_ok(), "late gate: {late:?}");
    assert_eq!(
        progress.total.load(Ordering::Relaxed),
        0,
        "late first claim must adopt the peer marker, not reindex"
    );
    assert!(!has_bootstrap_claim(&search_db_path(&root)).unwrap());
}
