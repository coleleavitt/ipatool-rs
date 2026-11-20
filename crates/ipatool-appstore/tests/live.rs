//! Live checks against Apple. Run with `IPATOOL_TEST_LIVE=1`.

use ipatool_appstore::{Client, machine};

fn live() -> bool {
    std::env::var_os("IPATOOL_TEST_LIVE").is_some()
}

#[tokio::test]
async fn bag_and_storefronts() {
    if !live() {
        return;
    }
    let client = Client::new().unwrap();
    let (guid, _) = machine::identity(&machine::mac_address().unwrap()).unwrap();
    let config = client.sap_config(&guid).await.unwrap();
    assert!(
        config.auth_endpoint.ends_with("/authenticate/")
            || config
                .auth_endpoint
                .starts_with("https://auth.itunes.apple.com/auth/v1/native"),
        "{config:?}"
    );
    assert_eq!(config.version, 200);

    assert_eq!(
        client
            .country_code_from_store_front("143441-1,29")
            .await
            .unwrap(),
        "US"
    );
    assert_eq!(
        client
            .country_code_from_store_front("143443-2,32")
            .await
            .unwrap(),
        "DE"
    );
    assert_eq!(client.discover_storefront_id("gb").await.unwrap(), "143444");
    assert!(
        client
            .country_code_from_store_front("999999")
            .await
            .is_err()
    );
}
