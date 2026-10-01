//! Lease behaviour (A2/F-71/F-73) against a real database. Requires
//! `CARGOBIKE_TEST_DATABASE_URL` (skips otherwise).

#[allow(clippy::print_stderr, clippy::expect_used)]
#[tokio::test(flavor = "current_thread")]
async fn test_acquire_release_transfer_behaviour() {
    use cargobike_engine::{LeaseAttempt, LeaseRepository, LeaseTransfer};
    use uuid::Uuid;

    let url = match std::env::var("CARGOBIKE_TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty())
    {
        Some(url) => url,
        None => {
            eprintln!("skipping: no fixture database");
            return;
        }
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("fixture pool");
    // The server crate owns the migrations; the engine test borrows them.
    sqlx::migrate!("../cargobike-server/migrations")
        .run(&pool)
        .await
        .expect("migrations");
    let _ = sqlx::query("DELETE FROM leases WHERE application = 'lease-test'")
        .execute(&pool)
        .await;
    let repository = LeaseRepository::new(pool.clone());

    let first = Uuid::now_v7();
    assert_eq!(
        repository
            .acquire("lease-test", "preview", first)
            .await
            .expect("acquire"),
        LeaseAttempt::Held
    );
    assert_eq!(
        repository
            .acquire("lease-test", "preview", first)
            .await
            .expect("again"),
        LeaseAttempt::HeldAlready
    );
    let second = Uuid::now_v7();
    assert_eq!(
        repository
            .acquire("lease-test", "preview", second)
            .await
            .expect("loss visible"),
        LeaseAttempt::HeldBy {
            other_release_id: first
        }
    );

    // F-73's atomic supersede transfer: one statement, no window.
    assert_eq!(
        repository
            .transfer("lease-test", "preview", first, second)
            .await
            .expect("transfer"),
        LeaseTransfer::Transferred
    );
    assert_eq!(
        repository
            .holder("lease-test", "preview")
            .await
            .expect("holder"),
        Some(second)
    );

    // A stale transfer attempt reports the state instead of fighting.
    assert_eq!(
        repository
            .transfer("lease-test", "preview", first, second)
            .await
            .expect("stale"),
        LeaseTransfer::RestState {
            holder: Some(second)
        }
    );

    // Release only when the holder matches; anything else keeps the lease.
    let third = Uuid::now_v7();
    assert!(
        !repository
            .release("lease-test", "preview", third)
            .await
            .expect("no-op"),
        "a non-holder cannot release"
    );
    assert!(
        repository
            .release("lease-test", "preview", second)
            .await
            .expect("release")
    );
    assert_eq!(
        repository
            .holder("lease-test", "preview")
            .await
            .expect("gone"),
        None
    );
}
