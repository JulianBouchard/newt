use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use newt_core::message::ToolCall;
use newt_core::prompt::ToolSpec;
use newt_core::tools::{CallContext, Retention, ToolDef, ToolHost, ToolOutput};
use newt_core::workspace::{Page, PageKey};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientConfig, ContentBlock, JsonObject,
    RequestMetaObject, Tool,
};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceError, ServiceExt};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub const META_RETENTION: &str = "newt/retention";
pub const META_INVALIDATES: &str = "newt/invalidates";
pub const META_IDEMPOTENT: &str = "newt/idempotent";
pub const META_SESSION: &str = "newt/session";
pub const META_IDEMPOTENCY_KEY: &str = "newt/idempotency_key";
pub const META_SKILLS_LOADED: &str = "newt/skills_loaded";
pub const META_PAGES_READ: &str = "newt/pages_read";
pub const META_SUMMARY: &str = "newt/summary";
pub const META_INVALIDATE: &str = "newt/invalidate";
pub const META_PAGES: &str = "newt/pages";

const CALL_TIMEOUT: Duration = Duration::from_secs(90);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const OUTCOME_UNKNOWN: &str = "it may or may not have taken effect. Do not repeat it: check the workspace first, and tell the person if you cannot confirm it.";
const BAD_META: &str = "(newt could not read this tool's workspace metadata, so every page will be refetched before its next read)";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuth {
    #[default]
    None,
    AgentToken,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub auth: McpAuth,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpServer")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("auth", &self.auth)
            .finish()
    }
}

type Client = RunningService<RoleClient, ClientConfig>;

struct Remote {
    client: Arc<Client>,
    server: String,
}

#[derive(Clone)]
pub struct McpHost {
    tools: Arc<HashMap<String, Remote>>,
    broken: Arc<AtomicBool>,
    session: String,
}

impl std::fmt::Debug for McpHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpHost")
            .field("tools", &self.tools.len())
            .field("broken", &self.broken)
            .field("session", &self.session)
            .finish()
    }
}

pub struct Toolset {
    pub tools: Vec<ToolDef>,
    remotes: Arc<HashMap<String, Remote>>,
    broken: Arc<AtomicBool>,
}

impl std::fmt::Debug for Toolset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Toolset")
            .field("tools", &self.tools.len())
            .finish_non_exhaustive()
    }
}

impl Toolset {
    pub async fn connect(servers: &[McpServer], agent_token: &str) -> Result<Self, String> {
        tokio::time::timeout(CONNECT_TIMEOUT, Self::connect_all(servers, agent_token))
            .await
            .map_err(|_| {
                format!(
                    "MCP servers did not connect and list their tools within {} seconds",
                    CONNECT_TIMEOUT.as_secs()
                )
            })?
    }

    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }

    async fn connect_all(servers: &[McpServer], agent_token: &str) -> Result<Self, String> {
        let mut tools = Vec::new();
        let mut remotes = HashMap::new();
        for server in servers {
            let client = Arc::new(connect(server, agent_token).await?);
            let listed = client
                .list_all_tools()
                .await
                .map_err(|error| format!("MCP server `{}`: listing tools: {error}", server.name))?;
            for tool in listed {
                let name = tool.name.to_string();
                let def = tool_def(tool).map_err(|error| {
                    format!("MCP server `{}`, tool `{name}`: {error}", server.name)
                })?;
                let remote = Remote {
                    client: Arc::clone(&client),
                    server: server.name.clone(),
                };
                if remotes.insert(name.clone(), remote).is_some() {
                    return Err(format!("two MCP servers both offer a tool named `{name}`"));
                }
                tools.push(def);
            }
            tracing::info!(server = %server.name, tools = tools.len(), "MCP server connected");
        }
        Ok(Self {
            tools,
            remotes: Arc::new(remotes),
            broken: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn host(&self, session: &str) -> McpHost {
        McpHost {
            tools: Arc::clone(&self.remotes),
            broken: Arc::clone(&self.broken),
            session: session.to_owned(),
        }
    }
}

async fn connect(server: &McpServer, agent_token: &str) -> Result<Client, String> {
    if !(server.url.starts_with("https://") || server.url.starts_with("http://")) {
        return Err(format!(
            "MCP server `{}`: url must start with http:// or https://",
            server.name
        ));
    }
    let mut headers = HashMap::new();
    for (name, value) in &server.headers {
        let header_name = name
            .parse()
            .map_err(|_| format!("MCP server `{}`: invalid header name `{name}`", server.name))?;
        let header_value = value.parse().map_err(|_| {
            format!(
                "MCP server `{}`: invalid value for header `{name}`",
                server.name
            )
        })?;
        headers.insert(header_name, header_value);
    }
    let mut config =
        StreamableHttpClientTransportConfig::with_uri(server.url.as_str()).custom_headers(headers);
    if server.auth == McpAuth::AgentToken {
        config = config.auth_header(agent_token);
    }
    ClientConfig::default()
        .serve(StreamableHttpClientTransport::from_config(config))
        .await
        .map_err(|error| format!("MCP server `{}`: connecting: {error}", server.name))
}

fn meta_field<T: DeserializeOwned>(
    meta: Option<&JsonObject>,
    key: &str,
) -> Result<Option<T>, String> {
    meta.and_then(|meta| meta.get(key))
        .map(|value| {
            T::deserialize(value).map_err(|error| format!("`_meta.{key}` is invalid: {error}"))
        })
        .transpose()
}

fn tool_def(tool: Tool) -> Result<ToolDef, String> {
    let meta = tool.meta.as_deref();
    let retention: Retention = meta_field(meta, META_RETENTION)?.unwrap_or(Retention::Durable);
    let invalidates: Vec<PageKey> = meta_field(meta, META_INVALIDATES)?.unwrap_or_default();
    let idempotent: bool = meta_field(meta, META_IDEMPOTENT)?.unwrap_or(false);
    if retention == Retention::Outward && !idempotent {
        return Err(format!(
            "outward tools must set `_meta.{META_IDEMPOTENT}: true` and honour `_meta.{META_IDEMPOTENCY_KEY}`, or a crash could send twice"
        ));
    }
    let mut def = ToolDef::new(
        ToolSpec {
            name: tool.name.to_string(),
            description: tool
                .description
                .map(|text| text.to_string())
                .unwrap_or_default(),
            input_schema: Value::Object((*tool.input_schema).clone()),
        },
        retention,
    );
    def.invalidates = invalidates;
    Ok(def)
}

impl McpHost {
    async fn run(&self, call: &ToolCall, context: &CallContext) -> ToolOutput {
        let Some(remote) = self.tools.get(&call.name) else {
            return ToolOutput::error(format!("unknown tool `{}`", call.name));
        };
        let Value::Object(arguments) = call.arguments.clone() else {
            return ToolOutput::error(format!("arguments for {} must be a JSON object", call.name));
        };
        let mut meta = RequestMetaObject::new();
        meta.insert(META_SESSION.to_owned(), Value::from(self.session.as_str()));
        meta.insert(
            META_IDEMPOTENCY_KEY.to_owned(),
            Value::from(context.idempotency_key.as_str()),
        );
        meta.insert(
            META_SKILLS_LOADED.to_owned(),
            serde_json::json!(context.skills_loaded),
        );
        meta.insert(
            META_PAGES_READ.to_owned(),
            serde_json::json!(context.pages_read),
        );
        let mut params = CallToolRequestParams::new(call.name.clone()).with_arguments(arguments);
        params.meta = Some(meta);
        match tokio::time::timeout(CALL_TIMEOUT, remote.client.call_tool(params)).await {
            Ok(Ok(result)) => tool_output(result),
            Ok(Err(ServiceError::McpError(error))) => {
                tracing::warn!(server = %remote.server, tool = %call.name, %error, "MCP call returned an error");
                ToolOutput::error(format!("The tool call failed: {}", error.message))
            }
            Ok(Err(error)) => {
                if matches!(
                    error,
                    ServiceError::TransportSend(_) | ServiceError::TransportClosed
                ) {
                    self.broken.store(true, Ordering::Relaxed);
                }
                tracing::warn!(server = %remote.server, tool = %call.name, %error, "MCP call lost its connection");
                ToolOutput::error(format!(
                    "The connection to the tool broke during the call, so {OUTCOME_UNKNOWN}"
                ))
            }
            Err(_) => ToolOutput::error(format!(
                "The tool call did not answer within {} seconds, so {OUTCOME_UNKNOWN}",
                CALL_TIMEOUT.as_secs()
            )),
        }
    }
}

impl ToolHost for McpHost {
    fn call(
        &self,
        call: &ToolCall,
        context: &CallContext,
    ) -> impl Future<Output = ToolOutput> + Send {
        self.run(call, context)
    }
}

fn tool_output(result: CallToolResult) -> ToolOutput {
    let mut content: Vec<String> = result
        .content
        .into_iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text,
            ContentBlock::Image(_) => "[image]".to_owned(),
            ContentBlock::Audio(_) => "[audio]".to_owned(),
            _ => "[resource]".to_owned(),
        })
        .collect();
    if content.is_empty()
        && let Some(structured) = &result.structured_content
    {
        content.push(structured.to_string());
    }
    let mut output = ToolOutput {
        content: content.join("\n"),
        is_error: result.is_error == Some(true),
        ..ToolOutput::default()
    };
    let meta = result.meta.as_deref();
    let summary = meta_field::<String>(meta, META_SUMMARY);
    let invalidate = meta_field::<Vec<PageKey>>(meta, META_INVALIDATE);
    let pages = meta_field::<Vec<Page>>(meta, META_PAGES).and_then(|pages| {
        if let Some(pages) = &pages {
            crate::context::check_pages(pages)?;
        }
        Ok(pages)
    });
    match (summary, invalidate, pages) {
        (Ok(summary), Ok(invalidate), Ok(pages)) => {
            output.summary = summary;
            output.invalidate = invalidate.unwrap_or_default();
            output.pages = pages.unwrap_or_default();
        }
        (summary, invalidate, pages) => {
            let problems: Vec<String> = [summary.err(), invalidate.err(), pages.err()]
                .into_iter()
                .flatten()
                .collect();
            tracing::error!(problems = %problems.join("; "), "MCP result had invalid newt metadata");
            output.invalidate = vec![Vec::new()];
            output.content.push('\n');
            output.content.push_str(BAD_META);
        }
    }
    output
}
