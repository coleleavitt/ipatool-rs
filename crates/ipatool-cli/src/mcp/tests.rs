//! Ports of Go `cmd/mcp_test.go` and `cmd/mcp_app_test.go`. The server runs over an
//! in-memory pipe, as Go's in-memory transports, with a scripted store; the argument
//! validation cases are Go binary output.

#![allow(clippy::unused_async_trait_impl, reason = "test fixtures")]

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use clap::Parser as _;
use ipatool_appstore::{
    Account, App, CatalogVersion, DownloadInput, DownloadOutput, Error, ListVersionsOutput,
    LookupQuery, OwnedAppsInput, OwnedAppsOutput, Platform, Sinf, VersionMetadata,
};
use serde_json::{Value, json};
use time::macros::datetime;
use tokio::io::{
    AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, ReadHalf, WriteHalf,
};

use super::app::complete_app;
use crate::commerce::{AppResult, Store};

type Handler<I, O> = RefCell<Box<dyn FnMut(I) -> AppResult<O>>>;

fn handler<I, O>(f: impl FnMut(I) -> AppResult<O> + 'static) -> Handler<I, O> {
    RefCell::new(Box::new(f))
}

fn unexpected<I, O>(name: &'static str) -> Handler<I, O> {
    handler(move |_| panic!("unexpected {name} call"))
}

/// Go's `fakeMCPAppStore`: every App Store call is a replaceable function.
struct Fake {
    account_info: Handler<(), Account>,
    login: Handler<Account, Account>,
    search: Handler<(Account, String, i64, Option<Platform>), Vec<App>>,
    lookup: Handler<(Account, LookupQuery, Option<Platform>), App>,
    purchase: Handler<(Account, App, Option<Platform>), ()>,
    download: Handler<DownloadInput, DownloadOutput>,
    replicate_sinf: Handler<PathBuf, ()>,
    owned_apps: Handler<OwnedAppsInput, OwnedAppsOutput>,
    list_versions: Handler<(Account, App, Option<Platform>), ListVersionsOutput>,
    get_version_metadata: Handler<(Account, App, String, Option<Platform>), VersionMetadata>,
}

impl Fake {
    /// Go's `BeforeEach`: the saved account, and a catalog without the app.
    fn new(account: &Account) -> Self {
        let account = account.clone();
        Self {
            account_info: handler(move |()| Ok(account.clone())),
            login: unexpected("login"),
            search: unexpected("search"),
            lookup: handler(|_| Err(Error::AppNotFound)),
            purchase: unexpected("purchase"),
            download: unexpected("download"),
            replicate_sinf: unexpected("replicate_sinf"),
            owned_apps: unexpected("owned_apps"),
            list_versions: unexpected("list_versions"),
            get_version_metadata: unexpected("get_version_metadata"),
        }
    }
}

impl Store for Fake {
    async fn account_info(&self) -> AppResult<Account> {
        (self.account_info.borrow_mut())(())
    }

    async fn login(&self, account: &Account) -> AppResult<Account> {
        (self.login.borrow_mut())(account.clone())
    }

    async fn search(
        &self,
        account: &Account,
        term: &str,
        limit: i64,
        platform: Option<Platform>,
    ) -> AppResult<Vec<App>> {
        (self.search.borrow_mut())((account.clone(), term.to_owned(), limit, platform))
    }

    async fn lookup(
        &self,
        account: &Account,
        query: &LookupQuery,
        platform: Option<Platform>,
    ) -> AppResult<App> {
        (self.lookup.borrow_mut())((account.clone(), query.clone(), platform))
    }

    async fn purchase(
        &self,
        account: &mut Account,
        app: &App,
        platform: Option<Platform>,
    ) -> AppResult<()> {
        (self.purchase.borrow_mut())((account.clone(), app.clone(), platform))
    }

    async fn download(&self, input: DownloadInput) -> AppResult<DownloadOutput> {
        (self.download.borrow_mut())(input)
    }

    async fn replicate_sinf(&self, _: Vec<Sinf>, package: PathBuf) -> AppResult<()> {
        (self.replicate_sinf.borrow_mut())(package)
    }

    async fn owned_apps(&self, input: OwnedAppsInput) -> AppResult<OwnedAppsOutput> {
        (self.owned_apps.borrow_mut())(input)
    }

    async fn list_versions(
        &self,
        account: &Account,
        app: &App,
        platform: Option<Platform>,
    ) -> AppResult<ListVersionsOutput> {
        (self.list_versions.borrow_mut())((account.clone(), app.clone(), platform))
    }

    async fn get_version_metadata(
        &self,
        account: &Account,
        app: &App,
        version_id: &str,
        platform: Option<Platform>,
    ) -> AppResult<VersionMetadata> {
        (self.get_version_metadata.borrow_mut())((
            account.clone(),
            app.clone(),
            version_id.to_owned(),
            platform,
        ))
    }

    async fn catalog_version_history(
        &self,
        _: &Account,
        _: &App,
        _: Option<Platform>,
    ) -> AppResult<Vec<CatalogVersion>> {
        unreachable!("not an MCP tool")
    }
}

/// A shared record of the inputs a scripted call saw.
fn recorder<T>() -> Rc<RefCell<Vec<T>>> {
    Rc::new(RefCell::new(Vec::new()))
}

fn account() -> Account {
    Account {
        email: "private-email".to_owned(),
        password: "private-password".to_owned(),
        password_token: "private-token".to_owned(),
        ..Account::default()
    }
}

fn example_app() -> App {
    App {
        id: 123,
        bundle_id: "com.example.app".to_owned(),
        name: "Example".to_owned(),
        ..App::default()
    }
}

fn download_output(path: &str) -> DownloadOutput {
    DownloadOutput {
        destination_path: PathBuf::from(path),
        sinfs: Vec::new(),
    }
}

/// A line-based JSON-RPC client, as an MCP client speaks over stdio.
struct Client {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    next_id: u64,
}

impl Client {
    async fn send(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        loop {
            let mut line = String::new();
            assert_ne!(
                self.reader.read_line(&mut line).await.unwrap(),
                0,
                "server closed"
            );
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }

    /// `session.CallTool`: the tool result.
    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        let response = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await;
        response["result"].clone()
    }
}

fn is_error(result: &Value) -> bool {
    result["isError"] == true
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

/// Runs `test` against a connected, initialized server; the server must end cleanly
/// when the client goes away.
async fn with_server(store: &Fake, test: impl AsyncFnOnce(&mut Client)) {
    let (client, server) = tokio::io::duplex(1 << 16);
    let (reader, writer) = tokio::io::split(client);
    let client = async move {
        let mut client = Client {
            reader: BufReader::new(reader),
            writer,
            next_id: 0,
        };
        let initialize = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1" },
                }),
            )
            .await;
        assert_eq!(initialize["result"]["serverInfo"]["name"], "ipatool");
        client
            .send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        test(&mut client).await;
    };
    let (result, ()) = tokio::join!(
        Box::pin(super::serve(store, tokio::io::split(server))),
        client
    );
    assert_eq!(result, Ok(()));
}

#[test]
fn registers_the_mcp_subcommand() {
    let cli = crate::Cli::try_parse_from(["ipatool", "mcp"]).unwrap();
    assert!(matches!(cli.command, crate::Command::Mcp));
    assert!(crate::interactive(
        &crate::Cli::try_parse_from(["ipatool", "search", "x"]).unwrap()
    ));
    assert!(!crate::interactive(&cli));
    assert!(crate::Cli::try_parse_from(["ipatool", "mcp", "unexpected"]).is_err());
}

#[tokio::test]
async fn advertises_exactly_six_tools_and_their_schemas_without_auth_functions() {
    let fake = Fake::new(&account());
    with_server(&fake, async |client| {
        let result = client.request("tools/list", json!({})).await["result"].clone();
        let mut names = Vec::new();
        for tool in result["tools"].as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            names.push(name.to_owned());
            assert!(tool["inputSchema"].is_object());
            assert!(tool["outputSchema"].is_object());
            let read_only = name != "download_app" && name != "purchase_app";
            assert_eq!(tool["annotations"]["readOnlyHint"], read_only, "{name}");
        }
        names.sort();
        assert_eq!(
            names,
            [
                "download_app",
                "get_version_metadata",
                "list_app_versions",
                "list_purchases",
                "purchase_app",
                "search_apps"
            ]
        );
    })
    .await;
}

#[tokio::test]
async fn lists_the_tools_exactly_as_the_go_server() {
    let mut expected: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/mcp_tools_list.json")).unwrap();
    expected.as_object_mut().unwrap().remove("_source");
    let fake = Fake::new(&account());
    with_server(&fake, async |client| {
        let result = client.request("tools/list", json!({})).await["result"].clone();
        assert_eq!(result, expected);
    })
    .await;
}

#[tokio::test]
async fn searches_with_cli_defaults_and_returns_structured_app_data_without_credentials() {
    let mut fake = Fake::new(&account());
    let inputs = recorder();
    let seen = inputs.clone();
    fake.search = handler(move |input| {
        seen.borrow_mut().push(input);
        Ok(vec![example_app()])
    });
    with_server(&fake, async |client| {
        let result = client
            .call("search_apps", json!({ "term": "example" }))
            .await;
        assert!(!is_error(&result));
        let data = result["structuredContent"].to_string();
        assert!(data.contains("com.example.app"));
        assert!(!data.contains("private-"));
        assert_eq!(result["content"].as_array().unwrap().len(), 1);
        assert_eq!(result["structuredContent"]["term"], "example");
        assert_eq!(result["structuredContent"]["limit"], 5);
        assert_eq!(result["structuredContent"]["platform"], "");
    })
    .await;
    assert_eq!(
        *inputs.borrow(),
        [(account(), "example".to_owned(), 5, None)]
    );
}

#[tokio::test]
async fn passes_search_platform_aliases_and_limits_to_the_app_store() {
    for (value, platform) in [("tvos", Platform::AppleTv), ("watch", Platform::WatchOs)] {
        let mut fake = Fake::new(&account());
        let inputs = recorder();
        let seen = inputs.clone();
        fake.search = handler(move |(_, _, limit, platform)| {
            seen.borrow_mut().push((limit, platform));
            Ok(Vec::new())
        });
        with_server(&fake, async |client| {
            let arguments = json!({ "term": "example", "platform": value, "limit": 7 });
            assert!(!is_error(&client.call("search_apps", arguments).await));
        })
        .await;
        assert_eq!(*inputs.borrow(), [(7, Some(platform))]);
    }
}

#[tokio::test]
async fn rejects_invalid_inputs_before_accessing_the_account() {
    let cases = [
        ("search_apps", json!({})),
        ("search_apps", json!({ "term": "  " })),
        ("search_apps", json!({ "term": "app", "limit": 0 })),
        ("search_apps", json!({ "term": 123 })),
        (
            "search_apps",
            json!({ "term": "app", "password": "secret" }),
        ),
        (
            "search_apps",
            json!({ "term": "app", "platform": "unknown" }),
        ),
        ("download_app", json!({})),
        ("purchase_app", json!({ "app_id": -1 })),
        ("list_app_versions", json!({})),
        (
            "get_version_metadata",
            json!({ "external_version_id": "200" }),
        ),
        ("get_version_metadata", json!({ "app_id": 123 })),
        (
            "get_version_metadata",
            json!({ "app_id": 123, "external_version_id": "" }),
        ),
        (
            "get_version_metadata",
            json!({ "app_id": 123, "external_version_id": "  " }),
        ),
        (
            "get_version_metadata",
            json!({ "app_id": 123, "external_version_id": 200 }),
        ),
        (
            "get_version_metadata",
            json!({ "app_id": 123, "external_version_id": "200", "platform": "unknown" }),
        ),
        ("list_purchases", json!({ "page": 0 })),
        ("list_purchases", json!({ "max_results": 101 })),
    ];
    let mut fake = Fake::new(&account());
    let account_calls = Rc::new(Cell::new(0));
    let calls = account_calls.clone();
    fake.account_info = handler(move |()| {
        calls.set(calls.get() + 1);
        Err(Error::Invalid("unexpected account access".to_owned()))
    });
    with_server(&fake, async |client| {
        for (name, arguments) in cases {
            let result = client.call(name, arguments.clone()).await;
            assert!(is_error(&result), "{name} {arguments}");
        }
    })
    .await;
    assert_eq!(account_calls.get(), 0);
}

/// The texts of the Go binary (`ipatool mcp`) for the same calls.
#[tokio::test]
async fn reports_validation_errors_as_the_go_server() {
    let cases = [
        (
            "search_apps",
            json!({}),
            r#"validating "arguments": validating root: required: missing properties: ["term"]"#,
        ),
        (
            "search_apps",
            Value::Null,
            r#"validating "arguments": validating root: required: missing properties: ["term"]"#,
        ),
        (
            "search_apps",
            json!({ "term": "  " }),
            "search_apps failed: search term must not be empty",
        ),
        (
            "search_apps",
            json!({ "term": "app", "limit": 0 }),
            "search_apps failed: limit must be greater than 0",
        ),
        (
            "search_apps",
            json!({ "term": 123 }),
            r#"validating "arguments": validating root: validating /properties/term: type: 123 has type "integer", want "string""#,
        ),
        (
            "search_apps",
            json!({ "term": "app", "password": "secret" }),
            r#"validating "arguments": validating root: unexpected additional properties ["password"]"#,
        ),
        (
            "search_apps",
            json!({ "term": "app", "platform": "unknown" }),
            r#"search_apps failed: invalid platform "unknown""#,
        ),
        (
            "search_apps",
            json!({ "term": "x", "limit": 1.5 }),
            r#"validating "arguments": validating root: validating /properties/limit: type: 1.5 has type "number", want one of "null, integer""#,
        ),
        (
            "search_apps",
            json!({ "term": null }),
            r#"validating "arguments": validating root: validating /properties/term: type: <invalid reflect.Value> has type "null", want "string""#,
        ),
        (
            "search_apps",
            json!({ "term": { "a": 1, "b": "c" } }),
            r#"validating "arguments": validating root: validating /properties/term: type: map[a:1 b:c] has type "object", want "string""#,
        ),
        (
            "search_apps",
            json!({ "term": [1, "a", null] }),
            r#"validating "arguments": validating root: validating /properties/term: type: [1 a <nil>] has type "array", want "string""#,
        ),
        (
            "search_apps",
            json!({ "term": "x", "limit": "5" }),
            r#"validating "arguments": validating root: validating /properties/limit: type: 5 has type "string", want one of "null, integer""#,
        ),
        (
            "download_app",
            json!({ "purchase": null }),
            r#"validating "arguments": validating root: validating /properties/purchase: type: <invalid reflect.Value> has type "null", want "boolean""#,
        ),
        (
            "purchase_app",
            json!({ "app_id": -1 }),
            "purchase_app failed: a positive app ID or a bundle identifier must be specified",
        ),
        (
            "purchase_app",
            json!({ "app_id": -5.0 }),
            "purchase_app failed: a positive app ID or a bundle identifier must be specified",
        ),
        (
            "purchase_app",
            json!({ "app_id": "abc" }),
            r#"validating "arguments": validating root: validating /properties/app_id: type: abc has type "string", want "integer""#,
        ),
        (
            "get_version_metadata",
            json!({ "app_id": 123 }),
            r#"validating "arguments": validating root: required: missing properties: ["external_version_id"]"#,
        ),
        (
            "get_version_metadata",
            json!({ "app_id": 123, "external_version_id": "  " }),
            "get_version_metadata failed: external version identifier must not be empty",
        ),
        (
            "list_purchases",
            json!({ "page": 0.0 }),
            "list_purchases failed: page must be greater than 0",
        ),
        (
            "list_purchases",
            json!({ "page": null, "max_results": 0 }),
            "list_purchases failed: max results must be between 1 and 100",
        ),
        (
            "list_purchases",
            json!({ "x": 1, "y": 2 }),
            r#"validating "arguments": validating root: unexpected additional properties ["x" "y"]"#,
        ),
    ];
    let fake = Fake::new(&account());
    with_server(&fake, async |client| {
        for (name, arguments, expected) in cases {
            let result = client.call(name, arguments.clone()).await;
            assert!(is_error(&result), "{name} {arguments}");
            assert_eq!(text(&result), expected, "{name} {arguments}");
        }
        let unknown = client
            .request(
                "tools/call",
                json!({ "name": "auth_login", "arguments": {} }),
            )
            .await;
        assert_eq!(unknown["error"]["code"], -32602);
        assert_eq!(unknown["error"]["message"], r#"unknown tool "auth_login""#);
    })
    .await;
}

#[tokio::test]
async fn returns_actionable_account_errors_and_keeps_serving_requests() {
    let mut fake = Fake::new(&account());
    fake.account_info = handler(|()| Err(Error::Invalid("account not found".to_owned())));
    with_server(&fake, async |client| {
        let result = client.call("list_purchases", json!({})).await;
        assert!(is_error(&result));
        assert_eq!(
            text(&result),
            "list_purchases failed: account unavailable; run 'ipatool auth login' before using MCP: account not found"
        );
        let tools = client.request("tools/list", json!({})).await;
        assert!(tools["result"]["tools"].is_array());
    })
    .await;
}

#[tokio::test]
async fn lists_purchases_with_pagination() {
    let cases = [
        (json!({}), 1, 10, None, ""),
        (
            json!({ "page": 2, "max_results": 5, "platform": "macos" }),
            2,
            5,
            Some(Platform::MacOs),
            "macos",
        ),
    ];
    for (arguments, page, limit, platform, platform_name) in cases {
        let mut fake = Fake::new(&account());
        let inputs = recorder();
        let seen = inputs.clone();
        fake.owned_apps = handler(move |input: OwnedAppsInput| {
            let page = input.page;
            seen.borrow_mut().push(input);
            Ok(OwnedAppsOutput {
                count: 1,
                total_count: 11,
                page,
                results: vec![example_app()],
            })
        });
        with_server(&fake, async |client| {
            let result = client.call("list_purchases", arguments.clone()).await;
            assert!(!is_error(&result));
            let output = &result["structuredContent"];
            assert_eq!(output["totalCount"], 11);
            assert_eq!(output["page"], page);
            assert_eq!(output["max_results"], limit);
            assert_eq!(output["platform"], platform_name);
        })
        .await;
        assert_eq!(
            *inputs.borrow(),
            [OwnedAppsInput {
                account: account(),
                page,
                limit,
                platform,
            }]
        );
    }
}

#[tokio::test]
async fn resolves_bundle_identifiers_and_lists_versions() {
    let mut fake = Fake::new(&account());
    let lookups = recorder();
    let seen = lookups.clone();
    fake.lookup = handler(move |(account, query, platform)| {
        seen.borrow_mut().push((account, query, platform));
        Ok(example_app())
    });
    let inputs = recorder();
    let seen = inputs.clone();
    fake.list_versions = handler(move |(_, app, _)| {
        seen.borrow_mut().push(app);
        Ok(ListVersionsOutput {
            external_version_identifiers: vec!["100".to_owned(), "200".to_owned()],
            latest_external_version_id: "200".to_owned(),
        })
    });
    with_server(&fake, async |client| {
        let arguments =
            json!({ "app_id": 999, "bundle_identifier": "com.example.app", "platform": "ipados" });
        let result = client.call("list_app_versions", arguments).await;
        assert!(!is_error(&result));
        let output = &result["structuredContent"];
        assert_eq!(output["externalVersionIdentifiers"], json!(["100", "200"]));
        assert_eq!(output["app_id"], 123);
        assert_eq!(output["bundle_identifier"], "com.example.app");
        assert_eq!(output["platform"], "ipad");
    })
    .await;
    let lookups = lookups.borrow();
    assert_eq!(lookups.len(), 1);
    assert_eq!(lookups[0].0, account());
    assert!(matches!(&lookups[0].1, LookupQuery::BundleId(id) if id == "com.example.app"));
    assert_eq!(lookups[0].2, Some(Platform::IPad));
    assert_eq!(*inputs.borrow(), [example_app()]);
}

#[tokio::test]
async fn resolves_a_bundle_identifier_and_returns_metadata_for_the_requested_version() {
    let app = App {
        version: "2.0".to_owned(),
        ..example_app()
    };
    let mut fake = Fake::new(&account());
    let lookups = recorder();
    let seen = lookups.clone();
    let found = app.clone();
    fake.lookup = handler(move |(_, query, platform)| {
        seen.borrow_mut().push((query, platform));
        Ok(found.clone())
    });
    let inputs = recorder();
    let seen = inputs.clone();
    fake.get_version_metadata = handler(move |input| {
        seen.borrow_mut().push(input);
        Ok(VersionMetadata {
            display_version: "1.2.3".to_owned(),
            release_date: datetime!(2025-01-02 3:04:05 UTC),
        })
    });
    with_server(&fake, async |client| {
        let arguments = json!({
            "app_id": 999,
            "bundle_identifier": "com.example.app",
            "external_version_id": "200",
            "platform": "ipados",
        });
        let result = client.call("get_version_metadata", arguments).await;
        assert!(!is_error(&result));
        let output = &result["structuredContent"];
        assert_eq!(output["app_id"], 123);
        assert_eq!(output["bundle_identifier"], "com.example.app");
        assert_eq!(output["platform"], "ipad");
        assert_eq!(output["external_version_id"], "200");
        assert_eq!(output["displayVersion"], "1.2.3");
        assert_eq!(output["releaseDate"], "2025-01-02T03:04:05Z");
        assert_eq!(output["success"], true);
        assert_eq!(output["app"]["version"], "2.0");
        assert!(!output.to_string().contains("private-"));
        let text: Value = serde_json::from_str(text(&result)).unwrap();
        assert_eq!(&text, output);
    })
    .await;
    let lookups = lookups.borrow();
    assert!(
        matches!(&lookups[..], [(LookupQuery::BundleId(id), Some(Platform::IPad))] if id == "com.example.app")
    );
    assert_eq!(
        *inputs.borrow(),
        [(account(), app, "200".to_owned(), Some(Platform::IPad))]
    );
}

#[tokio::test]
async fn retrieves_version_metadata_by_app_id_when_catalog_metadata_is_unavailable() {
    for (platform, expected) in [
        ("", None),
        ("ios", Some(Platform::IPhone)),
        ("macos", Some(Platform::MacOs)),
    ] {
        let mut fake = Fake::new(&account());
        let inputs = recorder();
        let seen = inputs.clone();
        fake.get_version_metadata = handler(move |(_, app, version, platform)| {
            seen.borrow_mut().push((app, version, platform));
            Ok(VersionMetadata {
                display_version: "1.2.3".to_owned(),
                release_date: datetime!(0001-01-01 0:00 UTC),
            })
        });
        with_server(&fake, async |client| {
            let arguments =
                json!({ "app_id": 123, "external_version_id": "200", "platform": platform });
            let result = client.call("get_version_metadata", arguments).await;
            assert!(!is_error(&result));
            // Go's zero time.
            assert_eq!(
                result["structuredContent"]["releaseDate"],
                "0001-01-01T00:00:00Z"
            );
        })
        .await;
        let app = App {
            id: 123,
            ..App::default()
        };
        assert_eq!(*inputs.borrow(), [(app, "200".to_owned(), expected)]);
    }
}

#[tokio::test]
async fn returns_bundle_lookup_failures_as_metadata_tool_errors() {
    let fake = Fake::new(&account());
    with_server(&fake, async |client| {
        let arguments =
            json!({ "bundle_identifier": "com.example.app", "external_version_id": "200" });
        let result = client.call("get_version_metadata", arguments).await;
        assert!(is_error(&result));
        assert_eq!(text(&result), "get_version_metadata failed: app not found");
    })
    .await;
}

#[tokio::test]
async fn returns_metadata_failures_as_tool_errors_without_acquiring_a_license() {
    let failures: [fn() -> Error; 2] = [
        || Error::LicenseRequired,
        || Error::Invalid("metadata unavailable".to_owned()),
    ];
    for failure in failures {
        let message = failure().to_string();
        let mut fake = Fake::new(&account());
        fake.get_version_metadata = handler(move |_| Err(failure()));
        with_server(&fake, async |client| {
            let arguments = json!({ "app_id": 123, "external_version_id": "200" });
            let result = client.call("get_version_metadata", arguments).await;
            assert!(is_error(&result));
            assert!(text(&result).contains(&message));
            assert!(!result.to_string().contains("private-"));
        })
        .await;
    }
}

fn refreshed_account() -> Account {
    Account {
        password_token: "refreshed-private-token".to_owned(),
        ..Account::default()
    }
}

#[tokio::test]
async fn refreshes_an_expired_session_when_retrieving_version_metadata() {
    let mut fake = Fake::new(&account());
    let accounts = recorder();
    let seen = accounts.clone();
    fake.get_version_metadata = handler(
        move |(account, ..): (Account, App, String, Option<Platform>)| {
            seen.borrow_mut().push(account);
            if seen.borrow().len() == 1 {
                return Err(Error::PasswordTokenExpired);
            }
            Ok(VersionMetadata {
                display_version: "1.2.3".to_owned(),
                release_date: datetime!(0001-01-01 0:00 UTC),
            })
        },
    );
    let logins = recorder();
    let seen = logins.clone();
    fake.login = handler(move |account: Account| {
        seen.borrow_mut().push((account.email, account.password));
        Ok(refreshed_account())
    });
    with_server(&fake, async |client| {
        let arguments = json!({ "app_id": 123, "external_version_id": "200" });
        let result = client.call("get_version_metadata", arguments).await;
        assert!(!is_error(&result));
        assert_eq!(result["structuredContent"]["displayVersion"], "1.2.3");
    })
    .await;
    assert_eq!(
        *logins.borrow(),
        [("private-email".to_owned(), "private-password".to_owned())]
    );
    assert_eq!(*accounts.borrow(), [account(), refreshed_account()]);
}

#[tokio::test]
async fn purchases_a_free_license() {
    let outcomes: [(fn() -> AppResult<()>, bool); 2] = [
        (|| Ok(()), false),
        (|| Err(Error::LicenseAlreadyExists), true),
    ];
    for (outcome, already_owned) in outcomes {
        let mut fake = Fake::new(&account());
        let inputs = recorder();
        let seen = inputs.clone();
        fake.purchase = handler(move |input| {
            seen.borrow_mut().push(input);
            outcome()
        });
        with_server(&fake, async |client| {
            let arguments = json!({ "app_id": 123, "platform": "visionos" });
            let result = client.call("purchase_app", arguments).await;
            assert!(!is_error(&result));
            assert_eq!(result["structuredContent"]["alreadyOwned"], already_owned);
        })
        .await;
        let app = App {
            id: 123,
            ..App::default()
        };
        assert_eq!(
            *inputs.borrow(),
            [(account(), app, Some(Platform::VisionOs))]
        );
    }
}

#[tokio::test]
async fn downloads_a_selected_version_and_finalizes_the_package_without_a_progress_bar() {
    let mut fake = Fake::new(&account());
    let inputs = recorder();
    let seen = inputs.clone();
    fake.download = handler(move |input: DownloadInput| {
        seen.borrow_mut().push(input);
        Ok(download_output("/tmp/example.ipa"))
    });
    let packages = recorder();
    let seen = packages.clone();
    fake.replicate_sinf = handler(move |package| {
        seen.borrow_mut().push(package);
        Ok(())
    });
    with_server(&fake, async |client| {
        let arguments = json!({
            "app_id": 123,
            "output": "/tmp/example.ipa",
            "external_version_id": "200",
            "platform": "ios",
        });
        let result = client.call("download_app", arguments).await;
        assert!(!is_error(&result));
        let output = &result["structuredContent"];
        assert_eq!(output["output"], "/tmp/example.ipa");
        assert_eq!(output["external_version_id"], "200");
        assert_eq!(output["purchase"], false);
    })
    .await;
    let inputs = inputs.borrow();
    let [input] = &inputs[..] else {
        panic!("one download expected")
    };
    assert_eq!(input.app.id, 123);
    assert_eq!(input.output_path, "/tmp/example.ipa");
    assert_eq!(input.external_version_id, "200");
    assert_eq!(input.platform, Some(Platform::IPhone));
    assert!(input.progress.is_none());
    assert_eq!(*packages.borrow(), [PathBuf::from("/tmp/example.ipa")]);
}

#[tokio::test]
async fn downloads_delisted_macos_apps_by_id_without_attempting_ipa_finalization() {
    let mut fake = Fake::new(&account());
    let inputs = recorder();
    let seen = inputs.clone();
    fake.download = handler(move |input: DownloadInput| {
        seen.borrow_mut().push(input.app.id);
        Ok(download_output("/tmp/example.pkg"))
    });
    with_server(&fake, async |client| {
        let arguments =
            json!({ "app_id": 123, "bundle_identifier": "com.example.app", "platform": "macos" });
        assert!(!is_error(&client.call("download_app", arguments).await));
    })
    .await;
    assert_eq!(*inputs.borrow(), [123]);
}

#[tokio::test]
async fn only_acquires_a_license_during_download_when_requested() {
    for purchase in [true, false] {
        let mut fake = Fake::new(&account());
        let downloads = Rc::new(Cell::new(0));
        let count = downloads.clone();
        fake.download = handler(move |_| {
            count.set(count.get() + 1);
            if count.get() == 1 {
                return Err(Error::LicenseRequired);
            }
            Ok(download_output("/tmp/example.pkg"))
        });
        let purchases = Rc::new(Cell::new(0));
        let count = purchases.clone();
        fake.purchase = handler(move |_| {
            count.set(count.get() + 1);
            Ok(())
        });
        with_server(&fake, async |client| {
            let arguments = json!({ "app_id": 123, "platform": "macos", "purchase": purchase });
            let result = client.call("download_app", arguments).await;
            assert_eq!(is_error(&result), !purchase);
            if purchase {
                assert_eq!(result["structuredContent"]["purchased"], true);
            }
        })
        .await;
        let expected = if purchase { (1, 2) } else { (0, 1) };
        assert_eq!((purchases.get(), downloads.get()), expected);
    }
}

#[tokio::test]
async fn uses_a_purchase_corrected_storefront_for_mcp_downloads() {
    for refresh in [false, true] {
        let saved = Rc::new(RefCell::new(Account {
            directory_services_id: "123456".to_owned(),
            store_front: "143441-1,34".to_owned(),
            ..account()
        }));
        let mut fake = Fake::new(&account());
        let current = saved.clone();
        fake.account_info = handler(move |()| Ok(current.borrow().clone()));
        let expected_purchases = if refresh { 2 } else { 1 };
        let purchases = Rc::new(Cell::new(0));
        let downloads = recorder();
        let (seen, count) = (downloads.clone(), purchases.clone());
        fake.download = handler(move |input: DownloadInput| {
            seen.borrow_mut().push(input.account);
            if count.get() < expected_purchases {
                return Err(Error::LicenseRequired);
            }
            Ok(download_output("/tmp/example.pkg"))
        });
        let (current, count) = (saved.clone(), purchases.clone());
        fake.purchase = handler(move |_| {
            count.set(count.get() + 1);
            current.borrow_mut().store_front = "143462-1,34".to_owned();
            if refresh && count.get() == 1 {
                return Err(Error::PasswordTokenExpired);
            }
            Ok(())
        });
        let current = saved.clone();
        fake.login = handler(move |_| {
            current.borrow_mut().password_token = "new-token".to_owned();
            Ok(current.borrow().clone())
        });
        with_server(&fake, async |client| {
            let arguments = json!({ "app_id": 123, "platform": "macos", "purchase": true });
            let result = client.call("download_app", arguments).await;
            assert!(!is_error(&result), "{result}");
            assert_eq!(result["structuredContent"]["purchased"], true);
        })
        .await;
        assert_eq!(purchases.get(), expected_purchases);
        let downloads = downloads.borrow();
        assert_eq!(downloads[0].store_front, "143441-1,34");
        let last = downloads.last().unwrap();
        assert_eq!(last.store_front, "143462-1,34");
        if refresh {
            assert_eq!(last.password_token, "new-token");
        }
    }
}

#[tokio::test]
async fn propagates_errors_after_purchasing_during_an_mcp_download() {
    for account_read_fails in [true, false] {
        let mut fake = Fake::new(&account());
        let fails = Rc::new(Cell::new(false));
        let failing = fails.clone();
        fake.account_info = handler(move |()| {
            if failing.get() {
                return Err(Error::Invalid("fixture post-purchase failure".to_owned()));
            }
            Ok(account())
        });
        let downloads = Rc::new(Cell::new(0));
        fake.download = handler(move |_| {
            downloads.set(downloads.get() + 1);
            if downloads.get() == 1 {
                return Err(Error::LicenseRequired);
            }
            Err(Error::Invalid("fixture post-purchase failure".to_owned()))
        });
        fake.purchase = handler(move |_| {
            fails.set(account_read_fails);
            Ok(())
        });
        with_server(&fake, async |client| {
            let arguments = json!({ "app_id": 123, "platform": "macos", "purchase": true });
            let result = client.call("download_app", arguments).await;
            assert!(is_error(&result));
            assert!(text(&result).contains("fixture post-purchase failure"));
        })
        .await;
    }
}

#[tokio::test]
async fn refreshes_an_expired_session_using_saved_credentials() {
    let mut fake = Fake::new(&account());
    let accounts = recorder();
    let seen = accounts.clone();
    fake.purchase = handler(move |(account, ..): (Account, App, Option<Platform>)| {
        seen.borrow_mut().push(account);
        if seen.borrow().len() == 1 {
            return Err(Error::PasswordTokenExpired);
        }
        Ok(())
    });
    let logins = recorder();
    let seen = logins.clone();
    fake.login = handler(move |account: Account| {
        seen.borrow_mut().push((account.email, account.password));
        Ok(refreshed_account())
    });
    with_server(&fake, async |client| {
        let result = client.call("purchase_app", json!({ "app_id": 123 })).await;
        assert!(!is_error(&result));
    })
    .await;
    assert_eq!(
        *logins.borrow(),
        [("private-email".to_owned(), "private-password".to_owned())]
    );
    assert_eq!(*accounts.borrow(), [account(), refreshed_account()]);
}

#[tokio::test]
async fn reports_two_factor_requirements_as_tool_errors_without_requesting_credentials() {
    let mut fake = Fake::new(&account());
    fake.purchase = handler(|_| Err(Error::PasswordTokenExpired));
    fake.login = handler(|_| Err(Error::AuthCodeRequired));
    with_server(&fake, async |client| {
        let result = client.call("purchase_app", json!({ "app_id": 123 })).await;
        assert!(is_error(&result));
        assert_eq!(
            text(&result),
            "purchase_app failed: session refresh failed; run 'ipatool auth login' outside MCP: auth code is required"
        );
    })
    .await;
}

#[tokio::test]
async fn reports_failures_after_a_session_refresh() {
    let mut fake = Fake::new(&account());
    fake.purchase = handler(|_| Err(Error::PasswordTokenExpired));
    fake.login = handler(|_| Ok(refreshed_account()));
    with_server(&fake, async |client| {
        let result = client.call("purchase_app", json!({ "app_id": 123 })).await;
        assert_eq!(
            text(&result),
            "purchase_app failed: operation failed after session refresh: password token is expired"
        );
    })
    .await;
}

#[tokio::test]
async fn does_not_expose_app_store_error_metadata() {
    let mut fake = Fake::new(&account());
    fake.search = handler(|_| Err(Error::Invalid("request failed".to_owned())));
    with_server(&fake, async |client| {
        let result = client
            .call("search_apps", json!({ "term": "example" }))
            .await;
        assert!(is_error(&result));
        let data = result.to_string();
        assert!(data.contains("request failed"));
        assert!(!data.contains("private-"));
    })
    .await;
}

#[tokio::test]
async fn reports_package_finalization_failures_as_tool_errors() {
    let mut fake = Fake::new(&account());
    fake.download = handler(|_| Ok(download_output("/tmp/example.ipa")));
    fake.replicate_sinf = handler(|_| Err(Error::Invalid("finalization failed".to_owned())));
    with_server(&fake, async |client| {
        let result = client.call("download_app", json!({ "app_id": 123 })).await;
        assert!(is_error(&result));
        assert!(text(&result).contains("finalization failed"));
    })
    .await;
}

/// The scripted calls of the metadata tests, for every tool.
fn script_every_tool(fake: &mut Fake, apps: &[App]) {
    let found = apps.to_vec();
    fake.search = handler(move |_| Ok(found.clone()));
    let found = apps.to_vec();
    fake.owned_apps = handler(move |_| {
        Ok(OwnedAppsOutput {
            count: 1,
            total_count: 1,
            page: 1,
            results: found.clone(),
        })
    });
    fake.list_versions = handler(|_| {
        Ok(ListVersionsOutput {
            external_version_identifiers: vec!["200".to_owned()],
            latest_external_version_id: String::new(),
        })
    });
    fake.get_version_metadata = handler(|_| {
        Ok(VersionMetadata {
            display_version: String::new(),
            release_date: datetime!(0001-01-01 0:00 UTC),
        })
    });
    fake.purchase = handler(|_| Ok(()));
    fake.download = handler(|_| Ok(download_output("/tmp/example.ipa")));
    fake.replicate_sinf = handler(|_| Ok(()));
}

#[tokio::test]
async fn returns_every_parameter_and_app_field_even_when_metadata_is_unavailable() {
    let cases = [
        (
            "search_apps",
            json!({ "term": "example" }),
            &["term", "limit", "platform", "count", "apps"][..],
        ),
        (
            "list_purchases",
            json!({}),
            &[
                "page",
                "max_results",
                "platform",
                "count",
                "totalCount",
                "apps",
            ][..],
        ),
        (
            "list_app_versions",
            json!({ "app_id": 123 }),
            &[
                "app_id",
                "bundle_identifier",
                "platform",
                "bundleID",
                "app",
                "externalVersionIdentifiers",
                "latestExternalVersionID",
                "success",
            ][..],
        ),
        (
            "get_version_metadata",
            json!({ "app_id": 123, "external_version_id": "200" }),
            &[
                "app_id",
                "bundle_identifier",
                "platform",
                "app",
                "external_version_id",
                "displayVersion",
                "releaseDate",
                "success",
            ][..],
        ),
        (
            "purchase_app",
            json!({ "app_id": 123 }),
            &[
                "app_id",
                "bundle_identifier",
                "platform",
                "app",
                "alreadyOwned",
                "success",
            ][..],
        ),
        (
            "download_app",
            json!({ "app_id": 123 }),
            &[
                "app_id",
                "bundle_identifier",
                "platform",
                "app",
                "output",
                "external_version_id",
                "purchase",
                "purchased",
                "success",
            ][..],
        ),
    ];
    for (name, arguments, keys) in cases {
        let mut fake = Fake::new(&account());
        let partial = App {
            id: 123,
            ..App::default()
        };
        script_every_tool(&mut fake, &[partial]);
        with_server(&fake, async |client| {
            let result = client.call(name, arguments.clone()).await;
            assert!(!is_error(&result), "{name}");
            let output = &result["structuredContent"];
            for key in keys {
                assert!(output.get(key).is_some(), "{name} {key}");
            }
            let metadata = output.get("app").unwrap_or(&output["apps"][0]);
            assert_eq!(
                *metadata,
                json!({
                    "trackId": 123, "bundleId": "", "trackName": "", "version": "",
                    "price": 0, "purchaseDate": null, "platforms": [],
                }),
                "{name}"
            );
            let text: Value = serde_json::from_str(text(&result)).unwrap();
            assert_eq!(&text, output);
        })
        .await;
    }
}

#[tokio::test]
async fn fills_missing_bundle_identifiers_in_app_lists_without_losing_purchase_metadata() {
    for (name, arguments) in [
        ("search_apps", json!({ "term": "example" })),
        ("list_purchases", json!({})),
    ] {
        let mut fake = Fake::new(&account());
        let partial = App {
            id: 123,
            name: "Original name".to_owned(),
            purchase_date: Some(datetime!(2025-01-02 3:04:05 UTC)),
            platforms: vec![Platform::MacOs],
            ..App::default()
        };
        script_every_tool(&mut fake, &[partial]);
        let lookups = recorder();
        let seen = lookups.clone();
        fake.lookup = handler(move |(account, query, platform)| {
            seen.borrow_mut().push((account, query, platform));
            Ok(App {
                id: 123,
                bundle_id: "com.example.app".to_owned(),
                name: "Catalog name".to_owned(),
                version: "2.0".to_owned(),
                price: 4.99,
                ..App::default()
            })
        });
        with_server(&fake, async |client| {
            let result = client.call(name, arguments.clone()).await;
            assert!(!is_error(&result));
            assert_eq!(
                result["structuredContent"]["apps"][0],
                json!({
                    "trackId": 123, "bundleId": "com.example.app", "trackName": "Original name",
                    "version": "2.0", "price": 4.99, "purchaseDate": "2025-01-02T03:04:05Z",
                    "platforms": ["macos"],
                })
            );
        })
        .await;
        let lookups = lookups.borrow();
        assert_eq!(lookups.len(), 1, "{name}");
        assert_eq!(lookups[0].0, account());
        assert!(matches!(lookups[0].1, LookupQuery::AppId(123)));
        assert_eq!(lookups[0].2, Some(Platform::MacOs));
    }
}

#[tokio::test]
async fn resolves_bundle_identifiers_for_numeric_app_ids() {
    for name in ["purchase_app", "download_app", "list_app_versions"] {
        let mut fake = Fake::new(&account());
        script_every_tool(&mut fake, &[]);
        fake.list_versions = handler(|_| Ok(ListVersionsOutput::default()));
        let lookups = recorder();
        let seen = lookups.clone();
        fake.lookup = handler(move |(_, query, _)| {
            seen.borrow_mut().push(query);
            Ok(example_app())
        });
        with_server(&fake, async |client| {
            let result = client.call(name, json!({ "app_id": 123 })).await;
            assert!(!is_error(&result), "{name}");
            let output = &result["structuredContent"];
            assert_eq!(output["app_id"], 123);
            assert_eq!(output["bundle_identifier"], "com.example.app");
            assert_eq!(output["app"]["bundleId"], "com.example.app");
            if name == "list_app_versions" {
                assert_eq!(output["bundleID"], "com.example.app");
                assert_eq!(output["externalVersionIdentifiers"], json!([]));
            }
        })
        .await;
        assert!(matches!(&lookups.borrow()[..], [LookupQuery::AppId(123)]));
    }
}

#[tokio::test]
async fn escapes_result_text_as_go_json() {
    let mut fake = Fake::new(&account());
    fake.search = handler(|_| {
        Ok(vec![App {
            name: "Tom & Jerry <3".to_owned(),
            price: 0.99,
            platforms: vec![Platform::IPhone],
            ..example_app()
        }])
    });
    fake.lookup = handler(|_| Ok(App::default()));
    with_server(&fake, async |client| {
        let result = client.call("search_apps", json!({ "term": "tom" })).await;
        assert_eq!(
            text(&result),
            r#"{"apps":[{"bundleId":"com.example.app","platforms":["iphone"],"price":0.99,"purchaseDate":null,"trackId":123,"trackName":"Tom \u0026 Jerry \u003c3","version":""}],"count":1,"limit":5,"platform":"","term":"tom"}"#
        );
        assert!(result.get("isError").is_none());
    })
    .await;
}

// Go `cmd/mcp_app_test.go`.

#[tokio::test]
async fn preserves_source_data_when_catalog_metadata_cannot_be_used() {
    let original = App {
        id: 123,
        name: "Original name".to_owned(),
        price: 2.99,
        platforms: vec![Platform::Unknown],
        ..App::default()
    };
    let catalogs: [fn() -> AppResult<App>; 4] = [
        || Err(Error::AppNotFound),
        || Err(Error::Invalid("network failure".to_owned())),
        || {
            Ok(App {
                id: 999,
                bundle_id: "wrong.bundle".to_owned(),
                ..App::default()
            })
        },
        || {
            Ok(App {
                id: 123,
                ..App::default()
            })
        },
    ];
    for catalog in catalogs {
        let mut fake = Fake::new(&Account::default());
        fake.lookup = handler(move |_| catalog());
        let app = complete_app(&fake, &Account::default(), original.clone(), None).await;
        assert_eq!(app, original);
    }
}

#[tokio::test]
async fn avoids_catalog_lookups_when_app_metadata_is_already_complete() {
    let mut fake = Fake::new(&Account::default());
    fake.lookup = unexpected("lookup");
    let app = App {
        id: 123,
        bundle_id: "com.example.app".to_owned(),
        name: "Example".to_owned(),
        version: "1.0".to_owned(),
        platforms: vec![Platform::IPhone],
        ..App::default()
    };
    assert_eq!(
        complete_app(&fake, &Account::default(), app.clone(), None).await,
        app
    );
}
