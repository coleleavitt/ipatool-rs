//! Go's `appstore_storefront_test.go` (`storefrontVisionApps`) and the visionOS cases of
//! `appstore_search_test.go`.

use super::*;

fn us() -> CountryCode {
    "us".parse().unwrap()
}

fn item(kind: &str, adam_id: &str, platform: &str, salable_adam_id: &str, version: &str) -> String {
    format!(
        r#"{{"$kind":{kind:?},"lockup":{{"adamId":{adam_id:?},"bundleId":"bundle.{adam_id}","title":"App {adam_id}"}},"offer":{{"purchaseConfiguration":{{"metricsPlatformDisplayStyle":{platform:?},"appPlatforms":[{platform:?}],"buyParams":"salableAdamId={salable_adam_id}&appExtVrsId={version}"}}}}}}"#
    )
}

fn page(items: &[String]) -> Vec<u8> {
    format!(
        "<html><head></head><body><script data-test=\"value\" id='serialized-server-data' type=\"application/json\">{{\n\t\t\"data\":[{{\"data\":{{\"shelves\":[{{\"items\":[{}] }}]}}}}]}}\n\t</script></body></html>",
        items.join(",")
    )
    .into_bytes()
}

fn app(id: i64, bundle_id: &str, name: &str) -> App {
    App {
        id,
        bundle_id: bundle_id.to_owned(),
        name: name.to_owned(),
        ..App::default()
    }
}

#[test]
fn keeps_unique_native_vision_apps_in_storefront_order_and_applies_the_limit() {
    let body = page(&[
        item("EditorialItem", "900", "vision", "900", "9000"),
        item("AppSearchResult", "200", "vision", "200", "2000"),
        item("AppSearchResult", "300", "ios", "300", "3000"),
        item("AppSearchResult", "not-an-id", "vision", "0", "4000"),
        item("AppSearchResult", "200", "vision", "200", "2000"),
        item("AppSearchResult", "100", "vision", "999", "1000"),
        item("AppSearchResult", "100", "vision", "100", "1000"),
        item("AppSearchResult", "400", "vision", "400", "4000"),
    ]);
    assert_eq!(
        storefront_vision_apps(&body, 2).unwrap(),
        [
            app(200, "bundle.200", "App 200"),
            app(100, "bundle.100", "App 100")
        ]
    );
}

#[test]
fn returns_no_apps_for_a_zero_or_negative_limit() {
    let body = page(&[item("AppSearchResult", "100", "vision", "100", "1000")]);
    assert_eq!(storefront_vision_apps(&body, 0).unwrap(), []);
    assert_eq!(storefront_vision_apps(&body, -1).unwrap(), []);
}

#[test]
fn caps_oversized_limits_to_the_storefront_maximum() {
    let items: Vec<String> = (1..=13)
        .map(|index| {
            let id = index.to_string();
            item("AppSearchResult", &id, "vision", &id, &format!("{id}0"))
        })
        .collect();
    assert_eq!(
        storefront_vision_apps(&page(&items), 1 << 62)
            .unwrap()
            .len(),
        12
    );
}

#[test]
fn reports_missing_and_invalid_serialized_data() {
    assert_eq!(
        storefront_vision_apps(b"<html></html>", 5)
            .unwrap_err()
            .to_string(),
        "serialized server data was not found"
    );
    let error = storefront_vision_apps(
        br#"<script id="serialized-server-data">not-json</script>"#,
        5,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with("failed to decode serialized server data"),
        "{error}"
    );
}

#[test]
fn builds_the_go_vision_search_and_metadata_urls() {
    assert_eq!(
        vision_search_url("spatial notes", &us()),
        "https://apps.apple.com/us/vision/search?term=spatial+notes"
    );
    assert_eq!(
        lookup_ids_url(&[200, 100], &us(), Some(Platform::VisionOs))
            .unwrap()
            .as_str(),
        "https://itunes.apple.com/lookup?country=US&entity=xrosSoftware&id=200%2C100"
    );
}

#[test]
fn hydrates_metadata_in_storefront_order() {
    // The search page of Go's "filters native results, applies the limit, hydrates
    // metadata, and preserves storefront order".
    let body = br#"<html><script type="application/json" id="serialized-server-data">{
        "data":[{"data":{"shelves":[{"items":[
            {"$kind":"EditorialItem","lockup":{"adamId":"999"},"purchaseConfiguration":{"metricsPlatformDisplayStyle":"vision","appPlatforms":["vision"],"buyParams":"salableAdamId=999&appExtVrsId=9990"}},
            {"$kind":"AppSearchResult","lockup":{"adamId":"200","bundleId":"fallback.two","title":"Fallback Two"},"purchaseConfiguration":{"metricsPlatformDisplayStyle":"vision","appPlatforms":["vision"],"buyParams":"salableAdamId=200&appExtVrsId=2000"}},
            {"$kind":"AppSearchResult","lockup":{"adamId":"300","bundleId":"wrong.platform","title":"Wrong Platform"},"purchaseConfiguration":{"metricsPlatformDisplayStyle":"ios","appPlatforms":["iphone"],"buyParams":"salableAdamId=300&appExtVrsId=3000"}},
            {"$kind":"AppSearchResult","lockup":{"adamId":"100","bundleId":"fallback.one","title":"Fallback One"},"purchaseConfiguration":{"metricsPlatformDisplayStyle":"vision","appPlatforms":["vision"],"buyParams":"salableAdamId=100&appExtVrsId=1000"}},
            {"$kind":"AppSearchResult","lockup":{"adamId":"400","bundleId":"limited.out","title":"Limited Out"},"purchaseConfiguration":{"metricsPlatformDisplayStyle":"vision","appPlatforms":["vision"],"buyParams":"salableAdamId=400&appExtVrsId=4000"}}
        ] }]}}]}
    </script></html>"#;
    let apps = storefront_vision_apps(body, 2).unwrap();
    assert_eq!(
        apps.iter().map(|app| app.id).collect::<Vec<_>>(),
        [200, 100]
    );
    let metadata = vec![
        App {
            version: "1.0".to_owned(),
            ..app(100, "hydrated.one", "Hydrated One")
        },
        App {
            version: "2.0".to_owned(),
            ..app(200, "hydrated.two", "Hydrated Two")
        },
    ];
    let vision = |app: App| App {
        platforms: vec![Platform::VisionOs],
        ..app
    };
    assert_eq!(
        hydrate_vision_apps(apps, metadata.clone()),
        [vision(metadata[1].clone()), vision(metadata[0].clone())]
    );
}

#[test]
fn preserves_confirmed_vision_support() {
    let with = |platforms: Vec<Platform>| App {
        platforms,
        ..app(42, "", "")
    };
    for (metadata, expected) in [
        (vec![], vec![Platform::VisionOs]),
        (
            vec![with(vec![Platform::Unknown])],
            vec![Platform::VisionOs],
        ),
        (
            vec![with(vec![Platform::IPhone, Platform::IPad])],
            vec![Platform::IPhone, Platform::IPad, Platform::VisionOs],
        ),
        (
            vec![with(vec![Platform::VisionOs])],
            vec![Platform::VisionOs],
        ),
    ] {
        let hydrated = hydrate_vision_apps(vec![app(42, "", "")], metadata);
        assert_eq!(hydrated[0].platforms, expected);
    }
}
