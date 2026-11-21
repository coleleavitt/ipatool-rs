//! Port of Go `cmd/mcp.go`: `ipatool mcp` serves the App Store tools over the Model
//! Context Protocol on stdio, without the authentication commands.

mod app;
mod schema;
mod tools;

#[cfg(test)]
mod tests;

use std::borrow::Cow;

#[expect(deprecated, reason = "Go's SDK still answers logging/setLevel")]
use rmcp::model::SetLevelRequestParams;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, InitializeResult, ListPromptsResult, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer, ServerInitializeError};
use rmcp::transport::IntoTransport;
use rmcp::{ServerHandler, ServiceExt as _};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use crate::commerce::Store;

const INSTRUCTIONS: &str = "Use the existing ipatool account. If authentication is required, ask the user to run ipatool auth login outside MCP. Purchases support free apps only.";

/// The protocol versions of Go's SDK v1.8.0, which answers an unknown version with
/// its latest.
const PROTOCOL_VERSIONS: [ProtocolVersion; 4] = [
    ProtocolVersion::V_2025_11_25,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_03_26,
    ProtocolVersion::V_2024_11_05,
];

/// One validated tool call.
enum Call {
    Search(tools::SearchInput),
    Download(tools::DownloadAppInput),
    ListVersions(tools::AppInput),
    GetVersionMetadata(tools::VersionMetadataInput),
    ListPurchases(tools::PurchasesInput),
    Purchase(tools::AppInput),
}

struct Job {
    name: String,
    call: Call,
    reply: oneshot::Sender<CallToolResult>,
}

/// The protocol side. Tool calls go to [`run_calls`], which owns the App Store
/// session and runs one call at a time, as Go's gate does: the client shares account
/// state and caches across calls.
struct Server {
    tools: Vec<Tool>,
    jobs: mpsc::UnboundedSender<Job>,
}

/// Go's `addMCPTool`, with `jsonschema.For` schemas of the input and output types.
fn tool<In: JsonSchema, Out: JsonSchema>(
    name: &'static str,
    description: &'static str,
    read_only: bool,
) -> Tool {
    Tool::new(name, description, schema::input_schema::<In>())
        .with_raw_output_schema(schema::output_schema::<Out>().into())
        .with_annotations(
            ToolAnnotations::new()
                .read_only(read_only)
                .idempotent(false),
        )
}

/// Go's `newMCPServer` tools, in the name order Go's SDK lists them.
fn tool_list() -> Vec<Tool> {
    let mut tools = vec![
        tool::<tools::SearchInput, tools::SearchOutput>(
            "search_apps",
            "Search for apps on the App Store.",
            true,
        ),
        tool::<tools::DownloadAppInput, tools::DownloadAppOutput>(
            "download_app",
            "Download an app package to the local filesystem. Optionally acquire a free license if purchase is true.",
            false,
        ),
        tool::<tools::AppInput, tools::VersionsOutput>(
            "list_app_versions",
            "List available external version identifiers for an app.",
            true,
        ),
        tool::<tools::VersionMetadataInput, tools::VersionMetadataOutput>(
            "get_version_metadata",
            "Retrieve the display version and release date for a specific external version identifier of an app.",
            true,
        ),
        tool::<tools::PurchasesInput, tools::PurchasesOutput>(
            "list_purchases",
            "List apps owned by the authenticated account, with pagination.",
            true,
        ),
        tool::<tools::AppInput, tools::PurchaseOutput>(
            "purchase_app",
            "Obtain a license for a free app. Paid apps are not supported.",
            false,
        ),
    ];
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    tools
}

impl Server {
    /// The tool's name and its validated call; an unknown tool is a protocol error, as
    /// in Go's SDK.
    fn parse(
        &self,
        request: CallToolRequestParams,
    ) -> Result<(String, Result<Call, String>), ErrorData> {
        let Some(tool) = self.tools.iter().find(|tool| tool.name == request.name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool {:?}", request.name),
                None,
            ));
        };
        let (schema, arguments) = (&tool.input_schema, request.arguments);
        let call = match tool.name.as_ref() {
            "search_apps" => schema::parse_arguments(schema, arguments).map(Call::Search),
            "download_app" => schema::parse_arguments(schema, arguments).map(Call::Download),
            "list_app_versions" => {
                schema::parse_arguments(schema, arguments).map(Call::ListVersions)
            }
            "get_version_metadata" => {
                schema::parse_arguments(schema, arguments).map(Call::GetVersionMetadata)
            }
            "list_purchases" => schema::parse_arguments(schema, arguments).map(Call::ListPurchases),
            _ => schema::parse_arguments(schema, arguments).map(Call::Purchase),
        };
        Ok((tool.name.to_string(), call))
    }
}

fn error_result(text: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text)])
}

impl ServerHandler for Server {
    #[expect(deprecated, reason = "Go's SDK still advertises logging")]
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_logging()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new("ipatool", env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&PROTOCOL_VERSIONS)
    }

    #[expect(deprecated, reason = "Go's SDK still answers logging/setLevel")]
    fn set_level(
        &self,
        _: SetLevelRequestParams,
        _: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), ErrorData>> + Send + '_ {
        std::future::ready(Ok(()))
    }

    // Go's SDK answers the prompt and resource lists without advertising them, and
    // sends the two freshness fields with every list.
    fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListPromptsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListPromptsResult::default()
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Public)))
    }

    fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListResourcesResult::default()
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Public)))
    }

    fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListResourceTemplatesResult::default()
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Public)))
    }

    fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(self.tools.clone())
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Public)))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let (name, call) = match self.parse(request)? {
            (name, Ok(call)) => (name, call),
            (_, Err(error)) => return Ok(error_result(error).into()),
        };
        let (reply, result) = oneshot::channel();
        if self.jobs.send(Job { name, call, reply }).is_err() {
            return Err(ErrorData::internal_error("the tool runner stopped", None));
        }
        tokio::select! {
            result = result => {
                result.map(Into::into).map_err(|_| ErrorData::internal_error("the tool runner stopped", None))
            }
            () = context.ct.cancelled() => Ok(error_result(CANCELED.to_owned()).into()),
        }
    }
}

const CANCELED: &str = "tool call canceled: context canceled";

/// Runs the queued tool calls one at a time until the server stops. A call whose
/// request was canceled is not started, or is dropped mid-way, like Go's context.
async fn run_calls(store: &impl Store, mut jobs: mpsc::UnboundedReceiver<Job>) {
    while let Some(Job {
        name,
        call,
        mut reply,
    }) = jobs.recv().await
    {
        if reply.is_closed() {
            continue;
        }
        let result = tokio::select! {
            result = run_call(store, call) => result,
            () = reply.closed() => continue,
        };
        let result = match result {
            Ok(value) => {
                let value = schema::go_value(value);
                let mut result =
                    CallToolResult::success(vec![ContentBlock::text(schema::go_json_text(&value))]);
                result.structured_content = Some(value);
                // Go omits a false `isError`.
                result.is_error = None;
                result
            }
            Err(error) => error_result(format!("{name} failed: {error}")),
        };
        let _ = reply.send(result);
    }
}

async fn run_call(store: &impl Store, call: Call) -> Result<serde_json::Value, String> {
    match call {
        Call::Search(input) => to_value(tools::search(store, input).await),
        Call::Download(input) => to_value(tools::download(store, input).await),
        Call::ListVersions(input) => to_value(tools::list_versions(store, input).await),
        Call::GetVersionMetadata(input) => {
            to_value(tools::get_version_metadata(store, input).await)
        }
        Call::ListPurchases(input) => to_value(tools::list_purchases(store, input).await),
        Call::Purchase(input) => to_value(tools::purchase(store, input).await),
    }
}

fn to_value(output: Result<impl Serialize, String>) -> Result<serde_json::Value, String> {
    serde_json::to_value(output?).map_err(|error| error.to_string())
}

/// Go's `mcpCmd` run: serves until the client closes the connection. An interrupt
/// ends it with Go's "context canceled" error.
pub(crate) async fn serve<T, E, A>(store: &impl Store, transport: T) -> Result<(), String>
where
    T: IntoTransport<RoleServer, E, A> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let (jobs, queue) = mpsc::unbounded_channel();
    let server = Server {
        tools: tool_list(),
        jobs,
    };
    let session = async {
        let running = match server.serve(transport).await {
            Ok(running) => running,
            // Go's SDK ends quietly when the client goes away before initializing.
            Err(ServerInitializeError::ConnectionClosed(_)) => return Ok(()),
            Err(error) => return Err(format!("MCP server failed: {error}")),
        };
        running
            .waiting()
            .await
            .map(drop)
            .map_err(|error| format!("MCP server failed: {error}"))
    };
    let serving = async {
        let (result, ()) = tokio::join!(session, run_calls(store, queue));
        result
    };
    tokio::select! {
        result = serving => result,
        _ = tokio::signal::ctrl_c() => Err("MCP server failed: context canceled".to_owned()),
    }
}
