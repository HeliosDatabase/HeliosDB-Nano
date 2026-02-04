//! MCP Server implementation
//!
//! Handles JSON-RPC communication over stdio for MCP protocol.

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::EmbeddedDatabase;
use super::protocol::*;
use super::tools::{execute_tool, get_tools};

/// MCP Server for HeliosDB
pub struct McpServer {
    db: Arc<EmbeddedDatabase>,
    initialized: bool,
}

impl McpServer {
    /// Create a new MCP server
    pub fn new(db: Arc<EmbeddedDatabase>) -> Self {
        Self {
            db,
            initialized: false,
        }
    }

    /// Run the MCP server on stdio
    ///
    /// Reads JSON-RPC requests from stdin and writes responses to stdout.
    pub async fn run(&mut self) -> crate::Result<()> {
        info!("Starting HeliosDB MCP server");

        let stdin = std::io::stdin();
        let stdout = Arc::new(Mutex::new(std::io::stdout()));
        let reader = BufReader::new(stdin.lock());

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    error!("Failed to read line: {}", e);
                    continue;
                }
            };

            if line.trim().is_empty() {
                continue;
            }

            debug!("Received: {}", line);

            let response = self.handle_request(&line).await;

            if let Some(resp) = response {
                let json = match serde_json::to_string(&resp) {
                    Ok(j) => j,
                    Err(e) => {
                        error!("Failed to serialize response: {}", e);
                        continue;
                    }
                };

                debug!("Sending: {}", json);

                let mut out = stdout.lock().await;
                if let Err(e) = writeln!(out, "{}", json) {
                    error!("Failed to write response: {}", e);
                }
                if let Err(e) = out.flush() {
                    error!("Failed to flush: {}", e);
                }
            }
        }

        info!("MCP server shutting down");
        Ok(())
    }

    /// Handle a single JSON-RPC request
    async fn handle_request(&mut self, line: &str) -> Option<JsonRpcResponse> {
        let request: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                return Some(JsonRpcResponse::error(
                    None,
                    PARSE_ERROR,
                    format!("Parse error: {}", e),
                ));
            }
        };

        let response = match request.method.as_str() {
            "initialize" => self.handle_initialize(&request).await,
            "initialized" => {
                // Notification, no response needed
                self.initialized = true;
                return None;
            }
            "tools/list" => self.handle_tools_list(&request).await,
            "tools/call" => self.handle_tools_call(&request).await,
            "resources/list" => self.handle_resources_list(&request).await,
            "resources/read" => self.handle_resources_read(&request).await,
            "prompts/list" => self.handle_prompts_list(&request).await,
            "prompts/get" => self.handle_prompts_get(&request).await,
            "ping" => JsonRpcResponse::success(request.id.clone(), serde_json::json!({})),
            _ => JsonRpcResponse::error(
                request.id.clone(),
                METHOD_NOT_FOUND,
                format!("Method not found: {}", request.method),
            ),
        };

        Some(response)
    }

    /// Handle initialize request
    async fn handle_initialize(&self, request: &JsonRpcRequest) -> JsonRpcResponse {
        let result = InitializeResult {
            protocol_version: "2024-11-05".to_string(),
            capabilities: Capabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(false),
                }),
                resources: Some(ResourcesCapability {
                    list_changed: Some(false),
                    subscribe: Some(false),
                }),
                prompts: Some(PromptsCapability {
                    list_changed: Some(false),
                }),
            },
            server_info: ServerInfo {
                name: "heliosdb-lite".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };

        JsonRpcResponse::success(
            request.id.clone(),
            serde_json::to_value(result).unwrap_or_default(),
        )
    }

    /// Handle tools/list request
    async fn handle_tools_list(&self, request: &JsonRpcRequest) -> JsonRpcResponse {
        let tools = get_tools();
        JsonRpcResponse::success(
            request.id.clone(),
            serde_json::json!({ "tools": tools }),
        )
    }

    /// Handle tools/call request
    async fn handle_tools_call(&self, request: &JsonRpcRequest) -> JsonRpcResponse {
        #[derive(serde::Deserialize)]
        struct CallParams {
            name: String,
            #[serde(default)]
            arguments: serde_json::Value,
        }

        let params: CallParams = match serde_json::from_value(request.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return JsonRpcResponse::error(
                    request.id.clone(),
                    INVALID_PARAMS,
                    format!("Invalid params: {}", e),
                );
            }
        };

        let result = execute_tool(self.db.clone(), &params.name, params.arguments).await;

        JsonRpcResponse::success(
            request.id.clone(),
            serde_json::to_value(result).unwrap_or_default(),
        )
    }

    /// Handle resources/list request
    async fn handle_resources_list(&self, request: &JsonRpcRequest) -> JsonRpcResponse {
        // List database schema as resources
        let mut resources = Vec::new();

        // Add database info resource
        resources.push(Resource {
            uri: "heliosdb://schema".to_string(),
            name: "Database Schema".to_string(),
            description: Some("Current database schema with all tables".to_string()),
            mime_type: Some("application/json".to_string()),
        });

        // Add branches resource
        resources.push(Resource {
            uri: "heliosdb://branches".to_string(),
            name: "Branches".to_string(),
            description: Some("List of all database branches".to_string()),
            mime_type: Some("application/json".to_string()),
        });

        JsonRpcResponse::success(
            request.id.clone(),
            serde_json::json!({ "resources": resources }),
        )
    }

    /// Handle resources/read request
    async fn handle_resources_read(&self, request: &JsonRpcRequest) -> JsonRpcResponse {
        #[derive(serde::Deserialize)]
        struct ReadParams {
            uri: String,
        }

        let params: ReadParams = match serde_json::from_value(request.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return JsonRpcResponse::error(
                    request.id.clone(),
                    INVALID_PARAMS,
                    format!("Invalid params: {}", e),
                );
            }
        };

        let content = match params.uri.as_str() {
            "heliosdb://schema" => {
                // Get schema info
                let sql = "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'";
                match self.db.query("main", sql, vec![]) {
                    Ok(result) => {
                        let tables: Vec<_> = result.rows.iter()
                            .filter_map(|row| row.first())
                            .collect();
                        ResourceContent {
                            uri: params.uri,
                            mime_type: Some("application/json".to_string()),
                            text: Some(serde_json::to_string_pretty(&tables).unwrap_or_default()),
                            blob: None,
                        }
                    }
                    Err(e) => {
                        return JsonRpcResponse::error(
                            request.id.clone(),
                            INTERNAL_ERROR,
                            format!("Failed to read schema: {}", e),
                        );
                    }
                }
            }
            "heliosdb://branches" => {
                match self.db.list_branches() {
                    Ok(branches) => ResourceContent {
                        uri: params.uri,
                        mime_type: Some("application/json".to_string()),
                        text: Some(serde_json::to_string_pretty(&branches).unwrap_or_default()),
                        blob: None,
                    },
                    Err(e) => {
                        return JsonRpcResponse::error(
                            request.id.clone(),
                            INTERNAL_ERROR,
                            format!("Failed to read branches: {}", e),
                        );
                    }
                }
            }
            _ => {
                return JsonRpcResponse::error(
                    request.id.clone(),
                    INVALID_PARAMS,
                    format!("Unknown resource: {}", params.uri),
                );
            }
        };

        JsonRpcResponse::success(
            request.id.clone(),
            serde_json::json!({ "contents": [content] }),
        )
    }

    /// Handle prompts/list request
    async fn handle_prompts_list(&self, request: &JsonRpcRequest) -> JsonRpcResponse {
        let prompts = vec![
            Prompt {
                name: "query-builder".to_string(),
                description: Some("Generate SQL queries from natural language".to_string()),
                arguments: Some(vec![
                    PromptArgument {
                        name: "description".to_string(),
                        description: Some("Natural language description of the query".to_string()),
                        required: Some(true),
                    },
                ]),
            },
            Prompt {
                name: "schema-designer".to_string(),
                description: Some("Design database schema from requirements".to_string()),
                arguments: Some(vec![
                    PromptArgument {
                        name: "requirements".to_string(),
                        description: Some("Description of data requirements".to_string()),
                        required: Some(true),
                    },
                ]),
            },
        ];

        JsonRpcResponse::success(
            request.id.clone(),
            serde_json::json!({ "prompts": prompts }),
        )
    }

    /// Handle prompts/get request
    async fn handle_prompts_get(&self, request: &JsonRpcRequest) -> JsonRpcResponse {
        #[derive(serde::Deserialize)]
        struct GetParams {
            name: String,
            #[serde(default)]
            arguments: std::collections::HashMap<String, String>,
        }

        let params: GetParams = match serde_json::from_value(request.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return JsonRpcResponse::error(
                    request.id.clone(),
                    INVALID_PARAMS,
                    format!("Invalid params: {}", e),
                );
            }
        };

        let messages = match params.name.as_str() {
            "query-builder" => {
                let description = params.arguments.get("description").map(|s| s.as_str()).unwrap_or("");
                vec![
                    PromptMessage {
                        role: "user".to_string(),
                        content: PromptContent::Text {
                            text: format!(
                                "You are a SQL expert. Generate a SQL query for HeliosDB based on this description:\n\n{}\n\nThe database is PostgreSQL-compatible with vector search support.",
                                description
                            ),
                        },
                    },
                ]
            }
            "schema-designer" => {
                let requirements = params.arguments.get("requirements").map(|s| s.as_str()).unwrap_or("");
                vec![
                    PromptMessage {
                        role: "user".to_string(),
                        content: PromptContent::Text {
                            text: format!(
                                "You are a database architect. Design a database schema for HeliosDB based on these requirements:\n\n{}\n\nInclude CREATE TABLE statements with appropriate types. HeliosDB supports VECTOR type for embeddings.",
                                requirements
                            ),
                        },
                    },
                ]
            }
            _ => {
                return JsonRpcResponse::error(
                    request.id.clone(),
                    INVALID_PARAMS,
                    format!("Unknown prompt: {}", params.name),
                );
            }
        };

        JsonRpcResponse::success(
            request.id.clone(),
            serde_json::json!({ "messages": messages }),
        )
    }
}
