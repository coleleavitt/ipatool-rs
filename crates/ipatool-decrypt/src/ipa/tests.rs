use super::*;

fn names(list: &[&str]) -> HashSet<String> {
    list.iter().map(|&name| name.to_owned()).collect()
}

#[test]
fn page_key_files_rank_newest_first() {
    let all = names(&[
        "Payload/A.app/SC_Info/A.supf",
        "Payload/A.app/SC_Info/A.supp",
        "Payload/A.app/SC_Info/A.v4.supp",
        "Payload/A.app/SC_Info/A.v5.supf",
        "Payload/A.app/SC_Info/B.supp",
        "Payload/A.app/SC_Info/B.supf",
        "Payload/A.app/SC_Info/C.supp",
        "Payload/A.app/SC_Info/D.v4.supp",
        "Payload/A.app/SC_Info/D.supp",
        "Payload/A.app/SC_Info/A.sinf",
    ]);
    assert_eq!(
        supf_paths(&all),
        [
            "Payload/A.app/SC_Info/A.v5.supf",
            "Payload/A.app/SC_Info/B.supf",
            "Payload/A.app/SC_Info/C.supp",
            "Payload/A.app/SC_Info/D.v4.supp",
        ]
    );
}

#[test]
fn plan_prefers_v4_sinf_for_new_key_files() {
    let all = names(&[
        "Payload/A.app/A",
        "Payload/A.app/SC_Info/A.sinf",
        "Payload/A.app/SC_Info/A.v4.sinf",
        "Payload/A.app/SC_Info/A.supf",
        "Payload/A.app/SC_Info/A.v5.supf",
        "Payload/A.app/F.framework/F",
        "Payload/A.app/F.framework/SC_Info/F.v4.supp",
        "Payload/A.app/G.framework/G",
        "Payload/A.app/G.framework/SC_Info/G.supf",
        "Payload/A.app/G.framework/SC_Info/G.v4.sinf",
    ]);
    let plan: Vec<(String, String)> = plan_entries(&all)
        .expect("plan")
        .into_iter()
        .map(|entry| (entry.binary_path, entry.sinf_path))
        .collect();
    assert_eq!(
        plan,
        [
            (
                "Payload/A.app/F.framework/F".to_owned(),
                "Payload/A.app/F.framework/SC_Info/F.sinf".to_owned()
            ),
            (
                "Payload/A.app/G.framework/G".to_owned(),
                "Payload/A.app/G.framework/SC_Info/G.sinf".to_owned()
            ),
            (
                "Payload/A.app/A".to_owned(),
                "Payload/A.app/SC_Info/A.v4.sinf".to_owned()
            ),
        ]
    );
}

#[test]
fn plan_errors() {
    let err = plan_entries(&names(&["Payload/A.app/A.supf"])).unwrap_err();
    assert_eq!(
        err.to_string(),
        "SUPF/SUPP outside SC_Info: Payload/A.app/A.supf"
    );
    let err = plan_entries(&names(&["Payload/A.app/SC_Info/A.supp"])).unwrap_err();
    assert_eq!(
        err.to_string(),
        "SUPF/SUPP lacks binary/SINF: Payload/A.app/SC_Info/A.supp"
    );
}

#[test]
fn enclosing_app_sinf_choice() {
    let all = names(&[
        "Payload/A.app/SC_Info/A.v4.sinf",
        "Payload/A.app/SC_Info/Z.sinf",
        "Payload/A.app/SC_Info/deeper/A.sinf",
    ]);
    assert_eq!(
        enclosing_app_sinf(&all, "Payload/A.app/Frameworks/F.framework").as_deref(),
        Some("Payload/A.app/SC_Info/A.v4.sinf")
    );
    let all = names(&[
        "Payload/A.app/SC_Info/Y.v4.sinf",
        "Payload/A.app/SC_Info/Z.sinf",
    ]);
    assert_eq!(
        enclosing_app_sinf(&all, "Payload/A.app/PlugIns/E.appex").as_deref(),
        Some("Payload/A.app/SC_Info/Z.sinf")
    );
    let all = names(&["Payload/A.app/SC_Info/Y.v4.sinf"]);
    assert_eq!(
        enclosing_app_sinf(&all, "Payload/A.app/PlugIns/E.appex").as_deref(),
        Some("Payload/A.app/SC_Info/Y.v4.sinf")
    );
    // The nearest enclosing .app wins.
    let all = names(&[
        "Payload/A.app/SC_Info/A.sinf",
        "Payload/A.app/Watch/W.app/SC_Info/W.sinf",
    ]);
    assert_eq!(
        enclosing_app_sinf(&all, "Payload/A.app/Watch/W.app/PlugIns/X.appex").as_deref(),
        Some("Payload/A.app/Watch/W.app/SC_Info/W.sinf")
    );
    assert_eq!(enclosing_app_sinf(&all, "Payload/Other"), None);
}

#[test]
fn excluded_entries_match_the_device_packager() {
    for name in [
        "iTunesMetadata.plist",
        "Payload/iTunesMetadata.plist",
        "Payload/iTunesArtwork",
        "Payload/META-INF/",
        "Payload/META-INF/x",
        "Payload/A.app/SC_Info/",
        "Payload/A.app/SC_Info/A.sinf",
        "Payload/A.app/Frameworks/F.framework/SC_Info/F.supf",
        "Payload/A.app/BCSymbolMaps/x",
        "Payload/A.app/Symbols/x",
        "Payload/A.app/A.app.dSYM/Contents",
        "Payload/A.app/Watch/W.app/W",
        "Payload/A.app/WatchKitSupport2/WK",
    ] {
        assert!(is_excluded_ipa_entry(name, false), "{name}");
    }
    for name in [
        "Payload/",
        "Payload/A.app/",
        "Payload/A.app/A",
        "Payload/A.app/Info.plist",
        "Payload/SC_Info/x",
        "META-INF/x",
    ] {
        assert!(!is_excluded_ipa_entry(name, false), "{name}");
    }
    assert!(!is_excluded_ipa_entry("Payload/A.app/Watch/W.app/W", true));
    assert!(is_excluded_ipa_entry(
        "Payload/A.app/Watch/W.app/SC_Info/W.sinf",
        true
    ));
}

#[test]
fn stems() {
    assert_eq!(path_stem("Payload/A.app"), "A");
    assert_eq!(path_stem("Payload/A.b.app"), "A.b");
    assert_eq!(path_stem("Payload/A"), "A");
}

#[test]
fn rejects_bad_credentials_and_existing_output() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = dir.path().join("out.ipa");
    let short = Credentials {
        standard: vec![0; 15],
        arcade: None,
    };
    let err = decrypt_ipa(
        Path::new("missing.ipa"),
        &output,
        &short,
        Options::default(),
    )
    .unwrap_err();
    assert_eq!(err.to_string(), "credential must contain exactly 16 bytes");
    let bad_arcade = Credentials {
        standard: vec![0; 16],
        arcade: Some(vec![0; 17]),
    };
    let err = decrypt_ipa(
        Path::new("missing.ipa"),
        &output,
        &bad_arcade,
        Options::default(),
    )
    .unwrap_err();
    assert_eq!(err.to_string(), "credential must contain exactly 16 bytes");
    fs::write(&output, b"x").expect("write");
    let ok = Credentials {
        standard: vec![0; 16],
        arcade: None,
    };
    let err = decrypt_ipa(Path::new("missing.ipa"), &output, &ok, Options::default()).unwrap_err();
    assert!(matches!(err, Error::OutputExists(_)), "{err}");
}
