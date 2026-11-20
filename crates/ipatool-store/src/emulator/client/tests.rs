use serde_json::Value as Json;

use super::*;

#[test]
fn allows_urls_like_go() {
    let oracle: Json =
        serde_json::from_str(include_str!("../../../tests/fixtures/network-oracle.json"))
            .expect("the oracle fixture is valid JSON");
    let urls = oracle["urls"].as_object().expect("urls");
    for (raw, want) in urls {
        let Ok(url) = gourl::parse(raw) else { continue };
        assert_eq!(
            allowed_url(&url, true),
            want["AllowedAuthenticated"],
            "{raw:?}"
        );
        assert_eq!(
            allowed_url(&url, false),
            want["AllowedAnonymous"],
            "{raw:?}"
        );
    }
}

#[test]
fn prints_parameters_like_fmt_sprint() {
    assert_eq!(go_sprint(None), "<nil>");
    assert_eq!(go_sprint(Some(&Value::Integer(0))), "0");
    assert_eq!(go_sprint(Some(&Value::Real(0.0))), "0");
    assert_eq!(go_sprint(Some(&Value::Real(1e21))), "1e+21");
    assert_eq!(go_sprint(Some(&Value::String("C".to_owned()))), "C");
    assert_eq!(go_sprint(Some(&Value::Boolean(true))), "true");
}
