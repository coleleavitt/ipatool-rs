use super::*;

const KARING_MAC_CONFIGURATION: &str = r#"{"purchaseConfiguration":{"bundleId":"com.nebula.karing","appPlatforms":["mac","tv","phone","pad"],"metricsPlatformDisplayStyle":"ios","buyParams":"salableAdamId=6472431552&appExtVrsId=876660716"}}"#;

/// Go's `macVersionPage`.
pub(crate) fn mac_version_page(configuration: &str) -> Vec<u8> {
    format!("<script id=\"serialized-server-data\">{configuration}</script>").into_bytes()
}

pub(crate) fn karing() -> App {
    App {
        id: 6_472_431_552,
        bundle_id: "com.nebula.karing".to_owned(),
        name: String::new(),
        version: String::new(),
        price: 0.0,
        platforms: Vec::new(),
    }
}

#[test]
fn selects_karings_mac_offer_and_ignores_unrelated_and_ios_offers() {
    let body = mac_version_page(&format!(
        r#"[{{"purchaseConfiguration":{{"bundleId":"com.nebula.karing","appPlatforms":["phone","pad"],"buyParams":"salableAdamId=6472431552&appExtVrsId=891116578"}}}}, {{"purchaseConfiguration":{{"appPlatforms":["mac"],"buyParams":"salableAdamId=100&appExtVrsId=123"}}}}, {KARING_MAC_CONFIGURATION}]"#
    ));
    assert_eq!(
        macos_external_version_id(&body, &karing()).expect("version"),
        "876660716"
    );
}

#[test]
fn rejects_invalid_catalog_metadata() {
    let conflicting = format!(
        r#"[{KARING_MAC_CONFIGURATION},{{"purchaseConfiguration":{{"bundleId":"com.nebula.karing","appPlatforms":["mac"],"buyParams":"salableAdamId=6472431552&appExtVrsId=123"}}}}]"#
    );
    let cases: &[(&str, &str, &str)] = &[
        ("invalid JSON", "{", "failed to decode Mac product page"),
        ("no offer", "{}", "has no external version id"),
        (
            "iOS only",
            r#"{"purchaseConfiguration":{"bundleId":"com.nebula.karing","appPlatforms":["phone","pad"],"buyParams":"salableAdamId=6472431552&appExtVrsId=891116578"}}"#,
            "has no external version id",
        ),
        (
            "wrong bundle",
            r#"{"purchaseConfiguration":{"bundleId":"other","appPlatforms":["mac"],"buyParams":"salableAdamId=6472431552&appExtVrsId=123"}}"#,
            "has no external version id",
        ),
        (
            "invalid version",
            r#"{"purchaseConfiguration":{"bundleId":"com.nebula.karing","appPlatforms":["mac"],"buyParams":"salableAdamId=6472431552&appExtVrsId=invalid"}}"#,
            "has no external version id",
        ),
        (
            "bad query",
            r#"{"purchaseConfiguration":{"bundleId":"com.nebula.karing","appPlatforms":["mac"],"buyParams":"salableAdamId=6472431552&appExtVrsId=123&x=%zz"}}"#,
            "has no external version id",
        ),
        (
            "conflicting offers",
            &conflicting,
            "conflicting external version ids",
        ),
    ];
    for (name, configuration, expected) in cases {
        let err = macos_external_version_id(&mac_version_page(configuration), &karing())
            .expect_err(name)
            .to_string();
        assert!(err.contains(expected), "{name}: {err}");
    }
}

#[test]
fn repeated_identical_offers_are_one_version() {
    let body = mac_version_page(&format!(
        "[{KARING_MAC_CONFIGURATION},{KARING_MAC_CONFIGURATION}]"
    ));
    assert_eq!(
        macos_external_version_id(&body, &karing()).expect("version"),
        "876660716"
    );
}

#[test]
fn finds_the_serialized_server_data_script() {
    assert_eq!(
        serialized_server_data(b"<html><script type=\"application/json\" id='serialized-server-data'>\n {} \n</script>")
            .expect("data"),
        b"{}"
    );
    let errors: &[(&[u8], &str)] = &[
        (b"<html>", "serialized server data was not found"),
        (
            b"<div id=\"serialized-server-data\">{}</div>",
            "serialized server data script was not found",
        ),
        (
            b"<script id=\"serialized-server-data\"",
            "serialized server data script is malformed",
        ),
        (
            b"<script id=\"serialized-server-data\">{}",
            "serialized server data script is not closed",
        ),
    ];
    for (body, expected) in errors {
        assert_eq!(
            serialized_server_data(body)
                .expect_err(expected)
                .to_string(),
            *expected
        );
    }
}
