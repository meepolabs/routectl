//! A refresh whose token POST is in flight while other store operations
//! run: reloads, logins, logouts and metadata updates of the same or a
//! sibling seat.

use std::time::Duration;

use crate::oauth::store::test_support::*;

const SEAT_A: &str = "anthropic#seat-a";
const SEAT_B: &str = "anthropic#seat-b";
const RT_A: &str = "rt-a";
const RT_B: &str = "rt-b";

fn seat_ref(label: &str) -> SecretRef {
    SecretRef::OAuth {
        provider: "anthropic".into(),
        label: Some(label.into()),
    }
}

fn near_expiry(access: &str, refresh: &str) -> TokenRecord {
    seat_rec(access, refresh, unix_now() + 10)
}

/// Seed `seats` on disk and open a store whose token endpoint parks on
/// `gate` while it serves the refresh token `park_on`.
async fn open_parked_store(
    path: &std::path::Path,
    seats: &[(&str, TokenRecord)],
    park_on: &str,
    gate: Arc<ParkGate>,
    reload_gate: Option<Arc<ParkGate>>,
) -> OAuthStore {
    let seed = OAuthStore::open(path).await.unwrap();
    for (seat, rec) in seats {
        seed.write_record(seat, rec.clone()).await.unwrap();
    }
    drop(seed);
    let flow = Arc::new(ParkedFlow::new(park_on, gate));
    open_with_flow_and_reload_gate(path, flow, reload_gate).await
}

async fn disk_records(path: &std::path::Path) -> BTreeMap<String, TokenRecord> {
    OAuthStore::open(path)
        .await
        .unwrap()
        .list()
        .await
        .into_iter()
        .collect()
}

async fn cached_records(store: &OAuthStore) -> BTreeMap<String, TokenRecord> {
    store.list().await.into_iter().collect()
}

/// Run `during` while seat-a's refresh POST is parked, then let the POST
/// finish and return what the refresh handed its caller.
async fn refresh_seat_a_around<F, Fut>(
    store: &OAuthStore,
    gate: &ParkGate,
    during: F,
) -> routectl_core::Result<String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let refreshing = {
        let store = store.clone();
        tokio::spawn(async move { store.get(&seat_ref("seat-a")).await })
    };
    tokio::time::timeout(Duration::from_secs(5), gate.wait_reached())
        .await
        .expect("premise: seat-a's refresh POST must start");
    during().await;
    gate.release();
    tokio::time::timeout(Duration::from_secs(5), refreshing)
        .await
        .expect("the refresh must finish once its POST is released")
        .expect("the refresh task must not panic")
}

/// What runs while seat-a's refresh POST is in flight, none of which
/// touches seat-a's credential.
#[derive(Clone, Copy, Debug)]
enum UnrelatedChange {
    /// A reload of a file nobody changed.
    NothingChanged,
    /// Another process re-logs seat-b, then the watcher reloads.
    SiblingRelogin,
    /// This store refreshes seat-b, and the watcher reloads on that write.
    SiblingRefresh,
}

impl UnrelatedChange {
    async fn apply(self, store: &OAuthStore, path: &std::path::Path) {
        match self {
            Self::NothingChanged => {}
            Self::SiblingRelogin => {
                let external = OAuthStore::open(path).await.unwrap();
                external
                    .write_record(
                        SEAT_B,
                        seat_rec("b-relogin", "rt-b-relogin", unix_now() + 7200),
                    )
                    .await
                    .unwrap();
            }
            Self::SiblingRefresh => {
                let got = store.get(&seat_ref("seat-b")).await.unwrap();
                assert!(
                    got == ParkedFlow::rotated_access(RT_B),
                    "premise: seat-b's own refresh committed"
                );
            }
        }
        store.reload_from_disk().await.unwrap();
    }
}

#[tokio::test]
async fn a_reload_that_leaves_the_seat_alone_keeps_the_rotated_token() {
    for change in [
        UnrelatedChange::NothingChanged,
        UnrelatedChange::SiblingRelogin,
        UnrelatedChange::SiblingRefresh,
    ] {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let gate = Arc::new(ParkGate::default());
        let seats = [
            (SEAT_A, near_expiry("a-stale", RT_A)),
            (SEAT_B, near_expiry("b-stale", RT_B)),
        ];
        let store = open_parked_store(&path, &seats, RT_A, gate.clone(), None).await;

        // Act
        let got = refresh_seat_a_around(&store, &gate, || change.apply(&store, &path)).await;

        // Assert
        let rotated_refresh = ParkedFlow::rotated_refresh(RT_A);
        assert!(
            got.as_deref().ok() == Some(ParkedFlow::rotated_access(RT_A).as_str()),
            "{change:?}: the caller must get the rotated access token"
        );
        let on_disk = disk_records(&path).await;
        let cached = cached_records(&store).await;
        assert!(
            on_disk[SEAT_A].refresh_token.expose() == rotated_refresh,
            "{change:?}: the rotated refresh token must be on disk"
        );
        assert!(
            cached[SEAT_A] == on_disk[SEAT_A],
            "{change:?}: the cache must hold seat-a's disk record"
        );
    }
}

/// What runs while seat-a's refresh POST is in flight and replaces or
/// removes seat-a's own credential.
#[derive(Clone, Copy, Debug)]
enum SameSeatChange {
    Relogin,
    Logout,
}

#[tokio::test]
async fn a_reload_carrying_a_same_seat_login_or_logout_wins_over_the_refresh() {
    for change in [SameSeatChange::Relogin, SameSeatChange::Logout] {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let gate = Arc::new(ParkGate::default());
        let seats = [(SEAT_A, near_expiry("a-stale", RT_A))];
        let store = open_parked_store(&path, &seats, RT_A, gate.clone(), None).await;
        let relogin = seat_rec("a-relogin", "rt-a-relogin", unix_now() + 7200);

        // Act
        let got = refresh_seat_a_around(&store, &gate, || {
            let (store, path, relogin) = (&store, &path, relogin.clone());
            async move {
                let external = OAuthStore::open(path).await.unwrap();
                match change {
                    SameSeatChange::Relogin => {
                        external.write_record(SEAT_A, relogin).await.unwrap()
                    }
                    SameSeatChange::Logout => assert!(external.logout(SEAT_A).await.unwrap()),
                }
                store.reload_from_disk().await.unwrap();
            }
        })
        .await;

        // Assert
        let on_disk = disk_records(&path).await;
        let cached = cached_records(&store).await;
        match change {
            SameSeatChange::Relogin => {
                assert!(
                    got.as_deref().ok() == Some("a-relogin"),
                    "the caller must get the re-login"
                );
                assert!(on_disk[SEAT_A] == relogin, "the re-login must stay on disk");
                assert!(cached[SEAT_A] == relogin, "the re-login must stay cached");
            }
            SameSeatChange::Logout => {
                assert!(got.is_err(), "a logged-out seat must not be refreshed");
                assert!(
                    !on_disk.contains_key(SEAT_A),
                    "the logout must stay on disk"
                );
                assert!(!cached.contains_key(SEAT_A), "the logout must stay cached");
            }
        }
    }
}

/// A reload whose disk read is in progress when a refresh commits must not
/// leave the cache holding the record that read saw.
#[tokio::test(start_paused = true)]
async fn a_reload_read_in_progress_does_not_undo_a_committing_refresh() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let unused_gate = Arc::new(ParkGate::default());
    let reload_gate = Arc::new(ParkGate::default());
    let seats = [(SEAT_A, near_expiry("a-stale", RT_A))];
    let store = open_parked_store(
        &path,
        &seats,
        "no-parked-refresh",
        unused_gate,
        Some(reload_gate.clone()),
    )
    .await;

    // Act: the reload reads the pre-refresh file, then the refresh runs as
    // far as it can before the reload resumes.
    let reloading = {
        let store = store.clone();
        tokio::spawn(async move { store.reload_from_disk().await })
    };
    reload_gate.wait_reached().await;
    let mut refreshing = {
        let store = store.clone();
        tokio::spawn(async move { store.get(&seat_ref("seat-a")).await })
    };
    // With the clock paused this returns as soon as nothing but the parked
    // reload can make progress: either the refresh finished or it waits on
    // the reload.
    let refreshed_first = tokio::time::timeout(Duration::from_mins(1), &mut refreshing).await;
    reload_gate.release();
    reloading.await.unwrap().unwrap();
    let got = match refreshed_first {
        Ok(joined) => joined.unwrap(),
        Err(_) => refreshing.await.unwrap(),
    };

    // Assert
    assert!(
        got.as_deref().ok() == Some(ParkedFlow::rotated_access(RT_A).as_str()),
        "the refresh must succeed"
    );
    let on_disk = disk_records(&path).await;
    let cached = cached_records(&store).await;
    assert!(
        on_disk[SEAT_A].refresh_token.expose() == ParkedFlow::rotated_refresh(RT_A),
        "premise: the rotation is on disk"
    );
    assert!(
        cached[SEAT_A] == on_disk[SEAT_A],
        "the cache must equal seat-a's disk record after both finish"
    );
}

#[tokio::test]
async fn a_refresh_keeps_every_locally_kept_field() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let seeded = TokenRecord {
        session_id: Some("sess-a".into()),
        cloud_project_id: Some("projects/p-a".into()),
        ..near_expiry("a-stale", RT_A)
    };
    let gate = Arc::new(ParkGate::default());
    let store =
        open_parked_store(&path, &[(SEAT_A, seeded)], "no-parked-refresh", gate, None).await;

    // Act
    let got = store.get(&seat_ref("seat-a")).await;

    // Assert
    assert!(got.is_ok(), "premise: the refresh succeeded");
    let on_disk = disk_records(&path).await;
    let cached = cached_records(&store).await;
    for (place, rec) in [("disk", &on_disk[SEAT_A]), ("cache", &cached[SEAT_A])] {
        assert!(
            rec.refresh_token.expose() == ParkedFlow::rotated_refresh(RT_A),
            "{place}: premise: the rotation was stored"
        );
        assert_eq!(rec.session_id.as_deref(), Some("sess-a"), "{place}");
        assert_eq!(
            rec.cloud_project_id.as_deref(),
            Some("projects/p-a"),
            "{place}"
        );
    }
}

/// A metadata update of seat-a while its refresh POST is in flight.
#[derive(Clone, Copy, Debug)]
enum MetadataUpdate {
    /// This store resolves a new Cloud Code project id.
    SetByThisStore,
    /// Another process clears the stored project id.
    ClearedByAnotherProcess,
}

#[tokio::test]
async fn a_metadata_update_during_a_refresh_keeps_both_the_rotation_and_the_update() {
    for (update, expected_project) in [
        (MetadataUpdate::SetByThisStore, Some("projects/p-new")),
        (MetadataUpdate::ClearedByAnotherProcess, None),
    ] {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let seeded = TokenRecord {
            session_id: Some("sess-a".into()),
            cloud_project_id: Some("projects/p-a".into()),
            ..near_expiry("a-stale", RT_A)
        };
        let gate = Arc::new(ParkGate::default());
        let store = open_parked_store(&path, &[(SEAT_A, seeded)], RT_A, gate.clone(), None).await;

        // Act
        let got = refresh_seat_a_around(&store, &gate, || {
            let (store, path) = (&store, &path);
            async move {
                match update {
                    MetadataUpdate::SetByThisStore => store
                        .set_cloud_project_id(SEAT_A, "projects/p-new")
                        .await
                        .unwrap(),
                    MetadataUpdate::ClearedByAnotherProcess => {
                        let external = OAuthStore::open(path).await.unwrap();
                        assert!(
                            external
                                .clear_cloud_project_id_if_matches(SEAT_A, "projects/p-a")
                                .await
                                .unwrap()
                        );
                    }
                }
            }
        })
        .await;

        // Assert
        assert!(
            got.as_deref().ok() == Some(ParkedFlow::rotated_access(RT_A).as_str()),
            "{update:?}: the caller must get the rotated access token"
        );
        let on_disk = disk_records(&path).await;
        let cached = cached_records(&store).await;
        for (place, rec) in [("disk", &on_disk[SEAT_A]), ("cache", &cached[SEAT_A])] {
            assert!(
                rec.refresh_token.expose() == ParkedFlow::rotated_refresh(RT_A),
                "{update:?} {place}: the rotation must be stored"
            );
            assert_eq!(
                rec.cloud_project_id.as_deref(),
                expected_project,
                "{update:?} {place}"
            );
            assert_eq!(
                rec.session_id.as_deref(),
                Some("sess-a"),
                "{update:?} {place}"
            );
        }
    }
}
