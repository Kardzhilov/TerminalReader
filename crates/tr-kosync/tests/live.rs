//! Opt-in round-trip tests against a real kosync server.
//!
//! These are `#[ignore]`d so CI never touches the network. Run them against
//! kosync.eu, the official server, or a self-hosted instance with:
//!
//! ```sh
//! TR_SYNC_TEST_SERVER=https://kosync.eu \
//! TR_SYNC_TEST_USER=youruser \
//! TR_SYNC_TEST_PASSWORD=yourpass \
//! cargo test -p tr-kosync --test live -- --ignored
//! ```
//!
//! Set `TR_SYNC_TEST_REGISTER=1` to first register the account.

#![allow(clippy::expect_used)]

use tr_kosync::{Credentials, KOSyncClient, ProgressUpdate, password_hash};

struct LiveConfig {
    server: String,
    username: String,
    password: String,
}

fn live_config() -> Option<LiveConfig> {
    Some(LiveConfig {
        server: std::env::var("TR_SYNC_TEST_SERVER").ok()?,
        username: std::env::var("TR_SYNC_TEST_USER").ok()?,
        password: std::env::var("TR_SYNC_TEST_PASSWORD").ok()?,
    })
}

#[test]
#[ignore = "requires TR_SYNC_TEST_* environment variables and network access"]
fn live_round_trip_push_then_pull() {
    let config = live_config().expect("TR_SYNC_TEST_SERVER/USER/PASSWORD must be set");
    if std::env::var("TR_SYNC_TEST_REGISTER").is_ok() {
        KOSyncClient::register(&config.server, &config.username, &config.password)
            .expect("registration failed");
    }
    let client = KOSyncClient::new(
        &config.server,
        Credentials {
            username: config.username,
            userkey: password_hash(&config.password),
        },
    )
    .expect("client construction failed");

    client.authorize().expect("authorization failed");

    let document = "terminalreader-live-test-0000000000000001".to_owned();
    let update = ProgressUpdate {
        document: document.clone(),
        metadata: None,
        progress: "/body/DocFragment[3]/body/p[7].0".to_owned(),
        percentage: 0.1234,
        device: "terminalreader-test".to_owned(),
        device_id: "terminalreader-live-test".to_owned(),
    };
    client.push(&update).expect("push failed");

    let record = client
        .pull(&document)
        .expect("pull failed")
        .expect("document missing on server after push");
    assert_eq!(record.progress.as_deref(), Some(update.progress.as_str()));
    let percentage = record.percentage.expect("percentage missing");
    assert!((percentage - update.percentage).abs() < 1e-9);
}
