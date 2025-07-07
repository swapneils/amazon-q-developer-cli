use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crossterm::{
    queue,
    style,
};
use eyre::Result;
use serde::{
    Deserialize,
    Serialize,
};
use tokio::sync::RwLock;
use tracing::warn;

use super::InvokeOutput;
use crate::cli::agent::{
    Agent,
    PermissionEvalResult,
};
use crate::cli::chat::CONTINUATION_LINE;
use crate::cli::chat::token_counter::TokenCounter;
use crate::mcp_client::{
    Client as McpClient,
    ClientConfig as McpClientConfig,
    JsonRpcResponse,
    JsonRpcStdioTransport,
    MessageContent,
    Messenger,
    PromptGet,
    ServerCapabilities,
    StdioTransport,
    ToolCallResult,
};
use crate::os::Os;

// TODO: support http transport type
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct CustomToolConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub sampling: bool,
}

pub fn default_timeout() -> u64 {
    120 * 1000
}

#[derive(Debug)]
pub enum CustomToolClient {
    Stdio {
        /// This is the server name as recognized by the model (post sanitized)
        server_name: String,
        client: McpClient<StdioTransport>,
        server_capabilities: RwLock<Option<ServerCapabilities>>,
    },
}

impl CustomToolClient {
    // TODO: add support for http transport
    /// Set the ApiClient for LLM integration in sampling requests
    pub fn set_streaming_client(&self, api_client: std::sync::Arc<crate::api_client::ApiClient>) {
        match self {
            CustomToolClient::Stdio { client, .. } => {
                client.set_streaming_client(api_client);
            }
        }
    }

    pub fn from_config(
        server_name: String,
        config: CustomToolConfig,
        sampling_sender: Option<tokio::sync::mpsc::UnboundedSender<crate::mcp_client::sampling_ipc::PendingSamplingRequest>>,
    ) -> Result<Self> {
        let CustomToolConfig {
            command,
            args,
            env,
            timeout,
            disabled: _,
            sampling,
        } = config;

        // Only pass sampling_sender if sampling is enabled for this server
        let conditional_sampling_sender = if sampling { sampling_sender } else { None };

        let mcp_client_config = McpClientConfig {
            server_name: server_name.clone(),
            bin_path: command.clone(),
            args,
            timeout,
            client_info: serde_json::json!({
               "name": "Q CLI Chat",
               "version": "1.0.0"
            }),
            env,
            sampling_sender: conditional_sampling_sender,
        };
        let client = McpClient::<JsonRpcStdioTransport>::from_config(mcp_client_config)?;
        Ok(CustomToolClient::Stdio {
            server_name,
            client,
            server_capabilities: RwLock::new(None),
        })
    }

    pub async fn init(&self) -> Result<()> {
        match self {
            CustomToolClient::Stdio {
                client,
                server_capabilities,
                ..
            } => {
                if let Some(messenger) = &client.messenger {
                    let _ = messenger.send_init_msg().await;
                }
                // We'll need to first initialize. This is the handshake every client and server
                // needs to do before proceeding to anything else
                let cap = client.init().await?;
                // We'll be scrapping this for background server load: https://github.com/aws/amazon-q-developer-cli/issues/1466
                // So don't worry about the tidiness for now
                server_capabilities.write().await.replace(cap);
                Ok(())
            },
        }
    }

    pub fn assign_messenger(&mut self, messenger: Box<dyn Messenger>) {
        match self {
            CustomToolClient::Stdio { client, .. } => {
                client.messenger = Some(messenger);
            },
        }
    }

    pub fn get_server_name(&self) -> &str {
        match self {
            CustomToolClient::Stdio { server_name, .. } => server_name.as_str(),
        }
    }

    pub async fn request(&self, method: &str, params: Option<serde_json::Value>) -> Result<JsonRpcResponse> {
        match self {
            CustomToolClient::Stdio { client, .. } => Ok(client.request(method, params).await?),
        }
    }

    pub fn list_prompt_gets(&self) -> Arc<std::sync::RwLock<HashMap<String, PromptGet>>> {
        match self {
            CustomToolClient::Stdio { client, .. } => client.prompt_gets.clone(),
        }
    }

    #[allow(dead_code)]
    pub async fn notify(&self, method: &str, params: Option<serde_json::Value>) -> Result<()> {
        match self {
            CustomToolClient::Stdio { client, .. } => Ok(client.notify(method, params).await?),
        }
    }

    pub fn is_prompts_out_of_date(&self) -> bool {
        match self {
            CustomToolClient::Stdio { client, .. } => client.is_prompts_out_of_date.load(Ordering::Relaxed),
        }
    }

    pub fn prompts_updated(&self) {
        match self {
            CustomToolClient::Stdio { client, .. } => client.is_prompts_out_of_date.store(false, Ordering::Relaxed),
        }
    }
}

/// Represents a custom tool that can be invoked through the Model Context Protocol (MCP).
#[derive(Clone, Debug)]
pub struct CustomTool {
    /// Actual tool name as recognized by its MCP server. This differs from the tool names as they
    /// are seen by the model since they are not prefixed by its MCP server name.
    pub name: String,
    /// Reference to the client that manages communication with the tool's server process.
    pub client: Arc<CustomToolClient>,
    /// The method name to call on the tool's server, following the JSON-RPC convention.
    /// This corresponds to a specific functionality provided by the tool.
    pub method: String,
    /// Optional parameters to pass to the tool when invoking the method.
    /// Structured as a JSON value to accommodate various parameter types and structures.
    pub params: Option<serde_json::Value>,
}

impl CustomTool {
    pub async fn invoke(&self, _os: &Os, _updates: impl Write) -> Result<InvokeOutput> {
        // Assuming a response shape as per https://spec.modelcontextprotocol.io/specification/2024-11-05/server/tools/#calling-tools
        let resp = self.client.request(self.method.as_str(), self.params.clone()).await?;
        let result = match resp.result {
            Some(result) => result,
            None => {
                let failure = resp.error.map_or("Unknown error encountered".to_string(), |err| {
                    serde_json::to_string(&err).unwrap_or_default()
                });
                return Err(eyre::eyre!(failure));
            },
        };

        match serde_json::from_value::<ToolCallResult>(result.clone()) {
            Ok(mut de_result) => {
                for content in &mut de_result.content {
                    if let MessageContent::Image { data, .. } = content {
                        *data = format!("Redacted base64 encoded string of an image of size {}", data.len());
                    }
                }
                Ok(InvokeOutput {
                    output: super::OutputKind::Json(serde_json::json!(de_result)),
                })
            },
            Err(e) => {
                warn!("Tool call result deserialization failed: {:?}", e);
                Ok(InvokeOutput {
                    output: super::OutputKind::Json(result.clone()),
                })
            },
        }
    }

    pub fn queue_description(&self, output: &mut impl Write) -> Result<()> {
        queue!(
            output,
            style::Print("Running "),
            style::SetForegroundColor(style::Color::Green),
            style::Print(&self.name),
            style::ResetColor,
        )?;
        if let Some(params) = &self.params {
            let params = match serde_json::to_string_pretty(params) {
                Ok(params) => params
                    .split("\n")
                    .map(|p| format!("{CONTINUATION_LINE} {p}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => format!("{:?}", params),
            };
            queue!(
                output,
                style::Print(" with the param:\n"),
                style::Print(params),
                style::Print("\n"),
                style::ResetColor,
            )?;
        } else {
            queue!(output, style::Print("\n"))?;
        }
        Ok(())
    }

    pub async fn validate(&mut self, _os: &Os) -> Result<()> {
        Ok(())
    }

    pub fn get_input_token_size(&self) -> usize {
        TokenCounter::count_tokens(self.method.as_str())
            + TokenCounter::count_tokens(self.params.as_ref().map_or("", |p| p.as_str().unwrap_or_default()))
    }

    pub fn eval_perm(&self, agent: &Agent) -> PermissionEvalResult {
        use crate::util::MCP_SERVER_TOOL_DELIMITER;
        let Self {
            name: tool_name,
            client,
            ..
        } = self;
        let server_name = client.get_server_name();

        if agent.allowed_tools.contains(&format!("@{server_name}"))
            || agent
                .allowed_tools
                .contains(&format!("@{server_name}{MCP_SERVER_TOOL_DELIMITER}{tool_name}"))
        {
            PermissionEvalResult::Allow
        } else {
            PermissionEvalResult::Ask
        }
    }
}
