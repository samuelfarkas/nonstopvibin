//! A profile's usage hub, as T3 Code v0.1.13 reads it: its URL and key from the management
//! API, then `/p/<slug>/v0/management/*` with that key, over the fake cores. No provider is
//! contacted and no real core runs.

use nsv_testkit::TempDir;

use crate::features::{Running, start};

async fn running(name: &str) -> Running {
    start(TempDir::new(name), None, Vec::new()).await
}

/// The management API's view of a profile's hub: its URL and key.
async fn usage_hub(app: &Running, method: &str, profile: &str) -> (String, String) {
    let hub = app
        .ok(method, &format!("/profiles/{profile}/usage-hub"), None)
        .await;
    (
        hub["url"].as_str().unwrap().to_owned(),
        hub["key"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn the_usage_hub_is_the_profile_route_and_keeps_its_key_until_replaced() {
    let app = running("hub-route").await;
    let profile = app.profile("Work Hub").await;
    let (url, key) = usage_hub(&app, "GET", &profile).await;
    assert_eq!(
        url,
        format!("http://127.0.0.1:{}/p/work-hub", app.daemon.port())
    );
    let shape = key.strip_prefix("nvh_").unwrap();
    assert_eq!(shape.len(), 43, "{key}");
    assert_eq!(
        usage_hub(&app, "GET", &profile).await,
        (url.clone(), key.clone())
    );

    let (replaced_url, replaced) = usage_hub(&app, "POST", &profile).await;
    assert_eq!(replaced_url, url);
    assert_ne!(replaced, key);
    assert_eq!(usage_hub(&app, "GET", &profile).await.1, replaced);
    app.close().await;
}
