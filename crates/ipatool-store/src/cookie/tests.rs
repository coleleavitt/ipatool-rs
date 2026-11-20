use serde_json::Value;

use super::*;

fn oracle_cookies() -> Vec<Value> {
    let oracle: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/network-oracle.json"))
            .expect("the oracle fixture is valid JSON");
    oracle["cookies"].as_array().cloned().unwrap_or_default()
}

#[test]
fn parses_set_cookie_lines_like_go() {
    let cases = oracle_cookies();
    assert!(cases.len() > 30);
    for case in cases {
        let line = case["Line"].as_str().unwrap_or_default();
        let parsed = parse_set_cookie(line);
        assert_eq!(parsed.is_some(), case["OK"], "{line:?}");
        let Some(cookie) = parsed else { continue };
        let text = |key: &str| case[key].as_str().unwrap_or_default().to_owned();
        assert_eq!(cookie.name, text("Name"), "{line:?}");
        assert_eq!(cookie.value, text("Value"), "{line:?}");
        assert_eq!(cookie.path, text("Path"), "{line:?}");
        assert_eq!(cookie.domain, text("Domain"), "{line:?}");
        assert_eq!(cookie.quoted, case["Quoted"], "{line:?}");
        assert_eq!(cookie.secure, case["Secure"], "{line:?}");
        assert_eq!(cookie.http_only, case["HTTPOnly"], "{line:?}");
        let expires = case["HasExpiry"].as_bool().unwrap_or_default().then(|| {
            Date::unix(
                case["ExpiresUnix"].as_i64().unwrap_or_default(),
                case["ExpiresNanos"].as_i64().unwrap_or_default(),
            )
        });
        assert_eq!(cookie.expires, expires, "{line:?}");
        let mut headers = Header::new();
        add_cookie(
            &mut headers,
            &Cookie {
                name: "first".to_owned(),
                value: "1".to_owned(),
                ..Cookie::default()
            },
        );
        add_cookie(&mut headers, &cookie);
        assert_eq!(headers.get("Cookie"), text("AddCookie"), "{line:?}");
    }
}

#[test]
fn reads_every_set_cookie_line() {
    let mut headers = Header::new();
    headers.add("Set-Cookie", "a=1");
    headers.add("Set-Cookie", "bad");
    headers.add("set-cookie", "b=2; Secure");
    let cookies = read_set_cookies(&headers);
    assert_eq!(
        cookies.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
}
